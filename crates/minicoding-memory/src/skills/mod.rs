//! 技能存储实现（声明式 SKILL.md，`SkillStore` trait 实现在此）。
//!
//! 技能 = `<dir>/skills/<name>/SKILL.md`，其中 `<dir>` 为全局（`MINICODING_HOME`）
//! 与项目（workdir）两级；同名技能项目级覆盖全局。SKILL.md 含 YAML frontmatter
//! （`name`/`description`/`when_to_use`）与 Markdown 指令正文。
//!
//! 与 `long_term.rs` 的 mtime 缓存一致：stat 比对 mtime，未变直接返回缓存（零 IO）；
//! 变化才重读重解析。frontmatter 解析为轻量手写解析器（`---` 定界 + `key: value`），
//! 不引入 YAML 依赖（core/memory 依赖白名单见 `modules.md` §1.4）。

use camino::Utf8PathBuf;
use minicoding_core::skill::{Skill, SkillError, SkillInfo, SkillStore};
use std::sync::Mutex;
use time::OffsetDateTime;

/// 技能目录名。
const SKILL_DIR: &str = "skills";
/// 技能清单文件名。
const SKILL_FILE: &str = "SKILL.md";

/// 单个技能的缓存（内容 + mtime）。
#[derive(Debug)]
struct CachedSkill {
    skill: Skill,
    mtime: Option<OffsetDateTime>,
}

/// 磁盘技能存储：扫描全局 + 项目两级 `skills/` 目录。
pub struct DiskSkillStore {
    global_dir: Utf8PathBuf,
    project_dir: Utf8PathBuf,
    /// 进程内缓存（技能名 → 内容），`std::sync::Mutex` 临界区不跨 await。
    cache: Mutex<std::collections::HashMap<String, CachedSkill>>,
    /// 目录扫描结果缓存（mtime 变化时失效）。
    listing: Mutex<Option<(OffsetDateTime, Vec<SkillInfo>)>>,
}

impl DiskSkillStore {
    /// 创建技能存储（`project_dir` 为工作目录，技能位于其 `.minicoding/skills`）。
    ///
    /// # Errors
    /// 全局目录无法确定时返回 io 错误。
    pub fn new(project_dir: &Utf8PathBuf) -> Result<Self, std::io::Error> {
        let global_dir = minicoding_core::paths::minicoding_home()?.join(SKILL_DIR);
        Ok(Self {
            global_dir,
            project_dir: project_dir.join(".minicoding").join(SKILL_DIR),
            cache: Mutex::new(std::collections::HashMap::new()),
            listing: Mutex::new(None),
        })
    }

    /// 返回两个技能根目录（全局 + 项目）。
    fn root_dirs(&self) -> Vec<Utf8PathBuf> {
        vec![self.global_dir.clone(), self.project_dir.clone()]
    }

    /// 扫描目录收集技能名（项目级覆盖全局）。
    fn scan_dirs(&self) -> Vec<SkillInfo> {
        let mut names: Vec<(String, Utf8PathBuf)> = Vec::new();
        for root in self.root_dirs() {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if path.join(SKILL_FILE).is_file() {
                    // 项目级覆盖全局：后者插入时替换前者（同名取后出现者）
                    names.retain(|(n, _)| n != &name);
                    names.push((name, Utf8PathBuf::from_path_buf(path).unwrap_or_default()));
                }
            }
        }
        names
            .into_iter()
            .map(|(name, dir)| SkillInfo {
                description: peek_description(&dir),
                when_to_use: peek_when_to_use(&dir),
                name,
                source: dir,
            })
            .collect()
    }

    /// 读取单个技能（带缓存）：mtime 未变直接返回缓存。
    fn get_skill_cached(&self, name: &str, dir: &Utf8PathBuf) -> Option<Skill> {
        let Ok(cache) = self.cache.lock() else {
            return None;
        };
        let cached = cache.get(name)?;
        let mtime = cached.mtime?;
        let modified = std::fs::metadata(dir.join(SKILL_FILE))
            .and_then(|m| m.modified())
            .ok()?;
        if OffsetDateTime::from(modified) == mtime {
            Some(cached.skill.clone())
        } else {
            None
        }
    }
}

/// 读 frontmatter 中的 `description`（不重读正文）。
fn peek_description(dir: &Utf8PathBuf) -> String {
    let Ok(raw) = std::fs::read_to_string(dir.join(SKILL_FILE)) else {
        return String::new();
    };
    parse_frontmatter(&raw)
        .ok()
        .and_then(|fm| fm.get("description").cloned())
        .unwrap_or_default()
}

/// 读 frontmatter 中的 `when_to_use`。
fn peek_when_to_use(dir: &Utf8PathBuf) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(SKILL_FILE)).ok()?;
    parse_frontmatter(&raw)
        .ok()
        .and_then(|fm| fm.get("when_to_use").cloned())
}

/// 读取单个技能（带缓存）：mtime 未变直接返回缓存。
fn load_skill(dir: &Utf8PathBuf) -> Result<Option<Skill>, SkillError> {
    let file = dir.join(SKILL_FILE);
    let raw = match std::fs::read_to_string(&file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mtime = std::fs::metadata(&file)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(OffsetDateTime::from);
    let (name, description, when_to_use, instructions) = parse_skill_file(&raw)?;
    Ok(Some(Skill {
        name,
        description,
        when_to_use,
        instructions,
        source: dir.clone(),
        mtime,
    }))
}

impl SkillStore for DiskSkillStore {
    fn list_skills(&self) -> Vec<SkillInfo> {
        // 目录扫描结果缓存（避免每次 prompt 构建都 re-scan）。
        if let Ok(mut listing) = self.listing.lock() {
            if let Some((mtime, cached)) = listing.as_ref()
                && self.root_dirs().iter().all(|d| {
                    std::fs::metadata(d)
                        .and_then(|m| m.modified())
                        .map_or(true, |t| OffsetDateTime::from(t) <= *mtime)
                })
            {
                return cached.clone();
            }
            let skills = self.scan_dirs();
            let now = OffsetDateTime::now_utc();
            *listing = Some((now, skills.clone()));
            skills
        } else {
            self.scan_dirs()
        }
    }

    fn get_skill(&self, name: &str) -> Result<Option<Skill>, SkillError> {
        let dir = {
            let mut dirs = self.scan_dirs();
            dirs.retain(|d| d.name == name);
            dirs.last().map(|d| d.source.clone())
        };
        let Some(dir) = dir else {
            return Ok(None);
        };
        // mtime 缓存检查
        if let Some(skill) = self.get_skill_cached(name, &dir) {
            return Ok(Some(skill));
        }
        let skill = load_skill(&dir)?;
        if let Some(skill) = &skill
            && let Ok(mut cache) = self.cache.lock()
        {
            cache.insert(
                name.to_string(),
                CachedSkill {
                    skill: skill.clone(),
                    mtime: skill.mtime,
                },
            );
        }
        Ok(skill)
    }
}

/// 解析 SKILL.md：返回 `(name, description, when_to_use, instructions)`。
///
/// frontmatter 为文件开头的 `---\n...\n---` 块，`key: value` 行。缺 name 时报错
/// （技能名是唯一标识）；缺 description 时用空串（提示 LLM 不可用）。
fn parse_skill_file(raw: &str) -> Result<(String, String, Option<String>, String), SkillError> {
    let Some((front, body)) = split_frontmatter(raw) else {
        return Err(SkillError::Parse(
            "SKILL.md 缺少 --- frontmatter 块".to_string(),
        ));
    };
    let fields = parse_frontmatter(front)?;
    let name = fields
        .get("name")
        .cloned()
        .ok_or_else(|| SkillError::Parse("frontmatter 缺少 name".to_string()))?;
    let description = fields.get("description").cloned().unwrap_or_default();
    let when_to_use = fields.get("when_to_use").cloned();
    Ok((name, description, when_to_use, body.trim().to_string()))
}

/// 切分 frontmatter：`---\n...\n---` 块与正文。返回 `(frontmatter, body)`。
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some((&rest[..end], &rest[end + 4..]))
}

/// 解析 frontmatter 的 `key: value` 行（极简 YAML 子集，单行标量）。
fn parse_frontmatter(front: &str) -> Result<std::collections::HashMap<String, String>, SkillError> {
    let mut map = std::collections::HashMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(SkillError::Parse(format!("frontmatter 行无法解析: {line}")));
        };
        map.insert(
            k.trim().to_string(),
            v.trim().trim_matches('"').trim_matches('\'').to_string(),
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_skill(root: &std::path::Path, name: &str, content: &str) {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parse_frontmatter_basic() {
        let fm = "name: git-repo-to-book\ndescription: 把仓库写成书\nwhen_to_use: 用户要写书\n";
        let map = parse_frontmatter(fm).unwrap();
        assert_eq!(map["name"], "git-repo-to-book");
        assert_eq!(map["description"], "把仓库写成书");
        assert_eq!(map["when_to_use"], "用户要写书");
    }

    #[test]
    fn parse_skill_file_ok() {
        let raw = "---\nname: test\n---\n# 正文\n步骤一\n";
        let (name, desc, when, body) = parse_skill_file(raw).unwrap();
        assert_eq!(name, "test");
        assert_eq!(desc, "");
        assert!(when.is_none());
        assert_eq!(body, "# 正文\n步骤一");
    }

    #[test]
    fn parse_skill_file_missing_name() {
        let raw = "---\ndescription: x\n---\nbody";
        assert!(parse_skill_file(raw).is_err());
    }

    #[test]
    fn disk_store_list_and_get() {
        let tmp = tempdir().unwrap();
        write_skill(
            tmp.path(),
            "book",
            "---\nname: book\ndescription: 写书\n---\n# 书\n正文",
        );
        let store = DiskSkillStore {
            global_dir: Utf8PathBuf::from_path_buf(tmp.path().join("skills")).unwrap(),
            project_dir: Utf8PathBuf::from_path_buf(
                tmp.path().join("proj").join(".minicoding").join("skills"),
            )
            .unwrap(),
            cache: Mutex::new(std::collections::HashMap::new()),
            listing: Mutex::new(None),
        };
        let list = store.list_skills();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "book");
        let skill = store.get_skill("book").unwrap().unwrap();
        assert_eq!(skill.name, "book");
        assert_eq!(skill.description, "写书");
        assert!(skill.instructions.contains("正文"));
        assert!(store.get_skill("nope").unwrap().is_none());
    }

    #[test]
    fn project_overrides_global() {
        let tmp = tempdir().unwrap();
        // 全局
        write_skill(
            tmp.path(),
            "dup",
            "---\nname: dup\ndescription: global\n---\ng",
        );
        // 项目（workdir 的 .minicoding/skills）
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join(".minicoding/skills/dup")).unwrap();
        std::fs::write(
            proj.join(".minicoding/skills/dup/SKILL.md"),
            "---\nname: dup\ndescription: project\n---\np",
        )
        .unwrap();
        let store = DiskSkillStore {
            global_dir: Utf8PathBuf::from_path_buf(tmp.path().join("skills")).unwrap(),
            project_dir: Utf8PathBuf::from_path_buf(proj.join(".minicoding/skills")).unwrap(),
            cache: Mutex::new(std::collections::HashMap::new()),
            listing: Mutex::new(None),
        };
        let skill = store.get_skill("dup").unwrap().unwrap();
        assert_eq!(skill.description, "project", "项目级应覆盖全局");
    }

    #[test]
    fn get_missing_dir_returns_none() {
        let store = DiskSkillStore {
            global_dir: Utf8PathBuf::from("/nonexistent-global"),
            project_dir: Utf8PathBuf::from("/nonexistent-project"),
            cache: Mutex::new(std::collections::HashMap::new()),
            listing: Mutex::new(None),
        };
        assert!(store.list_skills().is_empty());
        assert!(store.get_skill("x").unwrap().is_none());
    }
}

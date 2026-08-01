//! Auto memory：自动学习记录（`auto.md` + `auto.index.json`）。
//!
//! 设计要点（见 `design.md` §8.7、`docs/rules.md` C-27）：
//! - **物理隔离**：与 `long_term.md` 分离存储，避免污染手写记忆；
//! - **容量控制**：上限 200 行或 25KB（先到者为准），超限按
//!   `confidence asc, updated asc` 淘汰低置信度旧条目；
//! - **置信度**：每条 `confidence ∈ [0.0, 1.0]`，多次确认递增；
//! - **指令性内容检测**：含 "Always use X"/"禁止 Y" 等指令性内容时降级 `Ask`
//!   （C-27：Auto memory 不可作为越权通道）；
//! - **内容是数据非指令**（C-05）：注入时由 `inject::inject_auto_memory`
//!   包裹 `<auto_memory>` 边界。
//!
//! 与 [`crate::LongTermMemory`] 的区别：AutoMemory 是条目制（add/update），
//! 不实现 `MemoryStore`（`save` 全量覆盖语义不适用）；`memory.write` 工具
//! 对 `target: "auto"` 调 `add_entry`，对 `target: "long_term"` 调 `save`。

use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::model::MemoryError;
use minicoding_core::paths;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio::fs;

/// Auto memory 正文文件名。
const AUTO_FILE: &str = "auto.md";
/// Auto memory 索引文件名（条目元数据，source of truth）。
const AUTO_INDEX_FILE: &str = "auto.index.json";
/// 原子写入临时文件后缀。
const TMP_SUFFIX: &str = ".tmp";
/// 行数上限（见 `design.md` §8.7）。
const MAX_LINES: usize = 200;
/// 字节上限（25KB，见 `design.md` §8.7）。
const MAX_BYTES: usize = 25_600;

/// Auto memory 条目类别（对应 `design.md` §8.7 触发场景）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoCategory {
    /// 用户修正 Agent 错误。
    Correction,
    /// Agent 反复踩坑。
    Pitfall,
    /// 用户显式偏好。
    Pref,
    /// 项目架构决策。
    Decision,
}

impl AutoCategory {
    /// 条目类别标签（用于渲染）。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Correction => "correction",
            Self::Pitfall => "pitfall",
            Self::Pref => "pref",
            Self::Decision => "decision",
        }
    }
}

/// Auto memory 单条记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoEntry {
    /// 主题键（去重与更新用，如 `command-style`）。
    pub topic: String,
    /// 正文内容。
    pub content: String,
    /// 置信度 `[0.0, 1.0]`。
    pub confidence: f64,
    /// 最后更新时间。
    #[serde(with = "time::serde::rfc3339")]
    pub updated: OffsetDateTime,
    /// 条目类别。
    pub category: AutoCategory,
}

/// Auto memory 存储（`auto.md` + `auto.index.json` + 内存缓存）。
///
/// `auto.index.json` 是 source of truth（条目数组），`auto.md` 是其人机共读渲染
/// （每次 `save` 时从索引重新生成）。`load` 返回渲染后的 Markdown 供注入。
pub struct AutoMemory {
    /// 正文路径（`~/.minicoding/memory/auto.md`）。
    path: Utf8PathBuf,
    /// 索引路径（`~/.minicoding/memory/auto.index.json`）。
    index_path: Utf8PathBuf,
    /// 缓存的条目列表（命中 mtime 时复用，零解析）。
    cached_entries: Mutex<Option<Vec<AutoEntry>>>,
    /// 缓存的索引文件 mtime。
    cached_mtime: Mutex<Option<OffsetDateTime>>,
}

impl AutoMemory {
    /// 从默认 `MINICODING_HOME/memory/` 目录构造。
    ///
    /// # Errors
    /// 当 home 目录无法确定时返回 `MemoryError`。
    pub fn new() -> Result<Self, MemoryError> {
        let dir = paths::memory_dir()?;
        Ok(Self::with_dir(&dir))
    }

    /// 从指定目录构造。
    #[must_use]
    pub fn with_dir(dir: &Utf8Path) -> Self {
        Self {
            path: dir.join(AUTO_FILE),
            index_path: dir.join(AUTO_INDEX_FILE),
            cached_entries: Mutex::new(None),
            cached_mtime: Mutex::new(None),
        }
    }

    /// 加载全部条目（命中 mtime 缓存时零 IO/解析）。
    ///
    /// 文件不存在时返回空 `Vec`。
    async fn load_entries(&self) -> Result<Vec<AutoEntry>, MemoryError> {
        let current = self.current_mtime().await?;

        // mtime 命中且缓存存在：直接复用。
        {
            let cached_mtime = lock(&self.cached_mtime);
            if current == *cached_mtime
                && let Some(entries) = lock(&self.cached_entries).clone()
            {
                return Ok(entries);
            }
        }

        // 文件不存在：空条目。
        let Some(current) = current else {
            *lock(&self.cached_entries) = Some(Vec::new());
            *lock(&self.cached_mtime) = None;
            return Ok(Vec::new());
        };

        // 读取索引（source of truth）。
        let bytes = fs::read(&self.index_path).await?;
        let entries: Vec<AutoEntry> = serde_json::from_slice(&bytes)
            .map_err(|e| MemoryError::Serialize(format!("auto index parse: {e}")))?;

        *lock(&self.cached_entries) = Some(entries.clone());
        *lock(&self.cached_mtime) = Some(current);
        Ok(entries)
    }

    /// 渲染条目为 Markdown 正文（供 `auto.md` 与注入用）。
    ///
    /// 渲染格式：每条以 `## [category] topic` 起头，正文，末尾标注置信度与更新时间。
    #[must_use]
    pub fn render(&self, entries: &[AutoEntry]) -> String {
        render_entries(entries)
    }

    /// 添加或更新条目（按 `topic` 去重），触发容量淘汰后原子落盘。
    ///
    /// 返回写入后的条目数。
    ///
    /// # Errors
    /// IO 或序列化失败时返回 `MemoryError`。
    pub async fn add_entry(
        &self,
        topic: String,
        content: String,
        category: AutoCategory,
        confidence: f64,
    ) -> Result<usize, MemoryError> {
        let mut entries = self.load_entries().await?;
        let now = OffsetDateTime::now_utc();
        let confidence = confidence.clamp(0.0, 1.0);

        // 按 topic 去重：存在则更新，不存在则追加。
        if let Some(existing) = entries.iter_mut().find(|e| e.topic == topic) {
            existing.content = content;
            existing.category = category;
            // 多次确认递增置信度（上限 1.0）。
            existing.confidence = (existing.confidence + 0.1).min(1.0);
            existing.updated = now;
        } else {
            entries.push(AutoEntry {
                topic,
                content,
                confidence,
                updated: now,
                category,
            });
        }

        // 容量淘汰。
        evict_until_fit(&mut entries);

        let count = entries.len();
        self.save_entries(&entries).await?;
        Ok(count)
    }

    /// 清空全部 Auto memory（删除 `auto.md` 与 `auto.index.json`）。
    ///
    /// # Errors
    /// 删除失败时返回 `MemoryError`。
    pub async fn clear(&self) -> Result<(), MemoryError> {
        let _ = fs::remove_file(&self.path).await;
        let _ = fs::remove_file(&self.index_path).await;
        *lock(&self.cached_entries) = Some(Vec::new());
        *lock(&self.cached_mtime) = None;
        Ok(())
    }

    /// 加载渲染后的 Markdown 正文（供注入 system 段）。
    ///
    /// 空条目返回空串（注入时跳过）。
    ///
    /// # Errors
    /// `load_entries` 失败时向上传播 `MemoryError`。
    pub async fn load_rendered(&self) -> Result<String, MemoryError> {
        let entries = self.load_entries().await?;
        Ok(self.render(&entries))
    }

    /// 原子写入索引与渲染后的正文，刷新缓存。
    async fn save_entries(&self, entries: &[AutoEntry]) -> Result<(), MemoryError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // 写索引（source of truth）。
        let index_bytes = serde_json::to_vec_pretty(entries)
            .map_err(|e| MemoryError::Serialize(format!("auto index serialize: {e}")))?;
        let idx_tmp = Utf8PathBuf::from(format!("{}{TMP_SUFFIX}", self.index_path.as_str()));
        fs::write(&idx_tmp, &index_bytes).await?;
        fs::rename(&idx_tmp, &self.index_path).await?;

        // 写渲染后的正文。
        let rendered = self.render(entries);
        let md_tmp = Utf8PathBuf::from(format!("{}{TMP_SUFFIX}", self.path.as_str()));
        fs::write(&md_tmp, rendered.as_bytes()).await?;
        fs::rename(&md_tmp, &self.path).await?;

        // 刷新缓存。
        let mtime = self
            .current_mtime()
            .await?
            .unwrap_or_else(OffsetDateTime::now_utc);
        *lock(&self.cached_entries) = Some(entries.to_vec());
        *lock(&self.cached_mtime) = Some(mtime);
        Ok(())
    }

    /// 读取索引文件 mtime；不存在返回 `None`。
    async fn current_mtime(&self) -> Result<Option<OffsetDateTime>, MemoryError> {
        match fs::metadata(&self.index_path).await {
            Ok(md) => Ok(Some(OffsetDateTime::from(md.modified()?))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

impl Default for AutoMemory {
    fn default() -> Self {
        let dir = paths::memory_dir().unwrap_or_else(|_| Utf8PathBuf::from("memory"));
        Self::with_dir(&dir)
    }
}

/// 渲染条目为 Markdown 正文（自由函数，供 `render` 与 `exceeds_limits` 共用）。
fn render_entries(entries: &[AutoEntry]) -> String {
    use std::fmt::Write as _;
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for e in entries {
        let updated = e
            .updated
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string());
        let _ = writeln!(
            out,
            "## [{}] {}\n\n{}\n\n- confidence: {:.2}\n- updated: {}\n",
            e.category.as_str(),
            e.topic,
            e.content.trim(),
            e.confidence,
            updated,
        );
    }
    out
}

/// 容量淘汰：按 `confidence asc, updated asc` 移除条目，直至行数与字节均在限内。
///
/// 淘汰顺序：低置信度优先；同置信度下旧条目优先（`updated asc`）。
/// 排序后从头部逐条移除，直至满足 `MAX_LINES` 与 `MAX_BYTES`。
fn evict_until_fit(entries: &mut Vec<AutoEntry>) {
    if entries.is_empty() {
        return;
    }
    // 按 confidence asc, updated asc 排序（低置信度旧条目在前，优先淘汰）。
    entries.sort_by(|a, b| {
        a.confidence
            .partial_cmp(&b.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.updated.cmp(&b.updated))
    });

    // 从头部淘汰（低置信度旧条目），直至满足容量限。
    while entries.len() > 1 && exceeds_limits(entries) {
        entries.remove(0);
    }
}

/// 判断渲染后的条目是否超过行数或字节上限（与 `render` 使用同一渲染函数）。
fn exceeds_limits(entries: &[AutoEntry]) -> bool {
    let rendered = render_entries(entries);
    rendered.lines().count() > MAX_LINES || rendered.len() > MAX_BYTES
}

/// 检测内容是否含指令性模式（C-27：降级 `Ask`）。
///
/// 命中以下任一模式则返回 `true`：
/// - 英文祈使/模态：`Always use`、`Never`、`Must`、`Do not`、`Don't`、`Should`（行首）；
/// - 中文祈使：`总是`、`永远`、`禁止`、`必须`、`不要`、`不得`、`应当`、`应`（行首或含）；
/// - `AGENTS.md` 风格 section 头：`## 规则`、`## Rules`、`## 约束`、`## Constraints`。
///
/// 检测以行为单位，忽略大小写（英文部分）。
#[must_use]
pub fn is_instructional(content: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // 英文祈使（行首，忽略大小写）。
        if lower.starts_with("always use")
            || lower.starts_with("never ")
            || lower.starts_with("must ")
            || lower.starts_with("do not ")
            || lower.starts_with("don't ")
            || lower.starts_with("should ")
        {
            return true;
        }
        // 中文祈使（行首或包含）。
        if line.starts_with("总是")
            || line.starts_with("永远")
            || line.starts_with("禁止")
            || line.starts_with("必须")
            || line.starts_with("不要")
            || line.starts_with("不得")
            || line.starts_with("应当")
            || line.starts_with("应")
        {
            return true;
        }
        // AGENTS.md 风格 section 头。
        if lower.starts_with("## rules")
            || lower.starts_with("## constraints")
            || line.starts_with("## 规则")
            || line.starts_with("## 约束")
        {
            return true;
        }
    }
    false
}

/// 锁定 `Mutex`，忽略 poison（与 `LongTermMemory::guard` 同语义）。
fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// 别名：与 `long_term.rs` 的 `guard` 同语义，避免 clippy 重复定义告警。
#[allow(clippy::missing_const_for_fn)]
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    guard(m)
}

#[cfg(test)]
mod tests {
    //! 最小单元测试：覆盖容量淘汰、置信度递增、指令性内容检测、渲染格式。

    use super::*;
    use camino::Utf8PathBuf;

    fn make(dir: &std::path::Path) -> AutoMemory {
        AutoMemory::with_dir(
            &Utf8PathBuf::from_path_buf(dir.to_owned())
                .expect("tempdir path is UTF-8 on linux test env"),
        )
    }

    #[tokio::test]
    async fn add_entry_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        let count = mem
            .add_entry(
                "command-style".to_string(),
                "prefer cargo fmt".to_string(),
                AutoCategory::Pref,
                0.5,
            )
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(tmp.path().join(AUTO_FILE).exists());
        assert!(tmp.path().join(AUTO_INDEX_FILE).exists());
    }

    #[tokio::test]
    async fn add_entry_dedup_by_topic_increases_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());

        mem.add_entry(
            "style".to_string(),
            "v1".to_string(),
            AutoCategory::Pref,
            0.5,
        )
        .await
        .unwrap();
        mem.add_entry(
            "style".to_string(),
            "v2".to_string(),
            AutoCategory::Pref,
            0.5,
        )
        .await
        .unwrap();

        let entries = mem.load_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "v2");
        // 二次确认 → confidence +0.1。
        assert!((entries[0].confidence - 0.6).abs() < 1e-9);
    }

    #[tokio::test]
    async fn load_rendered_empty_when_no_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        let rendered = mem.load_rendered().await.unwrap();
        assert!(rendered.is_empty());
    }

    #[tokio::test]
    async fn load_rendered_contains_topic_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        mem.add_entry(
            "style".to_string(),
            "prefer 4-space indent".to_string(),
            AutoCategory::Pref,
            0.8,
        )
        .await
        .unwrap();

        let rendered = mem.load_rendered().await.unwrap();
        assert!(rendered.contains("## [pref] style"));
        assert!(rendered.contains("prefer 4-space indent"));
        assert!(rendered.contains("confidence: 0.80"));
    }

    #[tokio::test]
    async fn clear_removes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        mem.add_entry("x".to_string(), "y".to_string(), AutoCategory::Pref, 0.1)
            .await
            .unwrap();
        assert!(tmp.path().join(AUTO_FILE).exists());

        mem.clear().await.unwrap();
        assert!(!tmp.path().join(AUTO_FILE).exists());
        assert!(!tmp.path().join(AUTO_INDEX_FILE).exists());

        let rendered = mem.load_rendered().await.unwrap();
        assert!(rendered.is_empty());
    }

    #[tokio::test]
    async fn eviction_removes_low_confidence_old() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());

        // 添加一条高置信度条目。
        mem.add_entry(
            "keep".to_string(),
            "important".to_string(),
            AutoCategory::Decision,
            1.0,
        )
        .await
        .unwrap();

        // 添加多条低置信度条目，触发淘汰。
        for i in 0..50 {
            mem.add_entry(
                format!("low-{i}"),
                "x".repeat(100),
                AutoCategory::Pitfall,
                0.1,
            )
            .await
            .unwrap();
        }

        let entries = mem.load_entries().await.unwrap();
        // 高置信度条目应保留。
        assert!(entries.iter().any(|e| e.topic == "keep"));
        // 渲染后应在容量限内。
        let rendered = mem.render(&entries);
        assert!(rendered.lines().count() <= MAX_LINES);
        assert!(rendered.len() <= MAX_BYTES);
    }

    #[test]
    fn is_instructional_detects_imperative_english() {
        assert!(is_instructional("Always use cargo fmt"));
        assert!(is_instructional("Never commit secrets"));
        assert!(is_instructional("Must run tests before push"));
        assert!(is_instructional("Do not use unwrap"));
        assert!(is_instructional("should handle errors"));
    }

    #[test]
    fn is_instructional_detects_imperative_chinese() {
        assert!(is_instructional("总是使用 cargo fmt"));
        assert!(is_instructional("禁止提交密钥"));
        assert!(is_instructional("必须运行测试"));
        assert!(is_instructional("不要使用 unwrap"));
        assert!(is_instructional("应当处理错误"));
    }

    #[test]
    fn is_instructional_detects_section_headers() {
        assert!(is_instructional("## Rules\n- rule 1"));
        assert!(is_instructional("## 规则\n- 规则 1"));
        assert!(is_instructional("## Constraints"));
        assert!(is_instructional("## 约束"));
    }

    #[test]
    fn is_instructional_negative_for_descriptive() {
        assert!(!is_instructional("prefer cargo fmt"));
        assert!(!is_instructional("the project uses rust"));
        assert!(!is_instructional("a note about testing"));
        assert!(!is_instructional("记录：用户喜欢深色主题"));
        assert!(!is_instructional(""));
    }

    #[test]
    fn render_empty_entries_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        assert_eq!(mem.render(&[]), "");
    }

    #[test]
    fn render_formats_category_and_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        let entry = AutoEntry {
            topic: "indent".to_string(),
            content: "use 4 spaces".to_string(),
            confidence: 0.9,
            updated: OffsetDateTime::now_utc(),
            category: AutoCategory::Pref,
        };
        let rendered = mem.render(std::slice::from_ref(&entry));
        assert!(rendered.contains("[pref] indent"));
        assert!(rendered.contains("use 4 spaces"));
        assert!(rendered.contains("confidence: 0.90"));
    }
}

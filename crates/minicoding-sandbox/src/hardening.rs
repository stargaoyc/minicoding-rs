//! 进程硬化与 VCS 目录保护（T-M4-3）。
//!
//! ## 进程硬化
//!
//! `harden_process()` 在 minicoding 启动时（main 早期）调用，降低自身进程被
//! 攻击的风险：
//! - `PR_SET_DUMPABLE = 0`：禁止 core dump 与 ptrace 附着（防凭证从内存转储）；
//! - `RLIMIT_CORE = 0`：禁用 core dump 文件生成；
//! - 清除 `LD_*` 环境变量：防动态链接器注入（C-04）。
//!
//! 仅 Linux 实现（`libc` 调用）；其他平台 no-op + warn。
//!
//! ## VCS 目录保护
//!
//! `vcs_protected_dirs()` 返回 workdir 下的 `.git`/`.hg`/`.svn` 目录列表，供
//! landlock 只读规则与 policy builtin 黑名单使用（防破坏版本库元数据，C-22）。
//!
//! 详见 `security.md` §8、`design.md` C-22。

use std::path::{Path, PathBuf};

/// 进程硬化：在 minicoding 启动时调用，降低自身被攻击风险。
///
/// Linux：`PR_SET_DUMPABLE=0` + `RLIMIT_CORE=0` + 清 `LD_*`。
/// 其他平台：no-op + warn（M4 仅 Linux，平台优先级 M5+/M6+ 补齐）。
///
/// # Errors
/// 仅在 `setrlimit`/`prctl` 系统调用失败时返回 `Err`（极少见，通常意味着内核
/// 拒绝；best effort 可忽略继续启动）。
pub fn harden_process() -> Result<(), std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        harden_linux()?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            platform = std::env::consts::OS,
            "进程硬化在当前平台为 no-op（M4 仅 Linux）"
        );
    }
    Ok(())
}

/// Linux 进程硬化实现。
#[cfg(target_os = "linux")]
fn harden_linux() -> Result<(), std::io::Error> {
    // 1. PR_SET_DUMPABLE = 0：禁止 ptrace 附着与 core dump（防凭证转储，C-04）
    // SAFETY: prctl(PR_SET_DUMPABLE, 0) 是简单的标志位设置，无内存安全风险；
    // 返回值 < 0 表示失败，通过 errno 转 io::Error。
    unsafe {
        let rc = libc::prctl(libc::PR_SET_DUMPABLE, 0);
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    // 2. RLIMIT_CORE = 0：禁用 core dump 文件生成
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit(RLIMIT_CORE, ptr) 传入栈上 rlimit 结构体指针，
    // 内核读取后返回，不持有指针；结构体为 POD 无内存安全风险。
    unsafe {
        let rc = libc::setrlimit(libc::RLIMIT_CORE, std::ptr::addr_of!(rlim));
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    // 3. 清除 LD_* 环境变量：防动态链接器注入（C-04）
    // 保留 keys 列表后逐个 remove（不持有 env 内部指针跨调用）
    let ld_keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("LD_"))
        .map(|(k, _)| k)
        .collect();
    for k in ld_keys {
        // SAFETY: `remove_var` 在 Rust 2024 标记为 unsafe 是因为多线程下修改环境
        // 非线程安全。此处仅在 minicoding 启动早期（main 单线程阶段）调用，且
        // 清除 `LD_*` 是一次性操作，不与并发读 env 的代码交错。
        unsafe {
            std::env::remove_var(&k);
        }
    }

    tracing::debug!("进程硬化完成：PR_SET_DUMPABLE=0, RLIMIT_CORE=0, LD_* 已清除");
    Ok(())
}

/// 返回 workdir 下应受写保护的 VCS 目录（`.git`/`.hg`/`.svn`）。
///
/// 仅返回实际存在的条目（避免 `landlock` `PathFd` 打开不存在路径报错）。
/// SEC-13（2026-08-25 R2 审查）：`.git` 允许是**文件**（worktree/submodule 的
/// gitdir 指针形式），`is_dir()` 过滤会漏掉这类形态导致 `ReadOnly` 场景失去
/// 内核级 VCS 保护——改为 `exists()`，landlock 的 path-beneath 规则与 Seatbelt
/// 的 `subpath` deny 对文件同样生效。
/// 供 landlock 只读规则（workdir 只读场景）与 policy builtin 黑名单（workdir
/// 可写场景）使用（C-22 VCS 保护）。
#[must_use]
pub fn vcs_protected_dirs(workdir: &Path) -> Vec<PathBuf> {
    const VCS_NAMES: &[&str] = &[".git", ".hg", ".svn"];
    VCS_NAMES
        .iter()
        .map(|name| workdir.join(name))
        .filter(|p| p.exists())
        .collect()
}

/// env 变量优先；未设置时回退 `$HOME`/`default_rel`（`home` 为 `None` 时无默认）。
// 仅 linux/macos 的 sandbox 驱动消费（windows 用 Job Object，无 HOME 白名单），
// 非 linux/macos 平台标注 allow 防 dead_code。
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn env_or_home(env_key: &str, default_rel: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Ok(v) = std::env::var(env_key)
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    home.map(|h| h.join(default_rel))
}

/// HOME 下的细粒度只读白名单（A3；Linux landlock RO 规则与 macOS Seatbelt 共用，
/// R5 SEC-4 修复后跨平台）。
///
/// 覆盖工具链与缓存的常见落点，环境变量优先于 `$HOME/<默认名>`：
/// `$CARGO_HOME`||`~/.cargo`、`$RUSTUP_HOME`||`~/.rustup`、`~/.config`、
/// `~/.cache`、`~/.local`、`$NVM_DIR`||`~/.nvm`、`$VOLTA_HOME`||`~/.volta`、
/// `~/.npm`、`~/go`、`$GOPATH`。
///
/// 基于存在性过滤（不存在的条目跳过）并去重。**刻意不含 `$HOME` 本身**：凭证目录
/// （`.ssh`/`.aws`/`.gnupg`）不可读，这是 A3 对"HOME 整体只读放行"旧语义的收敛
/// （见 `linux.rs` 的 `build_ruleset` 与 `macos.rs` 的 `build_profile`）。
///
/// SEC-11（2026-08-27 R5 审查，如实记录）：白名单内 `~/.config`（gh/gcloud 凭证
/// 落点）与 `~/.cargo`（crates.io 令牌 `credentials` 落点）属凭证通道——macOS
/// 侧以 profile 尾部显式 deny 覆盖（`credential_dir_deny_paths`），Linux 侧经
/// [`home_read_allow_paths_without_credentials`] 展开白名单排除（SEC-R6-2，
/// 2026-08-28 R6 修复；此前注释声称"依赖 ABI 5+ deny 规则"，但 landlock crate
/// 0.4.x 不支持 deny 规则、`linux.rs` 亦未添加，属声明-实现裂缝）。
// 仅 linux/macos 的 sandbox 驱动消费（windows 用 Job Object），非 linux/macos
// 平台标注 allow 防 dead_code（与 credential_dir_deny_paths 同模式）。
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[must_use]
pub fn home_read_allow_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);

    let candidates = [
        env_or_home("CARGO_HOME", ".cargo", home.as_deref()),
        env_or_home("RUSTUP_HOME", ".rustup", home.as_deref()),
        home.as_deref().map(|h| h.join(".config")),
        home.as_deref().map(|h| h.join(".cache")),
        home.as_deref().map(|h| h.join(".local")),
        env_or_home("NVM_DIR", ".nvm", home.as_deref()),
        env_or_home("VOLTA_HOME", ".volta", home.as_deref()),
        home.as_deref().map(|h| h.join(".npm")),
        home.as_deref().map(|h| h.join("go")),
        std::env::var("GOPATH")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
    ];

    let mut allow: Vec<PathBuf> = Vec::with_capacity(candidates.len());
    for c in candidates.into_iter().flatten() {
        if c.exists() && !allow.contains(&c) {
            allow.push(c);
        }
    }
    allow
}

/// 白名单内的凭证高危落点（SEC-11）：对已允许的 `~/.config`/`~/.cargo` 子树
/// 内的活凭证文件/目录做尾部显式 deny（Seatbelt 最后匹配规则优先，
/// Linux landlock 侧经 [`home_read_allow_paths_without_credentials`] 展开排除）。
///
/// SEC-R7-1（2026-08-28 R7 审查）：`~/.config` 整体在 allow 白名单（Linux 展开
/// 后保留 `gh`/`gcloud` 之外的其余子项）——R6 只排除了 `gh`/`gcloud`，但
/// `github-copilot`（Copilot OAuth token，`hosts.json`）、`git/credentials`、
/// `docker`（registry auth）、`uv`/`pypoetry`（Python 包索引 token）、`aws`
/// 均为同一"仓库即边界"凭证通道。补全 deny 列表后 Linux 展开与 macOS 尾部
/// deny 自动覆盖。
///
/// 仅返回实际存在的条目（避免 profile 内引用不存在路径）。
// 仅 macOS Seatbelt profile 消费（Linux 侧用 `home_read_allow_paths_without_credentials`
// 展开白名单，非 mac 平台标注 allow 防 dead_code——mac 编译时正常使用）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[must_use]
pub fn credential_dir_deny_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let Some(home) = home else {
        return Vec::new();
    };
    let candidates = [
        // 顶层凭证目录（不在 allow 白名单内，防御性双保险）
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".gnupg"),
        home.join(".docker"),
        // `~/.config` 子树内的活凭证落点（SEC-11 + SEC-R7-1）
        home.join(".config/gh"),
        home.join(".config/gcloud"),
        home.join(".config/github-copilot"),
        home.join(".config/git/credentials"),
        home.join(".config/docker"),
        home.join(".config/uv"),
        home.join(".config/pypoetry"),
        home.join(".config/aws"),
        // `~/.cargo` 子树内的 crates.io 令牌
        home.join(".cargo/credentials"),
    ];
    candidates.into_iter().filter(|p| p.exists()).collect()
}

/// Linux landlock 侧的 HOME 读白名单（SEC-R6-2，2026-08-28 R6 审查）。
///
/// 用途：`home_read_allow_paths` 供 macOS Seatbelt（可叠加尾部 deny 规则）与
/// 通用场景；landlock `path_beneath` allow 规则**覆盖其下全部子路径**，且
/// landlock crate 0.4.x 不支持 ABI5+ 的 deny 规则——`~/.config` 的 allow 会连带
/// 放行 `~/.config/gh` 等凭证落点。因此 Linux 侧把含凭证子路径的顶层目录
/// 展开为其**安全直接子项**（不含 `credential_dir_deny_paths` 中的凭证路径），
/// 逐级下钻直到不再覆盖任何 deny 路径。
///
/// 展开失败（目录不可读）时该目录整体不放行（fail-closed，凭证优先）。
#[cfg(target_os = "linux")]
#[must_use]
pub fn home_read_allow_paths_without_credentials() -> Vec<PathBuf> {
    subtract_denied(home_read_allow_paths(), &credential_dir_deny_paths())
}

/// 从 allow 集合中剔除覆盖 `denied` 中任一凭证路径的条目。
///
/// 算法：对每个 allow 路径，若其下无任何 deny 路径 → 原样保留；若存在 → 展开
/// 为直接子项，跳过等于 deny 路径的子项、递归下钻仍覆盖 deny 的子项。结果不含
/// 任何覆盖凭证路径的规则。
// 仅 Linux 的 `home_read_allow_paths_without_credentials` 消费（macOS 用 Seatbelt
// 尾部 deny，不展开）——非 Linux 平台标注 allow 防 dead_code（CI 跨平台 -D warnings）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn subtract_denied(allowed: Vec<PathBuf>, denied: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in allowed {
        let under: Vec<&PathBuf> = denied.iter().filter(|d| d.starts_with(&a)).collect();
        if under.is_empty() {
            out.push(a);
            continue;
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(&a)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        children.sort();
        for c in children {
            if under.iter().any(|d| d.as_path() == c.as_path()) {
                // 子项本身是凭证路径 → 整体跳过
                continue;
            }
            if under.iter().any(|d| d.starts_with(&c)) {
                // 凭证路径在该子项之下 → 递归下钻
                out.extend(subtract_denied(vec![c], denied));
            } else {
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use tempfile::TempDir;

    /// 环境变量进程级全局——所有改 env 的测试共享此锁（函数内 static 各自
    /// 为锁不互斥，并行测试会交叉污染，集成期修复）。仅 linux 测试使用
    /// （home_read_allow_paths 为 linux cfg），非 linux target 下不编译。
    #[cfg(target_os = "linux")]
    static ENV_SERIAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn subtract_denied_excludes_credential_subtrees() {
        // SEC-R6-2（2026-08-28 R6 审查）：landlock path_beneath 的 allow 覆盖
        // 子路径——含凭证落点的顶层目录必须展开为安全子项而非原样放行。
        // SEC-R7-1（2026-08-28 R7 审查）：扩展覆盖 github-copilot/docker 等
        // `~/.config` 下的补充凭证落点。
        let tmp = TempDir::new().unwrap();
        // 顶层目录 ~/.config，含安全子项与凭证子项
        let config = tmp.path().join(".config");
        for sub in ["git", "gh", "gcloud", "github-copilot", "docker", "fish"] {
            std::fs::create_dir_all(config.join(sub)).unwrap();
        }
        // `~/.config/git/credentials`（嵌套凭证文件，须递归下钻排除）；
        // `~/.config/git/config`（git 配置文件，非凭证，应保留）
        std::fs::create_dir_all(config.join("git")).unwrap();
        std::fs::write(config.join("git/credentials"), "token").unwrap();
        std::fs::write(config.join("git/config"), "[user]\n\tname = test").unwrap();
        // ~/.cargo 下含 credentials 文件与 registry 目录
        let cargo = tmp.path().join(".cargo");
        std::fs::create_dir_all(cargo.join("registry")).unwrap();
        std::fs::write(cargo.join("credentials"), "token").unwrap();

        let (gh, gcloud, copilot, docker, git_creds, credentials) = (
            config.join("gh"),
            config.join("gcloud"),
            config.join("github-copilot"),
            config.join("docker"),
            config.join("git").join("credentials"),
            cargo.join("credentials"),
        );
        let denied = vec![
            gh.clone(),
            gcloud.clone(),
            copilot.clone(),
            docker.clone(),
            git_creds.clone(),
            credentials.clone(),
        ];
        let allowed = vec![config.clone(), cargo.clone()];
        let out = subtract_denied(allowed, &denied);

        // 凭证路径必须整体缺席（组件级 PathBuf 相等比较，兼容 Windows `\` 分隔符，
        // 避免按 `/` 后缀断言在 Windows 下失败）
        for cred in [&gh, &gcloud, &copilot, &docker, &git_creds, &credentials] {
            assert!(
                !out.iter().any(|p| p == cred),
                "凭证路径不得出现在展开结果: {out:?}"
            );
        }
        // 安全子项保留
        assert!(
            out.iter().any(|p| p == &config.join("fish")),
            "安全子项 .config/fish 应保留: {out:?}"
        );
        assert!(
            out.iter().any(|p| p == &config.join("git").join("config")),
            "git 目录的安全子项 .config/git/config 应保留: {out:?}"
        );
        assert!(
            out.iter().any(|p| p == &cargo.join("registry")),
            "安全子项 .cargo/registry 应保留: {out:?}"
        );
    }

    #[test]
    fn subtract_denied_keeps_clean_paths() {
        // 不含凭证子路径的 allow 原样保留
        let tmp = TempDir::new().unwrap();
        let clean = tmp.path().join(".cache");
        std::fs::create_dir_all(&clean).unwrap();
        let denied = vec![tmp.path().join(".ssh")];
        let out = subtract_denied(vec![clean.clone()], &denied);
        assert_eq!(out, vec![clean]);
    }

    #[test]
    fn vcs_dirs_returns_existing() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        // .hg/.svn 不创建，应被过滤
        let dirs = vcs_protected_dirs(tmp.path());
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with(".git"));
    }

    #[test]
    fn vcs_dirs_includes_git_file_form() {
        // SEC-13：worktree/submodule 的 .git 是文件（gitdir 指针），不得被过滤
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: /elsewhere/.git\n").unwrap();
        let dirs = vcs_protected_dirs(tmp.path());
        assert_eq!(dirs.len(), 1, "文件形式的 .git 应纳入保护");
    }

    #[test]
    fn vcs_dirs_empty_when_none() {
        let tmp = TempDir::new().unwrap();
        let dirs = vcs_protected_dirs(tmp.path());
        assert!(dirs.is_empty(), "expected empty: dirs");
    }

    #[test]
    fn harden_process_does_not_panic() {
        // 仅验证可调用、不 panic；实际效果取决于平台权限
        let _ = harden_process();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn home_read_allow_paths_collects_existing_whitelisted_dirs() {
        // 环境变量是进程级全局状态——使用模块级 ENV_SERIAL_LOCK 串行化
        let _guard = ENV_SERIAL_LOCK.lock().expect("env serial lock");
        let tmp = TempDir::new().unwrap();
        for d in [".cargo", ".rustup", ".config", "go"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        // 存在但不在白名单的目录（如凭证目录）不得被收集
        std::fs::create_dir(tmp.path().join(".ssh")).unwrap();

        // 快照全部相关环境变量，测试内统一受控（env 优先级高于 $HOME 默认值）
        const ENV_KEYS: &[&str] = &[
            "HOME",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "NVM_DIR",
            "VOLTA_HOME",
            "GOPATH",
        ];
        let snapshot: Vec<Option<String>> =
            ENV_KEYS.iter().map(|k| std::env::var(k).ok()).collect();
        // SAFETY: 测试进程内修改环境变量；已用 ENV_LOCK 串行化，且逐项恢复原值
        unsafe {
            for k in ENV_KEYS {
                std::env::remove_var(k);
            }
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("CARGO_HOME", tmp.path().join(".cargo"));
        }
        let paths = home_read_allow_paths();
        // 先恢复环境再断言，避免断言失败泄漏污染
        // SAFETY: 同上——恢复快照值
        unsafe {
            for (k, v) in ENV_KEYS.iter().zip(&snapshot) {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }

        for expected in [
            tmp.path().join(".cargo"),
            tmp.path().join(".rustup"),
            tmp.path().join(".config"),
            tmp.path().join("go"),
        ] {
            assert!(
                paths.contains(&expected),
                "白名单应含存在的工具链/缓存目录 {expected:?}，实际 {paths:?}"
            );
        }
        assert!(
            !paths.contains(&tmp.path().to_path_buf()),
            "$HOME 本身不得整体放行（A3 收敛语义）"
        );
        assert!(
            !paths.iter().any(|p| p.ends_with(".ssh")),
            "凭证目录不得进入读白名单"
        );
        // 不存在条目（.cache/.local/.npm/.nvm/.volta 未创建）应被过滤
        for absent in [".cache", ".local", ".npm", ".nvm", ".volta"] {
            assert!(
                !paths.iter().any(|p| p.ends_with(absent)),
                "不存在的 {absent} 应被过滤，实际 {paths:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn home_read_allow_paths_env_overrides_default() {
        let _guard = ENV_SERIAL_LOCK.lock().expect("env serial lock");
        let tmp = TempDir::new().unwrap();
        let custom_cargo = tmp.path().join("custom-toolchain");
        std::fs::create_dir(&custom_cargo).unwrap();

        let prev_cargo = std::env::var("CARGO_HOME").ok();
        // SAFETY: 同上——ENV_LOCK 串行 + 测试内一次性
        unsafe { std::env::set_var("CARGO_HOME", &custom_cargo) };
        let paths = home_read_allow_paths();
        match prev_cargo {
            Some(c) => unsafe { std::env::set_var("CARGO_HOME", c) },
            None => unsafe { std::env::remove_var("CARGO_HOME") },
        }

        assert!(
            paths.contains(&custom_cargo),
            "$CARGO_HOME 优先于 ~/.cargo 默认值"
        );
    }
}

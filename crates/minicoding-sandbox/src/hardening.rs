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
/// 仅返回实际存在的目录（避免 `landlock` `PathFd` 打开不存在路径报错）。
/// 供 landlock 只读规则（workdir 只读场景）与 policy builtin 黑名单（workdir
/// 可写场景）使用（C-22 VCS 保护）。
#[must_use]
pub fn vcs_protected_dirs(workdir: &Path) -> Vec<PathBuf> {
    const VCS_NAMES: &[&str] = &[".git", ".hg", ".svn"];
    VCS_NAMES
        .iter()
        .map(|name| workdir.join(name))
        .filter(|p| p.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use tempfile::TempDir;

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
}

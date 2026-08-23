//! Linux Landlock 沙箱驱动（T-M4-1）。
//!
//! 基于 `landlock` crate（rust-landlock，MIT/Apache-2.0）实现 `SandboxDriver` trait。
//! 文件系统隔离：workdir 可写、其余只读、VCS 目录保护、系统只读路径放行。
//!
//! ## apply 时机
//!
//! `apply()` 在**父进程**构建 `RulesetCreated`（打开 `PathFd`、`add_rule`），然后通过
//! `Command::pre_exec` 在**子进程** fork 后 exec 前调用 `restrict_self()`。父进程
//! 不被约束，子进程 exec 后 Landlock 约束持久保持（LSM 级跨 exec）。
//!
//! ## 旧内核降级
//!
//! `landlock_available()` 用 `HardRequirement` + `create()` 探测内核支持（不约束
//! 探测进程）。内核 < 5.13 或 Landlock 未启用时返回 `false`，`detect_driver()`
//! 据此降级 `NoopDriver` + warn（C-22）。
//!
//! ## VCS 保护限制
//!
//! Landlock 规则是"白名单并集"语义：workdir 可写会让其下 `.git` 也继承可写
//! （无法在可写父目录下做子目录只读）。故 VCS 目录（`.git`/`.hg`/`.svn`）的
//! 写保护由应用层 `policy::builtin` 黑名单补充（S5 已落地：fs/shell 写 .git
//! 与约束文件硬 Deny，见 `security.md` §16.1），landlock
//! 仅做粗粒度"workdir 可写、其余只读"隔离。这是 OS 层第二道防线与应用层第一道
//! 防线的分工（见 `security.md` §8）。

use crate::hardening::vcs_protected_dirs;
use landlock::{RulesetAttr, RulesetCreatedAttr};
use minicoding_core::sandbox::{SandboxDriver, SandboxError, SandboxPolicy};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

/// Landlock ABI 上限（V3 = Linux 6.2，含 Truncate，覆盖 `std::fs::write` 等常见写操作）。
/// `BestEffort` 兼容模式下自动降到内核实际支持版本。
const TARGET_ABI: landlock::ABI = landlock::ABI::V3;

/// 只读放行的系统路径（命令/库/设备/proc 必须可读，否则子进程无法 exec）。
const SYSTEM_RO_PATHS: &[&str] = &[
    "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/dev", "/proc",
];

/// Linux Landlock 沙箱驱动。
///
/// 无状态（所有策略通过 `apply` 参数传入），可被 `Runtime` 以 `Arc<dyn SandboxDriver>`
/// 共享。`is_hardened()` 恒 `true`（构造前已由 `landlock_available()` 探测确认）。
pub struct LandlockDriver;

impl LandlockDriver {
    /// 创建 Landlock 驱动。
    ///
    /// 调用方应先经 `landlock_available()` 确认内核支持，否则用 `NoopDriver`。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for LandlockDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxDriver for LandlockDriver {
    fn apply(
        &self,
        policy: &SandboxPolicy,
        cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        match policy {
            SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => {
                apply_landlock(policy, cmd)
            }
            // ExternalSandbox / DangerFullAccess 不应用内核限制（C-22：依赖外部隔离
            // 或用户显式放弃隔离）。NoopDriver 等价语义，但走这里说明用户选了这两
            // 种策略之一，故意不拦。
            SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => Ok(()),
        }
    }

    fn is_hardened(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "landlock"
    }
}

/// 探测内核是否支持 Landlock（不约束探测进程）。
///
/// 用 `HardRequirement` + `create()` 探测：仅调用 `landlock_create_ruleset(2)`，
/// 内核 `ENOSYS`/`EOPNOTSUPP` 时返回 `Err`，据此判定不支持。不调用 `restrict_self`，
/// 不会约束当前进程（见 landlock 研究报告方案 A）。
#[must_use]
pub fn landlock_available() -> bool {
    use landlock::{Access, CompatLevel, Compatible, Ruleset};

    Ruleset::default()
        .handle_access(landlock::AccessFs::from_all(landlock::ABI::V1))
        .ok()
        .map(|r| r.set_compatibility(CompatLevel::HardRequirement))
        .and_then(|r| r.create().ok())
        .is_some()
}

/// 在子进程 `pre_exec` 内应用 Landlock 限制。
///
/// 父进程构建 `RulesetCreated`（打开 `PathFd`、`add_rule`），子进程 `pre_exec` 内仅
/// `restrict_self()`。`Option::take()` 模式解决 `restrict_self(self)` 的 `FnOnce`
/// 语义与 `pre_exec` 要求 `FnMut` 的冲突。
fn apply_landlock(
    policy: &SandboxPolicy,
    cmd: &mut std::process::Command,
) -> Result<(), SandboxError> {
    let ruleset = build_ruleset(policy)?;
    let mut ruleset_slot = Some(ruleset);

    // SAFETY: pre_exec 闭包在 fork 后、exec 前的子进程内运行（单线程上下文）。
    // 闭包体内仅做：Option::take（栈操作）+ restrict_self（两个 syscall：
    // landlock_restrict_self + prctl(PR_SET_NO_NEW_PRIVS)）+ 构造栈上
    // RestrictionStatus + drop RulesetCreated（close fd，close(2) async-signal-safe）。
    // 成功路径无堆分配、无锁、无 malloc，满足 POSIX async-signal-safe 要求。
    // 错误路径的 to_string 分配仅发生在即将 exec 失败时，可接受。
    unsafe {
        cmd.pre_exec(move || {
            let rs = ruleset_slot
                .take()
                .ok_or_else(|| std::io::Error::other("pre_exec invoked twice"))?;
            let status = rs
                .restrict_self()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            match status.ruleset {
                landlock::RulesetStatus::FullyEnforced => Ok(()),
                landlock::RulesetStatus::PartiallyEnforced => Err(std::io::Error::other(format!(
                    "landlock only partially enforced: {status:?}"
                ))),
                landlock::RulesetStatus::NotEnforced => Err(std::io::Error::other(format!(
                    "landlock not enforced: {status:?}"
                ))),
            }
        });
    }
    Ok(())
}

/// 父进程构建 Landlock ruleset。
///
/// - `ReadOnly`：workdir 只读，所有路径只读放行；
/// - `WorkspaceWrite`：workdir + writable 可写，其余只读，VCS 目录列入只读
///   （注：landlock 并集语义下 workdir 可写会使 .git 继承可写，VCS 实际写保护
///   由应用层 builtin 黑名单补充（S5 已落地），见模块文档）。
fn build_ruleset(policy: &SandboxPolicy) -> Result<landlock::RulesetCreated, SandboxError> {
    use landlock::{Access, Ruleset, path_beneath_rules};

    let handled = landlock::AccessFs::from_all(TARGET_ABI);
    let ro_access = landlock::AccessFs::from_read(TARGET_ABI);
    let write_access = landlock::AccessFs::from_all(TARGET_ABI);

    let mut ruleset = Ruleset::default()
        .handle_access(handled)
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?
        .create()
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?;

    // 系统只读路径放行（必须，否则子进程无法 exec / 读库）
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            SYSTEM_RO_PATHS.iter().copied(),
            ro_access,
        ))
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?;

    // HOME 只读 + TMPDIR 读写（2026-08-23 审查 §9-P2 可用性修复）：landlock
    // 未列入规则的路径**连读都拒绝**——此前 $HOME/.cargo、~/.ssh 全不可读，
    // /tmp 不可写，cargo build/编译器/测试框架大概率直接失败，把用户推向
    // external-sandbox/danger-full-access 的安全侵蚀压力。HOME 只读满足
    // registry 缓存/配置读取；TMPDIR 读写满足编译临时目录。
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        ruleset = ruleset
            .add_rules(path_beneath_rules([home.as_str()], ro_access))
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
    }
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    ruleset = ruleset
        .add_rules(path_beneath_rules([tmpdir.as_str()], write_access))
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?;

    // 按策略放行 workdir / writable
    let (workdir, writable): (PathBuf, Vec<PathBuf>) = match policy {
        SandboxPolicy::ReadOnly => {
            // ReadOnly：workdir 默认为当前目录，只读放行
            (
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                vec![],
            )
        }
        SandboxPolicy::WorkspaceWrite { workdir, writable } => (
            workdir.clone().into_std_path_buf(),
            writable
                .iter()
                .map(|p| p.clone().into_std_path_buf())
                .collect(),
        ),
        SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => {
            // 这两种策略不应用 landlock（apply() 已提前返回 Ok），不应到达此处
            return Err(SandboxError::Sandbox(
                "build_ruleset 不应为 ExternalSandbox/DangerFullAccess 调用".into(),
            ));
        }
    };

    let workdir_str = workdir.to_string_lossy().into_owned();
    let writable_strs: Vec<String> = writable
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    if matches!(policy, SandboxPolicy::WorkspaceWrite { .. }) {
        // workdir 可写
        ruleset = ruleset
            .add_rules(path_beneath_rules([workdir_str.as_str()], write_access))
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
        // 额外 writable 可写
        if !writable_strs.is_empty() {
            ruleset = ruleset
                .add_rules(path_beneath_rules(
                    writable_strs.iter().map(String::as_str),
                    write_access,
                ))
                .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
        }
    } else {
        // ReadOnly：workdir 只读放行
        ruleset = ruleset
            .add_rules(path_beneath_rules([workdir_str.as_str()], ro_access))
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
    }

    // VCS 目录只读保护（best effort：landlock 并集语义下若 workdir 可写则 .git
    // 仍可写，此处规则在 workdir 只读场景下生效；workdir 可写场景由应用层补充）
    let vcs_dirs = vcs_protected_dirs(&workdir);
    let vcs_strs: Vec<String> = vcs_dirs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if !vcs_strs.is_empty() {
        ruleset = ruleset
            .add_rules(path_beneath_rules(
                vcs_strs.iter().map(String::as_str),
                ro_access,
            ))
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
    }

    Ok(ruleset)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn landlock_available_does_not_panic() {
        // 仅验证探测函数可调用、不 panic；实际 true/false 取决于内核
        let _ = landlock_available();
    }

    #[test]
    fn driver_id_and_hardened() {
        let d = LandlockDriver::new();
        assert_eq!(d.id(), "landlock");
        assert!(d.is_hardened());
    }

    #[test]
    fn external_and_full_access_are_noop() {
        // ExternalSandbox / DangerFullAccess 不应用 landlock（apply 返回 Ok 不加 pre_exec）
        let d = LandlockDriver::new();
        let mut cmd = std::process::Command::new("true");
        d.apply(&SandboxPolicy::ExternalSandbox, &mut cmd).unwrap();
        d.apply(&SandboxPolicy::DangerFullAccess, &mut cmd).unwrap();
    }
}

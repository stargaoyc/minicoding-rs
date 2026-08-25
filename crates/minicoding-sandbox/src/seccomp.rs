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
//! 本过滤器**不**封堵网络类 syscall（socket/connect/sendto 等保持放行）——
//! landlock ABI4 已拒绝子进程 TCP bind/connect；UDP/DNS 残留通道由
//! web 工具的 DNS 解析-连接 IP pinning（A2，`minicoding-tools::web`）在应用层
//! 另行处理，seccomp 层不做重复且更粗糙的拦截。
//!
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
/// - 命名空间切换（逃逸沙箱命名空间）：`setns`/`unshare`。
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
    // Native 架构：子进程与父进程同架构（pre_exec fork 语义保证），无需多架构
    ctx.add_arch(ScmpArch::Native)
        .map_err(|e| SandboxError::Sandbox(format!("seccomp add_arch 失败: {e}")))?;

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
        // 关键项防回退：列表被误删时在此暴露
        for required in ["ptrace", "bpf", "unshare", "setns", "open_by_handle_at"] {
            assert!(
                DENIED_SYSCALLS.contains(&required),
                "deny-list 缺少关键 syscall `{required}`"
            );
        }
    }
}

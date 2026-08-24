//! Windows Job Object 进程遏制沙箱驱动。
//!
//! 基于 `windows-sys` crate（MIT/Apache-2.0）实现 `SandboxDriver` trait。
//! **单层遏制**（诚实边界，2026-08-25 审查 §6.2-S5 措辞修正）：
//! 1. **Job Object**：限制子进程创建数量、UI 访问（剪贴板等）、
//!    `CREATE_SUSPENDED` 两阶段确保首条指令前生效；
//!
//! Job Object **不提供**：文件系统隔离（不像 Linux Landlock / macOS Seatbelt）、
//! 网络过滤（需 WFP）、CPU/内存资源上限（当前未设置 JOBOBJECT 限额字段）。
//! 文件系统隔离需要 AppContainer 或 Mandatory Integrity Control，作为后续增强。
//! 当前实现提供进程级遏制（限制子进程数、UI 隔离），是 Windows 平台
//! 最佳实践的子集，优于完全无沙箱（NoopDriver）——`is_hardened()` 如实
//! 返回 false，doctor 不高估防护。
//!
//! ## apply + post_spawn 两阶段
//!
//! Windows 不支持 `pre_exec`（Linux/macOS 的 fork+exec 模型），进程创建与首条
//! 指令之间无用户态钩子。故采用两阶段：
//! - `apply`：设置 `CREATE_SUSPENDED` 标志（进程创建后挂起，不执行任何代码）；
//! - `post_spawn`：创建 Job Object → 分配子进程 → 恢复线程。
//! 这样子进程首条指令执行前 Job Object 已生效。
//!
//! **网络隔离（2026-08-23 审查遗留#1）**：Job Object 无网络过滤原语（WFP 才
//! 能做），Windows 平台子进程**不限制网络**——`is_hardened()` 如实返回 false；
//! 网络管控仅由应用层权限审批承担。文档矩阵中该平台网络列为"未实现"。

use minicoding_core::sandbox::{SandboxDriver, SandboxError, SandboxPolicy};
use std::io;

/// `CREATE_SUSPENDED`：进程创建后挂起主线程，等待 `ResumeThread`。
const CREATE_SUSPENDED: u32 = 0x00000004;

/// Windows Job Object 沙箱驱动。
///
/// `apply` 设置 `CREATE_SUSPENDED`，`post_spawn` 创建 Job Object 并恢复线程。
/// 存储策略供 `post_spawn` 使用（Windows 不支持 `pre_exec`，策略无法在闭包中传递）。
pub struct WindowsJobDriver {
    /// 最近一次 `apply` 的策略快照（供 `post_spawn` 读取）。
    /// 使用 `Mutex` 保证线程安全（`SandboxDriver: Send + Sync`）。
    ///
    /// S24 已知限制（文档化）：共享同一 driver 实例**并发** spawn 时，
    /// apply(A)/apply(B)/post_spawn(A) 交错会使 A 拿到 B 的策略。实际无风险：
    /// Runtime 对副作用工具严格串行（design.md §2.3 规则 2），且 builder 每个
    /// Runtime 独立 `detect_driver()`——不存在跨 Runtime 共享 driver 的路径。
    /// 若未来引入并发 spawn 场景，需改为 pid→policy 映射或 apply 返回句柄。
    last_policy: std::sync::Mutex<Option<SandboxPolicy>>,
    /// 活跃 Job Object 句柄（2026-08-25 审查 §6.2-S5）。
    ///
    /// 此前 `assign_process_to_job` 后 JobHandle 立即 drop——运行期失去 kill
    /// 整个 Job 的能力，`KILL_ON_JOB_CLOSE` 的泄漏防护承诺落空（该标志绑定
    /// "最后句柄关闭"事件）。句柄保存在驱动内：随驱动（即 Runtime）drop 时
    /// 关闭并按 `KILL_ON_JOB_CLOSE` 终止残留沙箱子进程。串行 spawn 不变式下
    /// 同一时刻至多一个活跃 Job。
    active_job: std::sync::Mutex<Option<JobHandle>>,
}

impl WindowsJobDriver {
    /// 创建 Windows Job Object 驱动。
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_policy: std::sync::Mutex::new(None),
            active_job: std::sync::Mutex::new(None),
        }
    }
}

impl Default for WindowsJobDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxDriver for WindowsJobDriver {
    fn apply(
        &self,
        policy: &SandboxPolicy,
        cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        match policy {
            SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => {
                // 存储策略快照供 post_spawn 使用
                *self
                    .last_policy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(policy.clone());

                // 设置 CREATE_SUSPENDED：进程创建后挂起，post_spawn 分配 Job Object 后恢复
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_SUSPENDED);
                Ok(())
            }
            SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => Ok(()),
        }
    }

    fn post_spawn(&self, pid: u32) -> Result<(), SandboxError> {
        let policy = self
            .last_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        let Some(policy) = policy else {
            // 无策略（ExternalSandbox/DangerFullAccess 或 apply 未调用）：仅恢复线程
            resume_thread(pid)?;
            return Ok(());
        };

        // 创建 Job Object，分配子进程，恢复线程
        let job = create_restricted_job(&policy)?;
        if let Err(e) = assign_process_to_job(&job, pid) {
            // 分配失败也要恢复线程：挂起进程若不 resume 将永久泄漏（S5）
            let _ = resume_thread(pid);
            return Err(e);
        }
        *self
            .active_job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
        resume_thread(pid)?;
        Ok(())
    }

    fn is_hardened(&self) -> bool {
        // S25：Windows 驱动仅进程遏制（Job Object）+ 受限令牌，无文件系统隔离——
        // 如实报告，避免 doctor --security 高估防护（对齐 security.md §8.2）
        false
    }

    fn id(&self) -> &'static str {
        "windows-token"
    }
}

// ── Windows FFI ──────────────────────────────────────────────────────────────

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};

/// Job Object 句柄包装：Drop 时自动 CloseHandle（触发 `KILL_ON_JOB_CLOSE`）。
struct JobHandle(HANDLE);

impl Drop for JobHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: handle 来自 CreateJobObjectW，有效时 CloseHandle 释放资源。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// 创建受限 Job Object。
///
/// 限制：
/// - `KILL_ON_JOB_CLOSE`：Job 最后句柄关闭时终止所有子进程（防泄漏）；
/// - `DIE_ON_UNHANDLED_EXCEPTION`：未处理异常时终止子进程；
/// - `LIMIT_ACTIVE_PROCESS`：限制子进程数（防 fork bomb）；
/// - UI 限制：禁止剪贴板/系统参数/退出 Windows 等。
///
/// 不设 `BREAKAWAY_OK`（2026-08-25 审查 §6.2-S5 方向修正）：该标志允许 Job 内
/// 进程以 `CREATE_BREAKAWAY_FROM_JOB` 创建**脱离 Job 的后代**——沙箱场景恰恰
/// 不应放行脱离。
fn create_restricted_job(_policy: &SandboxPolicy) -> Result<JobHandle, SandboxError> {
    // SAFETY: CreateJobObjectW(NULL, NULL) 创建匿名 Job Object，无内存安全风险。
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }

    // 设置扩展限制
    let mut ext_limit: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    ext_limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    ext_limit.BasicLimitInformation.ActiveProcessLimit = 64; // 限制 64 个子进程

    // SAFETY: handle 有效，ext_limit 为栈上 POD 结构体。
    let ret = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &ext_limit as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ret == 0 {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }

    // 设置 UI 限制（禁止剪贴板/系统参数/退出 Windows）
    let mut ui_limit: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { std::mem::zeroed() };
    ui_limit.UIRestrictionsClass = JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
        | JOB_OBJECT_UILIMIT_EXITWINDOWS
        | JOB_OBJECT_UILIMIT_GLOBALATOMS
        | JOB_OBJECT_UILIMIT_READCLIPBOARD
        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;

    // SAFETY: handle 有效，ui_limit 为栈上 POD 结构体。
    let ret = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectBasicUIRestrictions,
            &ui_limit as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )
    };
    if ret == 0 {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }

    Ok(JobHandle(handle))
}

/// 分配子进程到 Job Object。
///
/// 仅借用 `&JobHandle`——句柄所有权留在驱动（`active_job`），运行期保留
/// kill 整个 Job 的能力（S5）。
fn assign_process_to_job(job: &JobHandle, pid: u32) -> Result<(), SandboxError> {
    // 打开子进程句柄
    let process_handle = open_process_handle(pid)?;

    // SAFETY: job handle 与 process handle 均有效。
    let ret = unsafe { AssignProcessToJobObject(job.0, process_handle) };

    // 关闭进程句柄（已分配到 Job，不需要持有）
    // SAFETY: process_handle 来自 OpenProcess，有效时 CloseHandle 释放。
    unsafe {
        CloseHandle(process_handle);
    }

    if ret == 0 {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

/// 通过 PID 打开进程句柄。
fn open_process_handle(pid: u32) -> Result<HANDLE, SandboxError> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    // SAFETY: OpenProcess 仅请求 SET_QUOTA + TERMINATE 权限，PID 来自 spawn()。
    let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }
    Ok(handle)
}

/// 恢复子进程主线程（解除 CREATE_SUSPENDED 挂起）。
///
/// toolhelp 线程快照对刚创建（CREATE_SUSPENDED）的进程存在已知竞态：进程创建后
/// 立即枚举快照，其线程条目可能尚未出现（表现为 `no suspendable thread found`）。
/// 故短重试数次（同步路径：`SandboxDriver::apply/post_spawn` 为同步 trait，
/// spawn 后立即调用，20ms 级等待可接受）。
fn resume_thread(pid: u32) -> Result<(), SandboxError> {
    for attempt in 0..3 {
        match try_resume_thread(pid) {
            Ok(true) => return Ok(()),
            // 未找到且未耗尽重试：稍等再枚举（快照竞态）
            Ok(false) if attempt < 2 => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(false) => {
                return Err(SandboxError::Sandbox(format!(
                    "no suspendable thread found for pid {pid}"
                )));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("resume_thread loop always returns")
}

/// 单次枚举线程快照，尝试恢复目标 PID 的主线程。返回是否成功恢复。
fn try_resume_thread(pid: u32) -> Result<bool, SandboxError> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // 枚举系统线程，找到属于目标 PID 的线程
    // SAFETY: CreateToolhelp32Snapshot 创建线程快照。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Io(io::Error::last_os_error()));
    }

    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;

    // SAFETY: snapshot 有效，entry 已初始化 dwSize。
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let mut resumed = false;

    while found {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: thread_id 来自快照，请求 SUSPEND_RESUME 权限。
            let thread_handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread_handle.is_null() && thread_handle != INVALID_HANDLE_VALUE {
                // SAFETY: thread_handle 有效，ResumeThread 恢复挂起的线程。
                let _ = unsafe { ResumeThread(thread_handle) };
                // SAFETY: 关闭线程句柄。
                unsafe { CloseHandle(thread_handle) };
                resumed = true;
            }
        }
        // SAFETY: 继续枚举。
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }

    // SAFETY: 关闭快照句柄。
    unsafe { CloseHandle(snapshot) };

    Ok(resumed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn driver_id_and_hardened() {
        let d = WindowsJobDriver::new();
        assert_eq!(d.id(), "windows-token");
        assert!(!d.is_hardened()); // S25：无文件系统隔离，如实报告
    }

    #[test]
    fn external_and_full_access_are_noop() {
        let d = WindowsJobDriver::new();
        let mut cmd = std::process::Command::new("cmd");
        d.apply(&SandboxPolicy::ExternalSandbox, &mut cmd).unwrap();
        d.apply(&SandboxPolicy::DangerFullAccess, &mut cmd).unwrap();
    }

    #[test]
    fn apply_sets_create_suspended() {
        let d = WindowsJobDriver::new();
        let mut cmd = std::process::Command::new("cmd");
        d.apply(
            &SandboxPolicy::WorkspaceWrite {
                workdir: camino::Utf8PathBuf::from("."),
                writable: vec![],
            },
            &mut cmd,
        )
        .unwrap();
        // CREATE_SUSPENDED 已通过 creation_flags 设置（无法从 Command 提取标志验证，
        // 但 apply 不报错即说明 CommandExt::creation_flags 调用成功）
    }
}

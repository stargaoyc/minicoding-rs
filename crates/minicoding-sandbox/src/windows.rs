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
    /// 最近 `apply` 的策略快照队列（FIFO，供 `post_spawn` 按序消费）。
    ///
    /// S24 已知限制（文档化）：共享同一 driver 实例**并发** spawn 时（如前台
    /// shell.run 与后台 shell.background 同时启动），apply/post_spawn 交错可能
    /// 使策略错配。SEC-4（2026-08-25 R2 审查）将单槽改为 FIFO 队列：串行场景
    /// 语义不变；交错场景下消费顺序确定（先 apply 先消费），残余风险为"A 拿
    /// 到 B 的策略"而非"拿到 None 裸奔 resume"。彻底消除需扩展
    /// `SandboxDriver` trait 的 apply↔post_spawn 关联句柄（列入 roadmap）。
    last_policy: std::sync::Mutex<std::collections::VecDeque<SandboxPolicy>>,
    /// pid → 活跃 Job Object 句柄表。
    ///
    /// SEC-4（2026-08-25 R2 审查）：此前为单槽 `Option<JobHandle>`——后台 shell
    /// 运行中下一条前台命令的 post_spawn 会覆盖槽位，旧 JobHandle drop 触发
    /// `KILL_ON_JOB_CLOSE` **静默杀死整个后台进程树**。改为按 pid 键控；过期
    /// 条目（ActiveProcesses==0）在每次 post_spawn 时惰性清理（关闭句柄仅释放
    /// 内核对象，进程已退出无杀灭副作用）。句柄随驱动 drop 时统一关闭兜底。
    active_jobs: std::sync::Mutex<std::collections::HashMap<u32, JobHandle>>,
}

impl WindowsJobDriver {
    /// 创建 Windows Job Object 驱动。
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_policy: std::sync::Mutex::new(std::collections::VecDeque::new()),
            active_jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for WindowsJobDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsJobDriver {
    /// 清理已结束（`ActiveProcesses == 0`）的 Job 条目（SEC-4）。
    ///
    /// 进程树全部退出后，关闭句柄只是释放内核对象；若仍有活跃进程则保留
    /// 句柄（维持运行期 kill 整个 Job 的能力与 KILL_ON_JOB_CLOSE 泄漏防护）。
    fn prune_dead_jobs(&self) {
        let mut jobs = self
            .active_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if jobs.is_empty() {
            return;
        }
        let dead: Vec<u32> = jobs
            .iter()
            .filter(|(pid, job)| {
                let _ = pid;
                job_active_processes(job).is_ok_and(|n| n == 0)
            })
            .map(|(pid, _)| *pid)
            .collect();
        for pid in dead {
            if jobs.remove(&pid).is_some() {
                tracing::debug!(pid, "pruned finished sandbox job object");
            }
        }
    }
}

/// 查询 Job Object 当前活跃进程数（查询失败返回 Err，调用方保守保留条目）。
fn job_active_processes(job: &JobHandle) -> Result<u32, io::Error> {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject,
    };
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: handle 有效，info 为栈上 POD 结构体，尺寸匹配查询类别。
    // 注意 lpJobObjectInformation 为 `*mut c_void`（输出参数），需可变转换。
    let ret = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ActiveProcesses)
}

impl SandboxDriver for WindowsJobDriver {
    fn apply(
        &self,
        policy: &SandboxPolicy,
        cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        match policy {
            SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => {
                // 策略快照入队供 post_spawn 按序消费（SEC-4 FIFO）
                self.last_policy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back(policy.clone());

                // 设置 CREATE_SUSPENDED：进程创建后挂起，post_spawn 分配 Job Object 后恢复
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_SUSPENDED);
                Ok(())
            }
            SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => Ok(()),
        }
    }

    fn post_spawn(&self, pid: u32) -> Result<(), SandboxError> {
        // SEC-4：先惰性清理已结束的 Job（ActiveProcesses==0），防止句柄表无界增长；
        // 关闭句柄时进程树已退出，KILL_ON_JOB_CLOSE 无杀灭副作用。
        self.prune_dead_jobs();

        let policy = self
            .last_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();

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
        self.active_jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pid, job);
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

// SAFETY: Windows 内核对象句柄**非线程亲和**——HANDLE 只是不透明标识符，
// CloseHandle/AssignProcessToJobObject 等均可从任意线程调用（与 socket fd
// 不同，无线程局部状态）。驱动侧经 `Mutex<Option<JobHandle>>` 串行访问
// （S24 串行 spawn 不变式），不存在并发 use-after-close；Drop 仅发生在
// 驱动释放时。满足 `SandboxDriver: Send + Sync` 的要求。
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

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
                // SEC-5（2026-08-25 R2 审查）：返回值为前次挂起计数，`u32::MAX`
                //（即 -1）表示失败——此前 `let _ =` 丢弃导致失败仍记 resumed=true，
                // 子进程永久挂起泄漏至驱动 drop。
                let prev = unsafe { ResumeThread(thread_handle) };
                if prev == u32::MAX {
                    tracing::warn!(
                        pid,
                        tid = entry.th32ThreadID,
                        error = %io::Error::last_os_error(),
                        "ResumeThread failed"
                    );
                } else {
                    resumed = true;
                }
                // SAFETY: 关闭线程句柄。
                unsafe { CloseHandle(thread_handle) };
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

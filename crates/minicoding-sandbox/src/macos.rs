//! macOS Seatbelt 沙箱驱动。
//!
//! 基于 `sandbox_init(3)` (deprecated but functional) FFI 实现 `SandboxDriver` trait。
//! 文件系统隔离：workdir 可写、其余只读、VCS 目录保护、系统只读路径放行。
//!
//! ## apply 时机
//!
//! `apply()` 在**父进程**生成 Seatbelt profile 并写入临时文件，然后通过
//! `Command::pre_exec` 在**子进程** fork 后 exec 前调用 `sandbox_init`。
//! 父进程不被约束，子进程 exec 后 Seatbelt 约束持久保持（内核级跨 exec）。
//!
//! ## 临时文件生命周期
//!
//! Profile 写入 tempfile 随机名 `.sb`（S26），`pre_exec` 内
//! `sandbox_init` 成功读取后立即删除（profile 已加载进内核，文件不再需要）。
//! 若 `sandbox_init` 失败也删除临时文件并返回错误。
//!
//! ## 限制
//!
//! `sandbox_init` 被 Apple 标记为 deprecated，但仍功能完整，是纯 Rust 中最实际的
//! Seatbelt 接入方式（`sandbox-exec` 需重写 Command 无法与现有 trait 配合）。
//! Apple 推荐的 Containerization framework 仅 Swift 可用，不适用于 Rust。

use crate::hardening::vcs_protected_dirs;
use minicoding_core::sandbox::{SandboxDriver, SandboxError, SandboxPolicy};
use std::ffi::CString;
use std::os::unix::process::CommandExt;

/// `SANDBOX_NAMED_EXTERNAL`：从文件路径加载 profile（见 `<sandbox.h>`）。
const SANDBOX_NAMED_EXTERNAL: u64 = 0x2;

unsafe extern "C" {
    /// 初始化沙箱（deprecated but functional，见 `<sandbox.h>`）。
    fn sandbox_init(
        profile: *const std::os::raw::c_char,
        flags: u64,
        errorbuf: *mut *mut std::os::raw::c_char,
    ) -> std::os::raw::c_int;

    /// 释放 `sandbox_init` 的 errorbuf。
    fn sandbox_free_error(errorbuf: *mut std::os::raw::c_char);
}

/// macOS Seatbelt 沙箱驱动。
///
/// 无状态（所有策略通过 `apply` 参数传入），可被 `Runtime` 以 `Arc<dyn SandboxDriver>`
/// 共享。`is_hardened()` 恒 `true`。
pub struct SeatbeltDriver;

impl SeatbeltDriver {
    /// 创建 Seatbelt 驱动。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for SeatbeltDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxDriver for SeatbeltDriver {
    fn apply(
        &self,
        policy: &SandboxPolicy,
        cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        match policy {
            SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => {
                apply_seatbelt(policy, cmd)
            }
            // ExternalSandbox / DangerFullAccess 不应用内核限制（C-22）。
            SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => Ok(()),
        }
    }

    fn is_hardened(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "seatbelt"
    }
}

/// 探测 macOS 是否支持 Seatbelt（`sandbox_init` 可用）。
///
/// macOS 10.5+ 均内置 Seatbelt，此函数始终返回 `true`。
#[must_use]
pub fn seatbelt_available() -> bool {
    // sandbox_init 在 macOS 10.5+ 均可用，无需运行期探测。
    true
}

/// 在子进程 `pre_exec` 内应用 Seatbelt 限制。
/// Seatbelt profile 字符串转义（2026-08-23 审查 §9-P2）：workdir/writable 来自
/// CLI/config，含 `"` 或 `)` 的路径可闭合 `(subpath "...")` 表达式注入新指令
/// （如把任意目录加入可写白名单）。Seatbelt 无标准转义机制——含 `(`/`)` 的
/// 路径直接拒绝（fail-closed），反斜杠/双引号转义。
fn seatbelt_escape(path: &str) -> std::io::Result<String> {
    for bad in ['(', ')'] {
        if path.contains(bad) {
            return Err(std::io::Error::other(format!(
                "sandbox: 路径含 Seatbelt 元字符 `{bad}`，拒绝生成 profile: {path}"
            )));
        }
    }
    Ok(path.replace('"', "\\\""))
}
fn apply_seatbelt(
    policy: &SandboxPolicy,
    cmd: &mut std::process::Command,
) -> Result<(), SandboxError> {
    let profile = build_profile(policy).map_err(|e| {
        tracing::error!(error = %e, "seatbelt profile 构建失败");
        e
    })?;

    // S26：tempfile 随机名 + 0600——消除 `/tmp` 可预测名的符号链接竞争窗口。
    // NamedTempFile 不 drop（disable_cleanup 等价：保留路径），由 pre_exec 内
    // sandbox_init 成功后删除；失败路径随 TempPath drop 清理。
    // 残留窗口（SEC-11，2026-08-25 R2 审查，如实记录）：keep() 之后若 Command
    // 在 fork 前（而非 exec 阶段）失败，父进程侧无清理钩子会残留一个 .sb 文件
    // ——std 无"spawn 完成回调"，彻底闭环需调用方协作；exec 阶段失败
    // （program-not-found 等）发生在 pre_exec 之后，子进程仍会正常删除。
    let tmp_file = tempfile::Builder::new()
        .prefix("minicoding-seatbelt")
        .suffix(".sb")
        .tempfile()
        .map_err(|e| SandboxError::Io(std::io::Error::other(e.to_string())))?;
    // 写入 profile 后保留路径（into_temp_path().keep() → io::Result<PathBuf>）
    use std::io::Write as _;
    write!(tmp_file.as_file(), "{profile}")
        .map_err(|e| SandboxError::Io(std::io::Error::other(e.to_string())))?;
    tmp_file
        .as_file()
        .sync_all()
        .map_err(|e| SandboxError::Io(std::io::Error::other(e.to_string())))?;
    let tmp_path_buf = tmp_file
        .into_temp_path()
        .keep()
        .map_err(|e| SandboxError::Io(e.into()))?;
    let tmp_path = tmp_path_buf.to_string_lossy().into_owned();

    // pre_exec 闭包：子进程内调 sandbox_init，成功后删除临时文件。
    let profile_path = tmp_path;
    unsafe {
        cmd.pre_exec(move || {
            let path_c = match CString::new(profile_path.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = std::fs::remove_file(&profile_path);
                    return Err(std::io::Error::other(e.to_string()));
                }
            };
            let mut errbuf: *mut std::os::raw::c_char = std::ptr::null_mut();

            // SAFETY: sandbox_init 读取 profile 文件路径，加载 Seatbelt 规则到内核。
            // errbuf 由 sandbox 分配，失败时需 sandbox_free_error 释放。
            // pre_exec 在 fork 后单线程上下文，无并发风险。
            let rc = sandbox_init(path_c.as_ptr(), SANDBOX_NAMED_EXTERNAL, &mut errbuf);

            // 临时文件已读取完毕（无论成功失败），删除。
            let _ = std::fs::remove_file(&profile_path);

            if rc != 0 {
                let msg = if errbuf.is_null() {
                    "unknown sandbox_init error".to_string()
                } else {
                    // SAFETY: errbuf 由 sandbox_init 分配，非空时为有效 C 字符串。
                    let msg = std::ffi::CStr::from_ptr(errbuf)
                        .to_string_lossy()
                        .into_owned();
                    sandbox_free_error(errbuf);
                    msg
                };
                return Err(std::io::Error::other(format!(
                    "seatbelt sandbox_init failed: {msg}"
                )));
            }
            Ok(())
        });
    }
    Ok(())
}

/// 生成 Seatbelt profile（Scheme DSL）。
///
/// - `ReadOnly`：全盘只读，仅允许 exec 系统路径；
/// - `WorkspaceWrite`：workdir + writable 可写，其余只读，VCS 目录保护。
///
/// ## 读权限模型（SEC-4，2026-08-27 R5 审查）
///
/// 此前的 `(allow file-read*)` 让沙箱化 `shell.run` 子进程可读取
/// `~/.ssh/id_rsa`、`~/.aws/credentials` 等全部凭证并复制到可写 workdir
/// （Linux A3 已封此通道，macOS 未移植）。现改为 deny-first + 白名单：
/// 系统路径与 workdir 显式放行，`$HOME` 默认拒绝读，仅放行工具链/缓存白名单
/// （[`crate::hardening::home_read_allow_paths`]），白名单内凭证高危落点
/// （gh/gcloud/cargo credentials）尾部显式 deny 覆盖。Seatbelt 规则
/// **最后匹配者优先**——`deny $HOME` 必须置于系统路径 allow 之后，
/// workdir 允许必须置于 `deny $HOME` 之后（workdir 常见于 HOME 内）。
fn build_profile(policy: &SandboxPolicy) -> std::io::Result<String> {
    let mut p = String::new();

    // 基础：允许 exec 系统路径、允许必要系统操作。
    p.push_str("(allow process-exec (subpath \"/usr/bin\") (subpath \"/bin\") (subpath \"/usr/sbin\") (subpath \"/sbin\") (subpath \"/usr/libexec\") (subpath \"/usr/local/bin\") (subpath \"/opt/homebrew/bin\"))\n");
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow signal)\n");
    p.push_str("(allow sysctl-read)\n");
    p.push_str("(allow ipc-posix-sem)\n");
    p.push_str("(allow ipc-posix-shm)\n");
    p.push_str("(allow mach-lookup)\n");
    // 网络隔离（2026-08-23 审查遗留#1，security.md §8 核心支柱）：
    // 不再 (allow network*)——Seatbelt 默认拒绝未 allow 的操作，子进程
    // TCP/UDP 全部被拦。web.fetch 在主进程内执行不受影响；需要子进程
    // 联网的场景用 external-sandbox（不套 profile）/ danger-full-access。
    p.push_str("(deny network*)\n");

    // 写权限：默认拒绝
    p.push_str("(deny file-write*)\n");

    // 读权限（SEC-4）：系统路径显式放行（在 deny $HOME 之前，最后匹配优先）
    p.push_str("(allow file-read* (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/etc\") (subpath \"/private\") (subpath \"/dev\") (subpath \"/var\") (subpath \"/tmp\") (subpath \"/opt\") (subpath \"/Library\") (subpath \"/Applications\"))\n");

    // 凭证目录读保护（SEC-4）：$HOME 默认拒绝读，仅白名单目录放行
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let home = home.to_string_lossy();
        p.push_str(&format!(
            "(deny file-read* (subpath \"{}\"))\n",
            seatbelt_escape(&home)?
        ));
        // 工具链/缓存白名单（与 Linux A3 同语义；存在性过滤）
        for w in crate::hardening::home_read_allow_paths() {
            p.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                seatbelt_escape(&w.to_string_lossy())?
            ));
        }
        // 白名单内凭证高危落点尾部 deny（最后匹配优先，覆盖白名单 allow）
        for cred in crate::hardening::credential_dir_deny_paths() {
            p.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                seatbelt_escape(&cred.to_string_lossy())?
            ));
        }
    }

    match policy {
        SandboxPolicy::ReadOnly => {
            // ReadOnly：仅 workdir 可读（在 deny $HOME 之后放行，workdir 常见于 HOME 内）
            // 注意：ReadOnly 策略无 workdir 字段，全盘系统路径 + 白名单已覆盖可读面
        }
        SandboxPolicy::WorkspaceWrite { workdir, writable } => {
            // workdir 可读（在 deny $HOME 之后——workdir 常位于 HOME 内，
            // 最后匹配优先保证 workdir 读不被 HOME deny 覆盖）
            p.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                seatbelt_escape(workdir.as_str())?
            ));
            // 额外 writable 目录可读写
            for w in writable {
                p.push_str(&format!(
                    "(allow file-read* (subpath \"{}\"))\n",
                    seatbelt_escape(w.as_str())?
                ));
                p.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    seatbelt_escape(w.as_str())?
                ));
            }
            // workdir 可写（路径经 seatbelt_escape 防元字符注入）
            p.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                seatbelt_escape(workdir.as_str())?
            ));
            // VCS 目录写保护（deny 最后匹配优先，确保 .git 等不可写）
            let vcs_dirs = vcs_protected_dirs(workdir.as_std_path());
            for vcs in vcs_dirs {
                p.push_str(&format!(
                    "(deny file-write* (subpath \"{}\"))\n",
                    seatbelt_escape(&vcs.to_string_lossy())?
                ));
            }
        }
        SandboxPolicy::ExternalSandbox | SandboxPolicy::DangerFullAccess => {
            // 不应到达此处（apply 已提前返回 Ok）
        }
    }

    Ok(p)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn driver_id_and_hardened() {
        let d = SeatbeltDriver::new();
        assert_eq!(d.id(), "seatbelt");
        assert!(d.is_hardened());
    }

    #[test]
    fn seatbelt_available_is_true() {
        assert!(seatbelt_available());
    }

    #[test]
    fn external_and_full_access_are_noop() {
        let d = SeatbeltDriver::new();
        let mut cmd = std::process::Command::new("true");
        d.apply(&SandboxPolicy::ExternalSandbox, &mut cmd).unwrap();
        d.apply(&SandboxPolicy::DangerFullAccess, &mut cmd).unwrap();
    }

    #[test]
    fn build_profile_readonly_denies_writes() {
        let p = build_profile(&SandboxPolicy::ReadOnly).expect("profile");
        assert!(p.contains("(deny file-write*)"));
        // SEC-4：读为 deny-first + 系统路径 subpath 白名单（非裸 allow file-read*）
        assert!(
            p.contains("(allow file-read* (subpath \""),
            "profile should allow read on system paths, got: {}",
            p
        );
        // 凭证目录必须显式 deny（SEC-4：$HOME 默认拒绝读）
        assert!(
            p.contains("(deny file-read*"),
            "profile should deny reads on credentials, got: {}",
            p
        );
        // ReadOnly 不应放行任何写路径
        assert!(!p.contains("(allow file-write*"));
    }

    #[test]
    fn build_profile_workspace_write_allows_workdir() {
        use camino::Utf8PathBuf;
        let p = build_profile(&SandboxPolicy::WorkspaceWrite {
            workdir: Utf8PathBuf::from("/tmp/test-workdir"),
            writable: vec![Utf8PathBuf::from("/tmp/test-extra")],
        })
        .expect("profile");
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/test-workdir\"))"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/test-extra\"))"));
    }
}

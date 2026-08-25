//! A1 seccomp deny-list 集成测试（feature gate `seccomp`，仅 Linux）。
//!
//! 验证经 `LandlockDriver::apply`（内含 `pre_exec` seccomp 加载，A1）spawn 的
//! 子进程：危险 syscall（`unshare`）被 `EPERM` 拒绝、正常命令不受影响。
#![cfg(all(target_os = "linux", feature = "seccomp"))]

use minicoding_core::sandbox::{SandboxDriver, SandboxPolicy};
use std::process::Command;

/// `WorkspaceWrite` 默认形态的测试策略。
fn test_policy() -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        workdir: camino::Utf8PathBuf::from("."),
        writable: Vec::new(),
    }
}

#[test]
fn dangerous_syscall_denied_with_eperm() {
    if !minicoding_sandbox::linux::landlock_available() {
        // 无 Landlock 的旧内核走 NoopDriver 降级路径，本测试不适用
        eprintln!("landlock unavailable, skipping");
        return;
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("unshare -p true");
    minicoding_sandbox::linux::LandlockDriver::new()
        .apply(&test_policy(), &mut cmd)
        .expect("apply should succeed");

    let output = cmd.output().expect("spawn child");
    assert!(
        !output.status.success(),
        "`unshare -p true` 应被 seccomp EPERM 拒绝"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Operation not permitted"),
        "失败应源于 EPERM 而非其他错误，stderr: {stderr}"
    );
}

#[test]
fn benign_command_still_succeeds() {
    if !minicoding_sandbox::linux::landlock_available() {
        eprintln!("landlock unavailable, skipping");
        return;
    }
    // deny-list 策略不破坏正常命令：默认 Allow 语义下 true 正常执行
    let mut cmd = Command::new("true");
    minicoding_sandbox::linux::LandlockDriver::new()
        .apply(&test_policy(), &mut cmd)
        .expect("apply should succeed");

    let status = cmd.status().expect("spawn child");
    assert!(status.success(), "正常命令不应受 deny-list 影响");
}

//! `HookInput`/`HookOutput` JSON 协议 + 退出码映射（见 `hooks.md` §3）。
//!
//! 协议：外部可执行通过 stdin 收到单行 `HookInput` JSON，处理后从 stdout 输出单行
//! `HookOutput` JSON。退出码语义：
//! - `0`：输出有效 JSON，按 `decision` 处理；
//! - `2`：阻断（等价 `decision=deny`，reason 取 stderr）；
//! - 其他：Hook 错误，按 `on_hook_error` 策略处理（默认 `continue` + warn）。
//!
//! `ScriptHook` 调用本模块完成 JSON 序列化/反序列化与退出码映射。

use minicoding_core::hooks::{HookError, HookInput, HookOutput};

/// 退出码常量（见 `hooks.md` §3.3）。
pub const EXIT_OK: i32 = 0;
/// 退出码 2：阻断（deny），reason 取 stderr。
pub const EXIT_DENY: i32 = 2;

/// 把 `HookInput` 序列化为单行 JSON（写入子进程 stdin）。
///
/// # Errors
/// 序列化失败时返回 `HookError::InvalidOutput`（理论不可达，`HookInput` 派生 `Serialize`）。
pub fn encode_input(input: &HookInput) -> Result<String, HookError> {
    serde_json::to_string(input).map_err(|e| HookError::InvalidOutput {
        name: String::new(),
        reason: format!("encode HookInput failed: {e}"),
    })
}

/// 解析子进程 stdout 为 `HookOutput`。
///
/// stdout 应为单行 JSON。解析失败返回 `InvalidOutput` 错误（按 `on_hook_error` 处理）。
///
/// # Errors
/// - `InvalidOutput`：stdout 非合法 JSON 或不符合 `HookOutput` schema。
pub fn decode_output(stdout: &str, hook_name: &str) -> Result<HookOutput, HookError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // 空输出视为 `Continue`（不干预），便于简单脚本只 echo decision
        return Ok(HookOutput::continue_());
    }
    serde_json::from_str(trimmed).map_err(|e| HookError::InvalidOutput {
        name: hook_name.to_string(),
        reason: format!("decode HookOutput failed: {e}"),
    })
}

/// 按退出码映射子进程结果（见 `hooks.md` §3.3）。
///
/// - `0`：解析 stdout 为 `HookOutput`（`Ok`）；
/// - `2`：阻断，返回 `Ok(HookOutput { decision: Deny, reason: stderr })`；
/// - 其他：返回 `Err(ExitCode { code, stderr })`，由 `on_hook_error` 处理。
///
/// # Errors
/// 退出码非 0/2 时返回 `HookError::ExitCode`。
pub fn map_exit_code(
    code: i32,
    stdout: &str,
    stderr: &str,
    hook_name: &str,
) -> Result<HookOutput, HookError> {
    if code == EXIT_OK {
        decode_output(stdout, hook_name)
    } else if code == EXIT_DENY {
        // 退出码 2 = deny，reason 取 stderr（trimmed）
        Ok(HookOutput {
            decision: minicoding_core::hooks::HookDecision::Deny,
            reason: Some(stderr.trim().to_string()),
            ..HookOutput::default()
        })
    } else {
        Err(HookError::ExitCode {
            name: hook_name.to_string(),
            code,
            stderr: stderr.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::hooks::{HookDecision, HookEvent};

    #[test]
    fn encode_input_roundtrip() {
        let input = HookInput::new(HookEvent::SessionStart, "s1", 1, Utf8PathBuf::from("/tmp"));
        let json = encode_input(&input).expect("encode");
        assert!(json.contains("SessionStart"));
        assert!(!json.contains('\n')); // 单行
    }

    #[test]
    fn decode_output_valid() {
        let json = r#"{"decision":"allow","reason":"ok"}"#;
        let out = decode_output(json, "h").expect("decode");
        assert_eq!(out.decision, HookDecision::Allow);
        assert_eq!(out.reason.as_deref(), Some("ok"));
    }

    #[test]
    fn decode_output_empty_is_continue() {
        let out = decode_output("", "h").expect("decode");
        assert_eq!(out.decision, HookDecision::Continue);
    }

    #[test]
    fn decode_output_invalid_json_errors() {
        let result = decode_output("not json", "h");
        assert!(result.is_err());
    }

    #[test]
    fn map_exit_code_zero_parses_output() {
        let result = map_exit_code(0, r#"{"decision":"continue"}"#, "", "h");
        let out = result.expect("ok");
        assert_eq!(out.decision, HookDecision::Continue);
    }

    #[test]
    fn map_exit_code_two_is_deny() {
        let result = map_exit_code(2, "", "blocked by policy\n", "h");
        let out = result.expect("deny is Ok");
        assert_eq!(out.decision, HookDecision::Deny);
        assert_eq!(out.reason.as_deref(), Some("blocked by policy"));
    }

    #[test]
    fn map_exit_code_other_is_error() {
        let result = map_exit_code(1, "", "crashed", "h");
        let err = result.expect_err("non-zero/2 is error");
        match err {
            HookError::ExitCode { name, code, stderr } => {
                assert_eq!(name, "h");
                assert_eq!(code, 1);
                assert_eq!(stderr, "crashed");
            }
            _ => panic!("expected ExitCode error"),
        }
    }
}

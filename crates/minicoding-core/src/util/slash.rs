//! 斜杠命令解析（F3，TUI/CLI 共享）。
//!
//! 单一事实来源：命令名与参数形态在此定义，`minicoding-cli` 与
//! `minicoding-tui` 共用同一 [`parse`] 入口，避免前端各自漂移。
//! 纯函数、零 IO——解析与执行分离：前端拿到 [`SlashCommand`] 后按自身
//! 能力分派（不可达的能力诚实降级提示，见 TUI app.rs）。

use std::fmt;

/// 斜杠命令（解析产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// `/help`：显示命令列表。
    Help,
    /// `/model [name]`：无参查看当前模型，带参切换模型。
    Model(Option<String>),
    /// `/status`：会话状态摘要。
    Status,
    /// `/tokens`：会话 token 计量。
    Tokens,
    /// `/clear`：清空显示（会话上下文不动）。
    Clear,
    /// `/undo [steps]`：回滚最近 `steps` 次文件改动 operation（缺省/非法值取 1）。
    Undo { steps: usize },
    /// `/plan`：切换 Plan 模式（CLI 另支持 `on|off|status` 子命令，由前端自行扩展）。
    PlanToggle,
    /// `/summary`：生成并展示会话摘要（R8：TUI/CLI 支持，走 `Runtime::summarize_session`）。
    Summary,
    /// 未识别的斜杠命令，携带命令名（`"/"` 单独输入时为空串）。
    Unknown(String),
}

impl fmt::Display for SlashCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Help => f.write_str("/help"),
            Self::Model(arg) => match arg {
                Some(name) => write!(f, "/model {name}"),
                None => f.write_str("/model"),
            },
            Self::Status => f.write_str("/status"),
            Self::Tokens => f.write_str("/tokens"),
            Self::Clear => f.write_str("/clear"),
            Self::Undo { steps } => write!(f, "/undo {steps}"),
            Self::PlanToggle => f.write_str("/plan"),
            Self::Summary => f.write_str("/summary"),
            Self::Unknown(name) => write!(f, "/{name}"),
        }
    }
}

/// 解析一行用户输入为斜杠命令。
///
/// - 返回 `None`：非斜杠输入（trim 后不以 `/` 开头），应作为普通消息发送；
/// - 前导/尾随空白容忍（先 trim，与 CLI REPL 行为一致）；
/// - 命令名区分大小写；多余参数忽略；
/// - `/undo` 的 steps 解析失败或缺省时取 1，解析值下限钳到 1（0 无语义）；
/// - `"/"` 或仅空白的斜杠输入返回 [`SlashCommand::Unknown`]（空命令名），
///   由前端给出反馈而非当作消息发送。
#[must_use]
pub fn parse(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    match parts.next() {
        None => Some(SlashCommand::Unknown(String::new())),
        Some("help") => Some(SlashCommand::Help),
        Some("model") => Some(SlashCommand::Model(parts.next().map(str::to_owned))),
        Some("status") => Some(SlashCommand::Status),
        Some("tokens") => Some(SlashCommand::Tokens),
        Some("clear") => Some(SlashCommand::Clear),
        Some("undo") => {
            // 缺省/非法均取 1：宽容语义优于报错（与 REPL 快速回滚的使用直觉一致）
            let steps = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            Some(SlashCommand::Undo { steps })
        }
        Some("plan") => Some(SlashCommand::PlanToggle),
        Some("summary") => Some(SlashCommand::Summary),
        Some(name) => Some(SlashCommand::Unknown(name.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_slash_input_returns_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("  hello world "), None);
        // 斜杠在中间不算命令
        assert_eq!(parse("a/b"), None);
    }

    #[test]
    fn simple_commands_without_args() {
        assert_eq!(parse("/help"), Some(SlashCommand::Help));
        assert_eq!(parse("/status"), Some(SlashCommand::Status));
        assert_eq!(parse("/tokens"), Some(SlashCommand::Tokens));
        assert_eq!(parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(parse("/plan"), Some(SlashCommand::PlanToggle));
    }

    #[test]
    fn leading_and_trailing_whitespace_tolerated() {
        assert_eq!(parse("  /help"), Some(SlashCommand::Help));
        assert_eq!(parse("/help  "), Some(SlashCommand::Help));
        assert_eq!(parse("\t/status\n"), Some(SlashCommand::Status));
    }

    #[test]
    fn model_with_and_without_arg() {
        assert_eq!(parse("/model"), Some(SlashCommand::Model(None)));
        assert_eq!(
            parse("/model gpt-4o"),
            Some(SlashCommand::Model(Some("gpt-4o".to_string())))
        );
        // 多余参数忽略（取第一个 token，与 CLI 行为一致）
        assert_eq!(
            parse("/model claude extra"),
            Some(SlashCommand::Model(Some("claude".to_string())))
        );
    }

    #[test]
    fn undo_steps_default_one_and_clamped() {
        assert_eq!(parse("/undo"), Some(SlashCommand::Undo { steps: 1 }));
        assert_eq!(parse("/undo 3"), Some(SlashCommand::Undo { steps: 3 }));
        // 非法数字宽容回退 1
        assert_eq!(parse("/undo abc"), Some(SlashCommand::Undo { steps: 1 }));
        // 0 无语义，钳到 1
        assert_eq!(parse("/undo 0"), Some(SlashCommand::Undo { steps: 1 }));
        // usize 溢出同样回退 1
        assert_eq!(
            parse("/undo 99999999999999999999999"),
            Some(SlashCommand::Undo { steps: 1 })
        );
    }

    #[test]
    fn unknown_command_carries_name() {
        assert_eq!(
            parse("/foobar"),
            Some(SlashCommand::Unknown("foobar".to_string()))
        );
        // 带参数的未知命令只保留命令名
        assert_eq!(
            parse("/foo bar baz"),
            Some(SlashCommand::Unknown("foo".to_string()))
        );
    }

    #[test]
    fn bare_slash_is_unknown_empty() {
        assert_eq!(parse("/"), Some(SlashCommand::Unknown(String::new())));
        assert_eq!(parse("  /   "), Some(SlashCommand::Unknown(String::new())));
    }

    #[test]
    fn case_sensitive_names() {
        // 与既有 CLI 行为一致：命令名区分大小写
        assert_eq!(
            parse("/Help"),
            Some(SlashCommand::Unknown("Help".to_string()))
        );
    }

    #[test]
    fn extra_args_on_flag_commands_ignored() {
        assert_eq!(parse("/help me now"), Some(SlashCommand::Help));
        assert_eq!(parse("/clear all"), Some(SlashCommand::Clear));
        assert_eq!(parse("/plan on"), Some(SlashCommand::PlanToggle));
    }

    #[test]
    fn display_roundtrip_shape() {
        assert_eq!(SlashCommand::Help.to_string(), "/help");
        assert_eq!(
            SlashCommand::Model(Some("m".to_string())).to_string(),
            "/model m"
        );
        assert_eq!(SlashCommand::Model(None).to_string(), "/model");
        assert_eq!(SlashCommand::Undo { steps: 2 }.to_string(), "/undo 2");
        assert_eq!(SlashCommand::Unknown("x".to_string()).to_string(), "/x");
    }
}

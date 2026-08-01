//! 会话导出：Markdown / JSONL。
//!
//! 设计意图（见 `docs/features.md` S-04、`rules.md` C-04）：
//! - **不含凭证**：导出仅转录已入消息流的 `Message`，凭证由工具层保证不入消息
//!   （见 AGENTS.md §5.3、`rules.md` C-04），导出层不做额外过滤；
//! - **Markdown**：人类可读，按角色分节，标注时间戳；
//! - **JSONL**：每行一条 `Message`（与 `.jsonl` 存储格式一致），便于回灌或迁移。

use minicoding_core::model::{Message, Role};
use minicoding_core::storage::SessionMeta;
use std::fmt::Write as _;
use time::format_description::well_known::Rfc3339;

/// 导出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Markdown（人类可读）。
    Markdown,
    /// JSONL（每行一条 `Message`）。
    Jsonl,
}

/// 导出会话为 Markdown。
///
/// 格式：标题含会话 ID 与元数据，正文按消息顺序逐条分节（角色 + 时间 + 文本）。
/// 非文本块（图片/工具调用/工具结果）以占位标记表示，避免泄露 base64 数据。
#[must_use]
pub fn export_session_md(messages: &[Message], meta: &SessionMeta) -> String {
    let created = meta.created_at.format(&Rfc3339).unwrap_or_default();
    let last = meta.last_message_at.format(&Rfc3339).unwrap_or_default();
    let mut out = String::new();
    let _ = write!(
        out,
        "# Session {}\n\n- Created: {}\n- Messages: {}\n- Last activity: {}\n\n---\n\n",
        meta.id, created, meta.message_count, last,
    );
    for msg in messages {
        let role = role_label(&msg.role);
        let ts = msg.created_at.format(&Rfc3339).unwrap_or_default();
        let _ = write!(out, "## {role}  ({ts})\n\n");
        let text = msg.text();
        if text.is_empty() {
            // 含非文本块时标注，不输出 base64/工具原始 payload
            let has_non_text = msg
                .content
                .iter()
                .any(|b| !matches!(b, minicoding_core::model::ContentBlock::Text { .. }));
            if has_non_text {
                out.push_str("_(non-text content blocks omitted)_\n\n");
            } else {
                out.push_str("_(empty)_\n\n");
            }
        } else {
            out.push_str(&text);
            out.push_str("\n\n");
        }
        if !msg.tool_calls.is_empty() {
            let names: Vec<&str> = msg.tool_calls.iter().map(|c| c.name.as_str()).collect();
            let _ = write!(out, "_(tool calls: {})_\n\n", names.join(", "));
        }
        out.push_str("---\n\n");
    }
    out
}

/// 导出会话为 JSONL（每行一条 `Message` 的 JSON 序列化）。
///
/// 与 `.jsonl` 存储格式一致，便于跨实例迁移或回灌。凭证过滤由工具层保证
/// （见 `rules.md` C-04），导出层不额外处理。
#[must_use]
pub fn export_session_jsonl(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| serde_json::to_string(m).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 角色的人类可读标签。
fn role_label(role: &Role) -> &'static str {
    match role {
        Role::System => "System",
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::Tool => "Tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::Message;
    use time::OffsetDateTime;

    fn sample_meta() -> SessionMeta {
        SessionMeta {
            id: "01TEST".to_string(),
            created_at: OffsetDateTime::now_utc(),
            message_count: 2,
            last_message_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn export_md_contains_role_and_text() {
        let msgs = vec![
            Message::user_text("hello world"),
            Message::assistant_text("hi there"),
        ];
        let out = export_session_md(&msgs, &sample_meta());
        assert!(out.contains("# Session 01TEST"));
        assert!(out.contains("## User"));
        assert!(out.contains("hello world"));
        assert!(out.contains("## Assistant"));
        assert!(out.contains("hi there"));
    }

    #[test]
    fn export_jsonl_one_line_per_message() {
        let msgs = vec![Message::user_text("a"), Message::assistant_text("b")];
        let out = export_session_jsonl(&msgs);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        // 每行应是合法 JSON
        for line in &lines {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }

    #[test]
    fn export_md_empty_messages_renders_header() {
        let out = export_session_md(&[], &sample_meta());
        assert!(out.contains("# Session 01TEST"));
        assert!(out.contains("Messages: 2"));
    }
}

//! Command DTO（前端→后端命令）。
//!
//! 对应 `design.md` §24 的 `Request` 枚举：`CreateSession`/`SendUserMessage`/
//! `Cancel`/`Undo`/`ListSessions`/`GetSession`/`SetPermissionMode`/
//! `ResolvePermission`。与 Runtime 方法一一对应。

use minicoding_core::model::{Attachment, SessionId};
use minicoding_core::policy::{Decision, PermissionMode};
use serde::{Deserialize, Serialize};

/// 会话配置（创建会话时传入）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    /// 工作目录（默认当前目录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// 系统 prompt 覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Provider 覆盖（如 `"openai"`/`"anthropic"`/`"ollama"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 模型覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 初始权限模式（默认 `Default`）。
    #[serde(default)]
    pub permission_mode: PermissionMode,
}

/// 前端→后端命令（JSON-RPC method 参数）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// 创建会话。
    CreateSession { config: SessionConfig },
    /// 发送用户消息。
    SendUserMessage {
        session_id: SessionId,
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    /// 取消当前 turn。
    Cancel { session_id: SessionId },
    /// 撤销文件改动。
    Undo { session_id: SessionId, steps: usize },
    /// 列出会话。
    ListSessions,
    /// 获取会话详情（含消息快照）。
    GetSession { session_id: SessionId },
    /// 设置权限模式。
    SetPermissionMode {
        session_id: SessionId,
        mode: PermissionMode,
    },
    /// 解析权限请求（用户已决策）。
    ResolvePermission { id: String, decision: Decision },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn create_session_command() {
        let cmd = Command::CreateSession {
            config: SessionConfig::default(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"command\":\"create_session\""));
    }

    #[test]
    fn send_message_roundtrip() {
        let cmd = Command::SendUserMessage {
            session_id: "01JTEST".into(),
            text: "hello".into(),
            attachments: vec![],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        match back {
            Command::SendUserMessage { text, .. } => assert_eq!(text, "hello"),
            _ => panic!("wrong variant"),
        }
    }
}

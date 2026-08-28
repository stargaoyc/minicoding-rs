//! JSON-RPC 2.0 wire types（请求/响应/通知/错误）。
//!
//! 与 LSP 协议风格一致，便于复用既有 LSP 客户端库。所有消息共享 `jsonrpc: "2.0"`
//! 版本字段。`Id` 支持 `Number` 与 `String`（LSP 客户端常用 `Number`）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 版本（固定 `"2.0"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(
        export,
        export_to = "../../minicoding-web/src/api/generated/",
        type = "\"2.0\""
    )
)]
pub struct Version;

impl Default for Version {
    fn default() -> Self {
        Self
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s: &str = Deserialize::deserialize(deserializer)?;
        if s == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected `\"2.0\"`"))
        }
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

/// 请求 ID（Number 或 String，Notification 无 ID）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(untagged)]
pub enum Id {
    /// 数字 ID（LSP 客户端常用）。
    Number(u64),
    /// 字符串 ID。
    String(String),
}

/// JSON-RPC 请求（含 ID，需响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 通知（无 ID，不需响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 响应。
///
/// JSON-RPC 2.0 规范：响应必须且只能含 `result` 或 `error` 二者之一。
/// FE-R6-3（2026-08-28 R6 审查）：此前派生 Deserialize 接受 result/error
/// 同缺或同在的非法形态——自定义反序列化校验，非法形态报错（fail-closed）。
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Response {
    pub jsonrpc: Version,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        struct Raw {
            jsonrpc: Version,
            id: Id,
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error: Option<Error>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (raw.result, raw.error) {
            (Some(_), Some(_)) => Err(D::Error::custom(
                "JSON-RPC Response 不得同时含 result 与 error",
            )),
            (None, None) => Err(D::Error::custom(
                "JSON-RPC Response 必须含 result 或 error 之一",
            )),
            (result, error) => Ok(Response {
                jsonrpc: raw.jsonrpc,
                id: raw.id,
                result,
                error,
            }),
        }
    }
}

impl Response {
    /// 构造成功响应。
    #[must_use]
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// 构造错误响应。
    #[must_use]
    pub fn err(id: Id, error: Error) -> Self {
        Self {
            jsonrpc: Version,
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Error {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    /// Parse error（-32700）。
    #[must_use]
    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self::new(-32700, msg)
    }

    /// Invalid request（-32600）。
    #[must_use]
    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(-32600, msg)
    }

    /// Method not found（-32601）。
    #[must_use]
    pub fn method_not_found(msg: impl Into<String>) -> Self {
        Self::new(-32601, msg)
    }

    /// Invalid params（-32602）。
    #[must_use]
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }

    /// Internal error（-32603）。
    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(-32603, msg)
    }

    /// 构造自定义错误。
    #[must_use]
    pub fn new(code: i32, msg: impl Into<String>) -> Self {
        Self {
            code,
            message: msg.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn request_serialization() {
        let req = Request {
            jsonrpc: Version,
            id: Id::Number(1),
            method: "minicoding.ask".to_string(),
            params: Some(serde_json::json!({"text": "hello"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"minicoding.ask\""));
    }

    #[test]
    fn notification_has_no_id() {
        let notif = Notification {
            jsonrpc: Version,
            method: "minicoding/event".to_string(),
            params: Some(serde_json::json!({"seq": 42})),
        };
        let json = serde_json::to_string(&notif).unwrap();
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn response_ok_roundtrip() {
        let resp = Response::ok(
            Id::Number(1),
            serde_json::json!({"stop_reason": "end_turn"}),
        );
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(back.error.is_none());
        assert!(back.result.is_some());
    }

    #[test]
    fn response_err_roundtrip() {
        let resp = Response::err(Id::String("abc".into()), Error::method_not_found("nope"));
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(back.result.is_none());
        assert_eq!(back.error.unwrap().code, -32601);
    }

    #[test]
    fn version_rejects_non_2_0() {
        let result: Result<Version, _> = serde_json::from_str("\"1.0\"");
        assert!(result.is_err());
    }

    #[test]
    fn response_rejects_both_result_and_error() {
        // FE-R6-3（2026-08-28 R6 审查）：JSON-RPC 2.0 要求 result/error 二选一，
        // 二者同在是非法形态——此前派生反序列化接受，解析错误路径的响应
        // 会静默取其一，掩盖协议错误。
        let json =
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true},"error":{"code":-1,"message":"x"}}"#;
        let result: Result<Response, _> = serde_json::from_str(json);
        assert!(result.is_err(), "result+error 同在必须拒绝");
    }

    #[test]
    fn response_rejects_neither_result_nor_error() {
        let json = r#"{"jsonrpc":"2.0","id":1}"#;
        let result: Result<Response, _> = serde_json::from_str(json);
        assert!(result.is_err(), "result+error 同缺必须拒绝");
    }

    #[test]
    fn response_accepts_valid_shapes() {
        let ok_json = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        assert!(serde_json::from_str::<Response>(ok_json).is_ok());
        let err_json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#;
        assert!(serde_json::from_str::<Response>(err_json).is_ok());
    }
}

//! E2E 端到端测试（R9 CI-2/E2E 框架，跨组件基础设施）。
//!
//! 起**真实** `minicoding-server` 二进制进程 + wiremock mock LLM 端点，
//! 通过 HTTP/SSE 驱动完整会话闭环：
//!
//! 1. 创建会话（`POST /sessions`）
//! 2. 发送消息（`POST /sessions/{id}/messages`，202 Accepted）
//! 3. 订阅 SSE 事件流（`GET /sessions/{id}/events`）
//! 4. 断言收到 `turn_streaming_started` → `token`/`message_appended` → `turn_end`
//! 5. 拉取消息快照验证落盘
//!
//! ## 两种 LLM 模式
//!
//! - **默认（CI 安全）**：wiremock 模拟 `OpenAI` 兼容 `/chat/completions`
//!   SSE 端点，返回脚本化响应。不连真实服务（AGENTS.md §5.4）。
//! - **真实 LLM（env 门控）**：设置 `MINICODING_E2E_REAL=1` 且提供
//!   `OPENAI_API_KEY`/`OPENAI_API_BASE`/`OPENAI_MODEL` 时，跳过 wiremock
//!   直接连真实 API（本地人工验证用，CI 不设置即跳过）。

use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 测试专用 mock 模型名。
const MOCK_MODEL: &str = "e2e-mock-model";

/// E2E server 句柄：持有子进程与端口，Drop 时回收。
struct E2eServer {
    child: tokio::process::Child,
    port: u16,
}

/// 启动真实 `minicoding-server` 二进制（`CARGO_BIN_EXE_minicoding-server`）。
///
/// 真实 LLM 模式（env 门控）时透传真实 `api_key`/`model`，否则用 mock 值。
async fn start_server(api_base: &str, workdir: &str) -> E2eServer {
    let bin = env!("CARGO_BIN_EXE_minicoding-server");
    let (api_key, model) = if real_llm_enabled() {
        (
            std::env::var("OPENAI_API_KEY").expect("真实模式需 OPENAI_API_KEY"),
            std::env::var("OPENAI_MODEL").unwrap_or_else(|_| MOCK_MODEL.to_string()),
        )
    } else {
        ("sk-test-e2e".to_string(), MOCK_MODEL.to_string())
    };
    let mut child = Command::new(bin)
        .args([
            "--bind",
            "127.0.0.1:0",
            "--no-auth",
            "--provider",
            "openai",
            "--api-base",
            api_base,
            "--api-key",
            &api_key,
            "--model",
            &model,
            "--workdir",
            workdir,
            "--permission-timeout-sec",
            "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn minicoding-server");

    let stdout = child.stdout.take().expect("server stdout");
    let mut lines = BufReader::new(stdout).lines();
    let port = timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.expect("read server stdout") {
            if let Some(p) = line.strip_prefix("MINICODING_LISTENING_PORT=") {
                return p.parse::<u16>().expect("parse port");
            }
        }
        panic!("server 未输出 MINICODING_LISTENING_PORT，进程可能提前退出");
    })
    .await
    .expect("等待 server 启动超时");

    // 继续消费 stdout 防止管道阻塞
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    E2eServer { child, port }
}

impl Drop for E2eServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// 是否启用真实 LLM E2E（env 门控）。
fn real_llm_enabled() -> bool {
    std::env::var("MINICODING_E2E_REAL").is_ok_and(|v| v == "1")
        && std::env::var("OPENAI_API_KEY").is_ok()
}

/// mock LLM 的 SSE 事件行包装（与 providers 测试同格式）。
fn sse_event(value: &serde_json::Value) -> String {
    format!("data: {value}\n\n")
}

/// 注册脚本化 LLM 响应：返回一段文本（不触发工具调用，E2E 最简闭环）。
async fn mount_text_reply(mock: &MockServer, text: &str) {
    let chunk = json!({
        "id": "e2e-1",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    let done = json!({
        "id": "e2e-1",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let sse = format!("{}{}data: [DONE]\n\n", sse_event(&chunk), sse_event(&done));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(mock)
        .await;
}

/// 注册两轮脚本化 LLM：第一轮返回工具调用 fs.read，第二轮返回文本。
/// 用两个 `Mock` 实例 + 不同优先级确保顺序。
async fn mount_tool_then_text_reply(mock: &MockServer, text: &str) {
    let tool_chunk = json!({
        "id": "e2e-2",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "id": "call_e2e_read",
            "type": "function",
            "function": {"name": "fs.read", "arguments": "{\"path\":\"e2e.txt\"}"}
        }]}, "finish_reason": null}]
    });
    let text_chunk = json!({
        "id": "e2e-2",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    let done = json!({
        "id": "e2e-2",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let tool_sse = format!(
        "{}{}data: [DONE]\n\n",
        sse_event(&tool_chunk),
        sse_event(&done)
    );
    let text_sse = format!(
        "{}{}data: [DONE]\n\n",
        sse_event(&text_chunk),
        sse_event(&done)
    );

    // 第一轮：匹配第一个请求 → 工具调用（`up_to_n_times(1)` 消费后失效）
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(tool_sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(mock)
        .await;

    // 第二轮：匹配后续请求 → 文本回复（第一轮消费后生效）
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(text_sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(mock)
        .await;
}

/// 创建会话（可选 `permission_mode` 与 `confirm_danger`）。
async fn create_session_full(
    base_url: &str,
    workdir: &str,
    permission_mode: Option<&str>,
    confirm_danger: Option<bool>,
) -> String {
    let client = reqwest::Client::new();
    let model = if real_llm_enabled() {
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| MOCK_MODEL.to_string())
    } else {
        MOCK_MODEL.to_string()
    };
    let mut body = json!({"workdir": workdir, "model": model});
    if let Some(mode) = permission_mode {
        body["permission_mode"] = json!(mode);
    }
    if let Some(confirm) = confirm_danger {
        body["confirm_danger"] = json!(confirm);
    }
    let resp = client
        .post(format!("{base_url}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("POST /sessions");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "创建会话应 200: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    body["session_id"].as_str().expect("session_id").to_string()
}

/// 创建会话（默认权限模式）。
async fn create_session(base_url: &str, workdir: &str) -> String {
    create_session_full(base_url, workdir, None, None).await
}

/// 注册三步脚本化 LLM：写 main.rs → 写 Cargo.toml → 最终文本（完整项目场景）。
///
/// 用 `with_priority` + `up_to_n_times(1)` 保证三步顺序；wiremock 0.6 中
/// 1 为最高优先级，255 最低，默认 5；相同时按插入顺序。
async fn mount_full_project_script(mock: &MockServer, final_text: &str) {
    let main_rs = json!({
        "id": "e2e-proj",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_write_main",
                "type": "function",
                "function": {
                    "name": "fs.write",
                    "arguments": "{\"path\":\"main.rs\",\"content\":\"fn main() { println!(\\\"hello\\\"); }\\n\"}"
                }
            }]},
            "finish_reason": null
        }]
    });
    let cargo_toml = json!({
        "id": "e2e-proj",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_write_cargo",
                "type": "function",
                "function": {
                    "name": "fs.write",
                    "arguments": "{\"path\":\"Cargo.toml\",\"content\":\"[package]\\nname = \\\"e2e-proj\\\"\\nversion = \\\"0.1.0\\\"\\nedition = \\\"2021\\\"\\n\"}"
                }
            }]},
            "finish_reason": null
        }]
    });
    let text_chunk = json!({
        "id": "e2e-proj", "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": final_text}, "finish_reason": null}]
    });
    let done = json!({
        "id": "e2e-proj", "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let step1_sse = format!(
        "{}{}data: [DONE]\n\n",
        sse_event(&main_rs),
        sse_event(&done)
    );
    let step2_sse = format!(
        "{}{}data: [DONE]\n\n",
        sse_event(&cargo_toml),
        sse_event(&done)
    );
    let step3_sse = format!(
        "{}{}data: [DONE]\n\n",
        sse_event(&text_chunk),
        sse_event(&done)
    );

    // 优先级 1（最高）→ 步骤 1：写 main.rs
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(step1_sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .mount(mock)
        .await;
    // 优先级 2 → 步骤 2：写 Cargo.toml
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(step2_sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .with_priority(2)
        .up_to_n_times(1)
        .mount(mock)
        .await;
    // 优先级 3 → 步骤 3：最终文本（兜底）
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(step3_sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .with_priority(3)
        .mount(mock)
        .await;
}

/// E2E 完整项目场景：多步工具调用（写 main.rs → 写 Cargo.toml → 最终回复），
/// 验证磁盘文件真实生成、工具事件完整、最终文本落盘。
#[tokio::test]
async fn e2e_full_project_scaffold() {
    if real_llm_enabled() {
        eprintln!("真实 LLM 模式：跳过完整项目脚本断言（mock 专用）");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().to_string_lossy().into_owned();

    let mock = MockServer::start().await;
    mount_full_project_script(&mock, "Rust 项目创建完成").await;

    let server = start_server(&mock.uri(), &workdir).await;
    let base = format!("http://127.0.0.1:{}", server.port);
    // bypass_permissions + confirm_danger 使 fs.write 自动 Allow（无前端交互）
    let session_id =
        create_session_full(&base, &workdir, Some("bypass_permissions"), Some(true)).await;

    let events = send_and_collect_events(
        &base,
        &session_id,
        "创建 Rust 项目：写 main.rs 和 Cargo.toml",
    )
    .await;
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    eprintln!("E2E 完整项目事件序列: {types:?}");

    // 断言工具调用生命周期
    let started: Vec<&(String, serde_json::Value)> = events
        .iter()
        .filter(|(t, _)| t == "tool_call_started")
        .collect();
    let finished: Vec<&(String, serde_json::Value)> = events
        .iter()
        .filter(|(t, _)| t == "tool_call_finished")
        .collect();
    assert_eq!(
        started.len(),
        2,
        "应有 2 次 tool_call_started（main.rs + Cargo.toml），实际 {}",
        started.len()
    );
    assert_eq!(
        finished.len(),
        2,
        "应有 2 次 tool_call_finished，实际 {}",
        finished.len()
    );

    // 断言工具名称顺序
    let tool_names: Vec<&str> = started
        .iter()
        .map(|(_, v)| v["tool"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        tool_names,
        vec!["fs.write", "fs.write"],
        "工具应依次为 fs.write, fs.write"
    );

    // 断言最终文本落盘
    let turn_end = events
        .iter()
        .find(|(t, _)| t == "turn_end")
        .expect("应有 turn_end");
    assert_eq!(
        turn_end.1["stop_reason"], "end_turn",
        "turn_end 应 end_turn"
    );

    // 断言磁盘文件真实生成（跨组件：server → tools → fs.write 写入真实文件系统）
    let main_rs = tmp.path().join("main.rs");
    let cargo_toml = tmp.path().join("Cargo.toml");
    assert!(main_rs.exists(), "main.rs 应存在于 workdir");
    assert!(cargo_toml.exists(), "Cargo.toml 应存在于 workdir");
    let main_content = std::fs::read_to_string(&main_rs).expect("read main.rs");
    assert!(main_content.contains("fn main()"), "main.rs 应含 Rust 代码");

    // 消息快照验证
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/sessions/{session_id}"))
        .send()
        .await
        .expect("GET session");
    let body: serde_json::Value = resp.json().await.expect("json");
    let all_text: String = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("Rust 项目创建完成"),
        "最终回复应含项目完成文本，实际: {all_text}"
    );
}

/// SSE 等待 `turn_end` 的超时：真实 LLM 首次响应可达 60s+，mock 30s 足够。
fn sse_timeout() -> Duration {
    if real_llm_enabled() {
        Duration::from_secs(180)
    } else {
        Duration::from_secs(30)
    }
}

/// 发送消息并读取 SSE 事件，直到 `turn_end` 或超时。
async fn send_and_collect_events(
    base_url: &str,
    session_id: &str,
    text: &str,
) -> Vec<(String, serde_json::Value)> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/sessions/{session_id}/messages"))
        .json(&json!({"text": text}))
        .send()
        .await
        .expect("POST /messages");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::ACCEPTED,
        "发送消息应 202"
    );

    // 订阅 SSE 事件流
    let resp = client
        .get(format!("{base_url}/sessions/{session_id}/events"))
        .send()
        .await
        .expect("GET /events");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "SSE 订阅应 200");

    let mut events: Vec<(String, serde_json::Value)> = Vec::new();
    let mut stream = resp.bytes_stream();
    timeout(sse_timeout(), async {
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let chunk = chunk.expect("sse chunk");
            let text = String::from_utf8_lossy(&chunk);
            for line in text.lines() {
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(data)
                    .unwrap_or_else(|e| panic!("SSE 解析失败: {e}, line: {data}"));
                let ty = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                events.push((ty.clone(), value));
                if ty == "turn_end" {
                    return;
                }
            }
        }
    })
    .await
    .expect("等待 turn_end 超时");

    drop(stream);
    events
}

/// E2E 全链路：真实 server + mock LLM + 完整会话闭环。
#[tokio::test]
async fn e2e_full_conversation_with_mock_llm() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().to_string_lossy().into_owned();

    let (api_base, _mock_guard) = if real_llm_enabled() {
        (std::env::var("OPENAI_API_BASE").unwrap_or_default(), None)
    } else {
        let mock = MockServer::start().await;
        mount_text_reply(&mock, "E2E 你好，这是一条 mock 回复。").await;
        (mock.uri(), Some(mock))
    };

    let server = start_server(&api_base, &workdir).await;
    let base = format!("http://127.0.0.1:{}", server.port);
    let session_id = create_session(&base, &workdir).await;

    let events = send_and_collect_events(&base, &session_id, "请回复 hello").await;
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    eprintln!("E2E 事件序列: {types:?}");

    assert!(
        events.iter().any(|(t, _)| t == "turn_streaming_started"),
        "应有 turn_streaming_started"
    );
    assert!(
        events.iter().any(|(t, _)| t == "message_appended"),
        "应有 message_appended"
    );
    let turn_end = events
        .iter()
        .find(|(t, _)| t == "turn_end")
        .expect("应有 turn_end");
    assert_eq!(
        turn_end.1["stop_reason"], "end_turn",
        "turn_end 应 end_turn"
    );

    // 拉取消息快照验证落盘
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/sessions/{session_id}"))
        .send()
        .await
        .expect("GET session");
    let body: serde_json::Value = resp.json().await.expect("json");
    let messages = body["messages"].as_array().expect("messages array");
    assert!(messages.len() >= 2, "至少应有用户消息 + assistant 回复");
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    assert!(
        roles.contains(&"user") && roles.contains(&"assistant"),
        "消息快照应含 user + assistant: {roles:?}"
    );
}

/// E2E 工具执行闭环：mock LLM 先发 fs.read 工具调用，验证工具结果回灌与最终文本回复。
#[tokio::test]
async fn e2e_tool_call_roundtrip() {
    if real_llm_enabled() {
        eprintln!("真实 LLM 模式：跳过工具闭环断言（mock 专用脚本）");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().to_string_lossy().into_owned();
    std::fs::write(tmp.path().join("e2e.txt"), "E2E 文件内容").expect("write e2e.txt");

    let mock = MockServer::start().await;
    mount_tool_then_text_reply(&mock, "E2E 工具执行完成").await;
    let server = start_server(&mock.uri(), &workdir).await;

    let base = format!("http://127.0.0.1:{}", server.port);
    let session_id = create_session(&base, &workdir).await;
    let events = send_and_collect_events(&base, &session_id, "读取 e2e.txt").await;
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    eprintln!("E2E 工具闭环事件序列: {types:?}");

    assert!(
        events.iter().any(|(t, _)| t == "tool_call_started"),
        "应有 tool_call_started"
    );
    assert!(
        events.iter().any(|(t, _)| t == "tool_call_finished"),
        "应有 tool_call_finished"
    );

    // 最终文本落盘验证
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/sessions/{session_id}"))
        .send()
        .await
        .expect("GET session");
    let body: serde_json::Value = resp.json().await.expect("json");
    let all_text: String = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("E2E 工具执行完成"),
        "最终回复应含 E2E 文本，实际: {all_text}"
    );
}

/// 并发消息不应卡死：turn 串行化 + 排队上限（FE-13）。
#[tokio::test]
async fn e2e_concurrent_messages_no_hang() {
    if real_llm_enabled() {
        eprintln!("真实 LLM 模式：跳过并发断言（mock 专用）");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path().to_string_lossy().into_owned();
    let mock = MockServer::start().await;
    mount_text_reply(&mock, "ok").await;
    let server = start_server(&mock.uri(), &workdir).await;
    let base = format!("http://127.0.0.1:{}", server.port);
    let session_id = create_session(&base, &workdir).await;

    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for i in 0..3 {
        let c = client.clone();
        let b = base.clone();
        let sid = session_id.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/sessions/{sid}/messages"))
                .json(&json!({"text": format!("msg {i}")}))
                .send()
                .await
                .expect("POST")
                .status()
        }));
    }
    let statuses = futures::future::join_all(handles).await;
    for s in statuses {
        let s = s.expect("join");
        assert!(
            s == reqwest::StatusCode::ACCEPTED || s == reqwest::StatusCode::TOO_MANY_REQUESTS,
            "并发消息应 202 或 429，实际 {s}"
        );
    }

    // 等待队列耗尽
    tokio::time::sleep(Duration::from_secs(3)).await;
    let resp = client
        .get(format!("{base}/sessions/{session_id}"))
        .send()
        .await
        .expect("GET session");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "会话应仍可用（未卡死）"
    );
}

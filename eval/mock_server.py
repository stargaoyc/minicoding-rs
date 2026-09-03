#!/usr/bin/env python3
"""MiniCode 评估 mock LLM server（OpenAI 兼容 SSE）。"""
import json, re, time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = 8765
TASK_ACTIONS = {
    "L1-001": ("fs.write", {"path": "src/lib.rs", "content": (
        "pub fn fib(n: u32) -> u64 {\n"
        "    match n { 0 => 0, 1 => 1, _ => fib(n - 1) + fib(n - 2) }\n"
        "}\n#[cfg(test)]\nmod tests {\n"
        "    use super::*;\n    #[test]\n    fn fib_works() {\n"
        "        assert_eq!(fib(10), 55);\n    }\n}\n"
    )}),
    "L2-001": ("fs.edit", {"path": "src/main.rs",
        "old": "a - b", "new": "a + b"}),
    "L3-001": ("shell.run", {"command": "mkdir -p src/backend src/frontend && touch src/backend/main.rs src/frontend/App.tsx"}),
    "L4-001": ("fs.write", {"path": "src/lib.rs", "content": (
        "pub fn sum(xs: &[i32]) -> i32 {\n    xs.iter().sum()\n}\n"
        "#[cfg(test)]\nmod tests {\n"
        "    use super::*;\n    #[test]\n    fn sum_works() {\n"
        "        assert_eq!(sum(&[1,2,3]), 6);\n        assert_eq!(sum(&[]), 0);\n    }\n}\n"
    )}),
}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        prompt = " ".join(m.get("content","") if isinstance(m.get("content"),str) else json.dumps(m.get("content",""))
                          for m in body.get("messages",[]) if m.get("role")=="user")
        action = self._pick_action(prompt)
        has_tool = any(m.get("role")=="tool" for m in body.get("messages",[]))
        self._reply(action, has_tool)

    def _pick_action(self, prompt):
        for key, act in TASK_ACTIONS.items():
            if key.lower() in prompt.lower():
                return act
        return None

    def _reply(self, action, has_tool_result):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        base = {"id":"mock-1","object":"chat.completion.chunk"}
        if action and not has_tool_result:
            tool, args = action
            chunk = {**base, "choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-mock-1","type":"function","function":{"name":tool,"arguments":json.dumps(args)}}]},"finish_reason":None}]}
            done = {**base, "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}
        else:
            chunk = {**base, "choices":[{"index":0,"delta":{"content":"任务已完成。"},"finish_reason":None}]}
            done = {**base, "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}
        sse = "data: " + json.dumps(chunk) + "\n\ndata: " + json.dumps(done) + "\n\ndata: [DONE]\n\n"
        self.wfile.write(sse.encode())
        time.sleep(0.05)

HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()

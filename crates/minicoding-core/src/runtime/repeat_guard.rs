//! 循环打断器（A4 自 rt.rs 抽出；M-08/R-03，见 `design.md` §2.2、`rules.md` C-13）。
//!
//! 三层语义：
//! - **单工具指纹**（`tool_fingerprint`）：软提醒粒度——同一工具反复调用即命中；
//! - **整轮签名**（`tool_calls_signature`）：硬停止粒度——整轮调用集合完全相同；
//! - **连续判定**（`is_repeating`）：最近 `threshold` 轮签名全部一致。
//!
//! 与压缩熔断（C-29）、沙箱拒绝熔断（C-30）并列的第三类防死循环机制，
//! 由 Runtime 在 turn 循环内驱动。

use crate::model::ToolCall;

/// 计算单工具调用指纹（`name|规范化 input`）。
///
/// `serde_json` 默认对 `Value::Object` 用 `BTreeMap`（键排序），跨轮比较稳定。
#[must_use]
pub fn tool_fingerprint(call: &ToolCall) -> String {
    let input = serde_json::to_string(&call.input).unwrap_or_else(|_| call.input.to_string());
    format!("{}|{}", call.name, input)
}

/// 计算一轮工具调用的签名：多调用指纹排序后拼接（与键顺序无关）。
#[must_use]
pub fn tool_calls_signature(calls: &[ToolCall]) -> String {
    let mut sigs: Vec<String> = calls.iter().map(tool_fingerprint).collect();
    sigs.sort_unstable();
    sigs.join(";")
}

/// 检测最近 `threshold` 轮工具调用签名是否完全相同（连续 ≥ threshold 轮 → 死循环）。
#[must_use]
pub fn is_repeating(signatures: &[String], threshold: u32) -> bool {
    let threshold = threshold.max(1) as usize;
    let n = signatures.len();
    if n < threshold {
        return false;
    }
    let last = &signatures[n - 1];
    signatures[n - threshold..].iter().all(|s| s == last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn fingerprint_is_order_independent_for_object_keys() {
        let a = call("1", "fs.read", json!({"path": "x", "mode": "r"}));
        let b = call("1", "fs.read", json!({"mode": "r", "path": "x"}));
        assert_eq!(tool_fingerprint(&a), tool_fingerprint(&b));
    }

    #[test]
    fn signature_sorts_multi_call_sets() {
        let s1 = [call("1", "a", json!({})), call("2", "b", json!({}))];
        let s2 = [call("3", "b", json!({})), call("4", "a", json!({}))];
        assert_eq!(tool_calls_signature(&s1), tool_calls_signature(&s2));
    }

    #[test]
    fn is_repeating_threshold_semantics() {
        let sig = "x".to_string();
        let sigs: Vec<String> = vec![sig.clone(), sig.clone(), sig.clone()];
        assert!(is_repeating(&sigs, 3));
        assert!(!is_repeating(&sigs[..2], 3));
        // 阈值下限 1
        assert!(is_repeating(&sigs[..1], 0));
    }
}

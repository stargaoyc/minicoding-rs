//! BM25 工具检索（T-M8-5）。
//!
//! 当 `ToolRegistry` 注册大量工具时，用 BM25 检索最相关的工具，减少 LLM context
//! 占用。索引工具的 `name` + `description` + 参数名，按自然语言查询排序。
//!
//! ## 使用场景
//!
//! - MCP server 模式：动态过滤暴露给 client 的工具子集；
//! - Runtime：在工具数 > 阈值时，仅向 LLM 传入 top-k 相关工具 schema。
//!
//! ## 设计依据
//!
//! 与 `minicoding-memory::vector` 同构（BM25 + 简单分词），但索引对象不同
//! （工具 schema vs 记忆段落）。不提取共享 BM25 crate——两端规模小、耦合低。

// BM25 评分涉及 usize→f64 转换，在工具数 < 1k 场景下精度损失可忽略。
#![allow(clippy::cast_precision_loss)]

use minicoding_core::model::ToolSchema;
use std::collections::HashMap;

/// BM25 参数（与 Lucene 默认一致）。
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// 工具检索结果。
#[derive(Debug, Clone)]
pub struct ToolSearchResult {
    /// 工具名（如 `fs.read`）。
    pub name: String,
    /// BM25 分数。
    pub score: f64,
}

/// BM25 工具检索索引。
pub struct ToolSearchIndex {
    docs: Vec<ToolDoc>,
    df: HashMap<String, usize>,
    avg_doc_len: f64,
}

struct ToolDoc {
    name: String,
    tf: HashMap<String, usize>,
    len: usize,
}

impl Default for ToolSearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSearchIndex {
    /// 创建空索引。
    #[must_use]
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            df: HashMap::new(),
            avg_doc_len: 0.0,
        }
    }

    /// 从 `ToolSchema` 列表构建索引。
    #[must_use]
    pub fn from_schemas(schemas: &[ToolSchema]) -> Self {
        let mut idx = Self::new();
        for schema in schemas {
            idx.add_schema(schema);
        }
        idx
    }

    /// 添加单个工具 schema 到索引。
    pub fn add_schema(&mut self, schema: &ToolSchema) {
        // 索引文本 = name + description + 参数名（从 input_schema 提取 properties keys）
        let param_names = extract_param_names(&schema.input_schema);
        let text = format!(
            "{} {} {}",
            schema.name,
            schema.description,
            param_names.join(" ")
        );
        let tokens = tokenize(&text);
        let len = tokens.len();
        let mut tf: HashMap<String, usize> = HashMap::new();
        for tok in &tokens {
            *tf.entry(tok.clone()).or_default() += 1;
        }
        for tok in tf.keys() {
            *self.df.entry(tok.clone()).or_default() += 1;
        }
        self.docs.push(ToolDoc {
            name: schema.name.clone(),
            tf,
            len,
        });
        let total: usize = self.docs.iter().map(|d| d.len).sum();
        self.avg_doc_len = if self.docs.is_empty() {
            0.0
        } else {
            total as f64 / self.docs.len() as f64
        };
    }

    /// BM25 检索：返回 top-k 最相关工具。
    #[must_use]
    pub fn search(&self, query: &str, top_k: usize) -> Vec<ToolSearchResult> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }

        let n = self.docs.len() as f64;
        let mut scored: Vec<(usize, f64)> = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let score = query_tokens
                    .iter()
                    .map(|qt| {
                        let tf = doc.tf.get(qt).copied().unwrap_or(0) as f64;
                        if tf == 0.0 {
                            return 0.0;
                        }
                        let df = self.df.get(qt).copied().unwrap_or(0) as f64;
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        let denom = tf + K1 * (1.0 - B + B * doc.len as f64 / self.avg_doc_len);
                        idf * (tf * (K1 + 1.0)) / denom
                    })
                    .sum();
                (i, score)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(i, score)| ToolSearchResult {
                name: self.docs[i].name.clone(),
                score,
            })
            .collect()
    }

    /// 索引工具数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// 索引是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

/// 从 JSON Schema 提取参数名（`properties` 的 keys）。
fn extract_param_names(schema: &serde_json::Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// 简易分词（与 `minicoding-memory::vector::tokenize` 同构）。
///
/// `_` 和 `.` 作为分隔符（不保留在 token 中），使 `fs.read` → `["fs", "read"]`、
/// `database_url` → `["database", "url"]`，便于子串匹配。CJK 字符逐字成 token
/// （`is_alphanumeric()` 对 CJK 返回 true，须优先检查 `is_cjk` 以避免相邻汉字
/// 被合并为单个 token）。
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current_word = String::new();
    for ch in text.chars() {
        if is_cjk(ch) {
            if !current_word.is_empty() {
                tokens.push(current_word.clone());
                current_word.clear();
            }
            tokens.push(ch.to_string());
        } else if ch.is_alphanumeric() {
            current_word.push(ch.to_ascii_lowercase());
        } else if !current_word.is_empty() {
            tokens.push(current_word.clone());
            current_word.clear();
        }
    }
    if !current_word.is_empty() {
        tokens.push(current_word);
    }
    tokens
}

/// CJK 字符判断。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x309F | 0x30A0..=0x30FF
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    fn make_schema(name: &str, desc: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: desc.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        }
    }

    #[test]
    fn search_finds_relevant_tool() {
        let schemas = vec![
            make_schema("fs.read", "读取文件内容"),
            make_schema("shell.run", "执行 shell 命令"),
            make_schema("web.fetch", "获取 URL 内容"),
        ];
        let idx = ToolSearchIndex::from_schemas(&schemas);
        let results = idx.search("读文件", 2);
        assert!(!results.is_empty(), "expected non-empty: results");
        assert_eq!(results[0].name, "fs.read");
    }

    #[test]
    fn empty_index_returns_empty() {
        let idx = ToolSearchIndex::new();
        assert!(idx.is_empty(), "expected empty: idx");
        assert!(idx.search("anything", 5).is_empty());
    }

    #[test]
    fn top_k_limits_results() {
        let schemas: Vec<ToolSchema> = (0..10)
            .map(|i| make_schema(&format!("tool_{i}"), "rust file read write"))
            .collect();
        let idx = ToolSearchIndex::from_schemas(&schemas);
        let results = idx.search("rust", 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn param_names_indexed() {
        let schema = ToolSchema {
            name: "custom.tool".into(),
            description: "custom tool".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "database_url": { "type": "string" },
                    "query": { "type": "string" }
                }
            }),
        };
        let idx = ToolSearchIndex::from_schemas(&[schema]);
        let results = idx.search("database", 1);
        assert!(!results.is_empty(), "expected non-empty: results");
        assert_eq!(results[0].name, "custom.tool");
    }
}

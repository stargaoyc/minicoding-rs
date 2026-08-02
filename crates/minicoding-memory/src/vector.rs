//! `@memory` 语义检索（T-M8-5）。
//!
//! 轻量级 BM25 文本检索——不依赖 embedding 模型，用 tokenization + IDF + TF
//! 排序。索引 `long_term.md` 段落 + 会话摘要，提供 `search(query, top_k)`。
//!
//! ## 设计依据
//!
//! `data-model.md` §"后续"：引入轻量向量索引支持 `@memory` 语义检索。BM25 在
//! 小规模语料（< 1k 段落）上效果接近 embedding，且零外部依赖、零网络调用。
//! 若后续需要更强语义匹配，可替换为 `LlmProvider::embed` + cosine similarity。

// BM25 评分涉及 usize→f64 转换（文档长度/词频），在 < 1k 段落场景下精度损失
// 可忽略（2^52 mantissa 远超实际文档长度）。整模块允许 `cast_precision_loss`。
#![allow(clippy::cast_precision_loss)]

use std::collections::HashMap;

/// BM25 参数（经验值，与 Lucene/Elasticsearch 默认一致）。
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// 检索结果。
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// 文档 ID（段落标题或摘要 ID）。
    pub id: String,
    /// BM25 分数（越高越相关）。
    pub score: f64,
    /// 匹配片段（前 200 字符）。
    pub snippet: String,
}

/// BM25 检索索引（`@memory` 语义检索）。
///
/// 索引 `long_term.md` 的 Markdown 段落（按 `##` 标题切分）+ 会话摘要。
/// `search` 返回 top-k 最相关段落，供 `ContextManager` 注入 context。
pub struct MemoryIndex {
    docs: Vec<Doc>,
    df: HashMap<String, usize>,
    avg_doc_len: f64,
}

struct Doc {
    id: String,
    content: String,
    tf: HashMap<String, usize>,
    len: usize,
}

impl Default for MemoryIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryIndex {
    /// 创建空索引。
    #[must_use]
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            df: HashMap::new(),
            avg_doc_len: 0.0,
        }
    }

    /// 添加文档（段落/摘要）到索引。
    pub fn add_document(&mut self, id: &str, content: &str) {
        let tokens = tokenize(content);
        let len = tokens.len();
        let mut tf: HashMap<String, usize> = HashMap::new();
        for tok in &tokens {
            *tf.entry(tok.clone()).or_default() += 1;
        }
        // 更新 document frequency
        for tok in tf.keys() {
            *self.df.entry(tok.clone()).or_default() += 1;
        }
        self.docs.push(Doc {
            id: id.to_string(),
            content: content.to_string(),
            tf,
            len,
        });
        // 重算 avg_doc_len
        let total: usize = self.docs.iter().map(|d| d.len).sum();
        self.avg_doc_len = if self.docs.is_empty() {
            0.0
        } else {
            total as f64 / self.docs.len() as f64
        };
    }

    /// 从 `long_term.md` 正文构建索引（按 `##` 标题切分段落）。
    #[must_use]
    pub fn from_long_term(content: &str) -> Self {
        let mut idx = Self::new();
        for (i, section) in split_sections(content).into_iter().enumerate() {
            let id = if section.heading.is_empty() {
                format!("section_{i}")
            } else {
                section.heading
            };
            idx.add_document(&id, &section.body);
        }
        idx
    }

    /// BM25 检索：返回 top-k 最相关文档。
    #[must_use]
    pub fn search(&self, query: &str, top_k: usize) -> Vec<SearchResult> {
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
            .map(|(i, score)| {
                let doc = &self.docs[i];
                let snippet = if doc.content.len() > 200 {
                    doc.content[..200].to_string()
                } else {
                    doc.content.clone()
                };
                SearchResult {
                    id: doc.id.clone(),
                    score,
                    snippet,
                }
            })
            .collect()
    }

    /// 索引文档数。
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

/// Markdown 段落切分（按 `##` 标题）。
struct Section {
    heading: String,
    body: String,
}

fn split_sections(content: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current_heading = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if line.starts_with("## ") {
            // 保存前一段落
            if !current_body.is_empty() {
                sections.push(Section {
                    heading: current_heading.clone(),
                    body: current_body.clone(),
                });
                current_body.clear();
            }
            current_heading = line.trim_start_matches("## ").trim().to_string();
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if !current_body.is_empty() {
        sections.push(Section {
            heading: current_heading,
            body: current_body,
        });
    }
    sections
}

/// 简易分词：英文按空格/标点切分 + CJK 逐字成 token。
///
/// 不引入 `tiktoken-rs`（重依赖）——`@memory` 检索只需粗粒度匹配，
/// 简单分词 + BM25 已足够。小写化以提升匹配率。
///
/// CJK 字符须优先于 `is_alphanumeric()` 检查——后者对 CJK 返回 true，
/// 会导致相邻汉字被合并为单个 token（如"异步编程" → 一个 token 而非四个）。
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

/// 判断字符是否为 CJK 统一汉字。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x3040..=0x309F // Hiragana
        | 0x30A0..=0x30FF // Katakana
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn bm25_retrieves_relevant_section() {
        let content = "## Rust 编码规范\n\
            edition 2024，MSRV 1.99+，async fn in trait 直接用。\n\
            错误处理用 thiserror，不 panic。\n\n\
            ## Git 工作流\n\
            分支命名 feature/xxx，提交用 Conventional Commits。\n";
        let idx = MemoryIndex::from_long_term(content);
        let results = idx.search("Rust 错误处理", 2);
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "Rust 编码规范");
    }

    #[test]
    fn empty_index_returns_empty() {
        let idx = MemoryIndex::new();
        assert!(idx.search("anything", 5).is_empty());
        assert!(idx.is_empty());
    }

    #[test]
    fn cjk_tokenization_works() {
        let tokens = tokenize("Rust 异步编程 async");
        assert!(tokens.contains(&"rust".to_string()));
        assert!(tokens.contains(&"异".to_string()));
        assert!(tokens.contains(&"步".to_string()));
        assert!(tokens.contains(&"async".to_string()));
    }

    #[test]
    fn top_k_limits_results() {
        let mut idx = MemoryIndex::new();
        for i in 0..10 {
            idx.add_document(&format!("doc_{i}"), &format!("rust programming {i}"));
        }
        let results = idx.search("rust", 3);
        assert_eq!(results.len(), 3);
    }
}

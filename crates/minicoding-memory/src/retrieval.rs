//! `@memory` 记忆检索聚合（B3）：auto 条目 + `long_term` 分节统一入 BM25 索引。
//!
//! 在 [`crate::vector::MemoryIndex`]（纯 BM25，CJK 逐字分词）之上做语料组装：
//! - `long_term.md` 按 `## [category] topic` / `## topic` 分节解析为文档；
//! - Auto memory 渲染正文（同为 `## ` 分节格式）直接按分节入索引；
//! - [`MemoryRetrieval::search`] 返回整节文本（标题 + 正文），供
//!   `AutoMemoryContributor` 注入 system 段（C-05 边界由 contributor 负责）。
//!
//! 消费方：`minicoding-memory::auto_contributor`（B2/B3 接线）。

use crate::{AutoMemory, MemoryIndex};
use std::collections::HashMap;

/// 检索语料中的一篇文档（一节）。
struct RetrievalDoc {
    /// 整节文本（标题行 + 正文），命中时原样返回。
    text: String,
}

/// BM25 记忆检索器（auto + `long_term` 统一语料）。
pub struct MemoryRetrieval {
    /// 倒排索引（评分用）。
    index: MemoryIndex,
    /// id → 文档（结果回取用）。
    docs: HashMap<String, RetrievalDoc>,
}

impl Default for MemoryRetrieval {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRetrieval {
    /// 创建空检索器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: MemoryIndex::new(),
            docs: HashMap::new(),
        }
    }

    /// 从两份 Markdown 正文构建索引。
    ///
    /// 两份语料均按 `## ` 标题分节；首节前的游离文本作为无标题文档入索引
    /// （与 `vector::split_sections` 口径一致）。同 id 文档后写覆盖先写
    /// （auto 与 `long_term` 同 topic 时以 auto 为准——更新鲜）。
    #[must_use]
    pub fn from_markdown(long_term_md: &str, auto_md: &str) -> Self {
        let mut r = Self::new();
        for (source, content) in [("long_term", long_term_md), ("auto", auto_md)] {
            for section in split_sections(content) {
                let title = section.title.unwrap_or_else(|| {
                    // 无标题节：来源前缀 + 顺序号合成稳定 id。
                    format!("{source}_intro")
                });
                let text = match &section.heading_line {
                    Some(h) => format!("{h}\n{}", section.body),
                    None => section.body.clone(),
                };
                let doc = RetrievalDoc { text };
                // R8 MEM-8：auto 覆盖 long_term 同标题时记 debug（design 语义
                // "以 auto 为准——更新鲜"）。此前静默覆盖，排障无法感知语料损失。
                if r.docs.contains_key(&title) {
                    tracing::debug!(
                        title = %title,
                        source,
                        "检索语料同标题覆盖（auto 优先于 long_term）"
                    );
                }
                r.docs.insert(title, doc);
            }
        }
        // HashMap 遍历序不稳定 → 索引构建顺序不稳定。BM25 分数只受文档长度
        // 影响（IDF/TF 与顺序无关），排序按分数降序 + id 字典序稳定化，
        // 保证同查询结果确定性。
        let mut ids: Vec<&String> = r.docs.keys().collect();
        ids.sort();
        for id in ids {
            let doc = &r.docs[id];
            r.index.add_document(id, &doc.text);
        }
        r
    }

    /// 从存储构建索引（async：AutoMemory 走 mtime 缓存加载）。
    ///
    /// 加载失败降级为仅 `long_term` 语料（warn 日志，不阻塞调用方）。
    pub async fn from_stores(auto: &AutoMemory, long_term_md: &str) -> Self {
        let auto_md = auto.load_rendered().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "检索语料加载 auto memory 失败，退化为仅 long_term");
            String::new()
        });
        Self::from_markdown(long_term_md, &auto_md)
    }

    /// BM25 检索 top-k，返回整节文本（标题 + 正文）。
    ///
    /// 无查询词或空索引返回空 `Vec`。同分数按 id 字典序稳定排序。
    #[must_use]
    pub fn search(&self, query: &str, k: usize) -> Vec<String> {
        self.index
            .search(query, k)
            .into_iter()
            .filter_map(|hit| self.docs.get(&hit.id).map(|d| d.text.clone()))
            .collect()
    }

    /// 语料文档数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// 语料是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

/// 分节解析中间结构。
struct RawSection {
    /// 节标题（`## ` 后文本）；首节前游离文本为 `None`。
    title: Option<String>,
    /// 原始标题行（含 `## `，重建整节文本用）。
    heading_line: Option<String>,
    /// 节正文。
    body: String,
}

/// 按 `## ` 行切分 Markdown 为节序列（与 `vector::split_sections` 同口径）。
fn split_sections(content: &str) -> Vec<RawSection> {
    let mut sections: Vec<RawSection> = Vec::new();
    let mut current = RawSection {
        title: None,
        heading_line: None,
        body: String::new(),
    };

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !current.body.trim().is_empty() || current.title.is_some() {
                sections.push(current);
            }
            current = RawSection {
                title: Some(rest.trim().to_string()),
                heading_line: Some(line.to_string()),
                body: String::new(),
            };
        } else {
            current.body.push_str(line);
            current.body.push('\n');
        }
    }
    if !current.body.trim().is_empty() || current.title.is_some() {
        sections.push(current);
    }
    sections
}

#[cfg(test)]
mod tests {
    //! B3 检索行为：中文 `query` 命中与排序、`from_stores` 组装、空语料边界。

    use super::*;
    use crate::AutoCategory;
    use camino::Utf8PathBuf;

    fn make(dir: &std::path::Path) -> AutoMemory {
        AutoMemory::with_dir(
            &Utf8PathBuf::from_path_buf(dir.to_owned())
                .expect("tempdir path is UTF-8 on linux test env"),
        )
    }

    #[test]
    fn chinese_query_ranks_relevant_doc_first() {
        let lt = "## 异步编程规范\n\
                  tokio 运行时不混用 async-std，锁用 tokio Mutex。\n\n\
                  ## 提交规范\n\
                  提交信息使用 Conventional Commits 中文描述。\n";
        let r = MemoryRetrieval::from_markdown(lt, "");
        assert_eq!(r.len(), 2);

        let hits = r.search("并发锁的写法", 1);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].contains("异步编程规范"),
            "应命中异步节: {}",
            hits[0]
        );
        assert!(!hits[0].contains("Conventional"));
    }

    #[test]
    fn auto_and_long_term_merged_with_auto_priority() {
        let lt = "## style\nold content from long term\n";
        let auto = "## [pref] style\nupdated via auto memory\n";
        let r = MemoryRetrieval::from_markdown(lt, auto);
        assert_eq!(r.len(), 2, "不同 id 各自成篇");

        let hits = r.search("style updated", 2);
        assert!(hits.iter().any(|h| h.contains("updated via auto memory")));
    }

    #[tokio::test]
    async fn from_stores_includes_auto_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = make(tmp.path());
        mem.add_entry(
            "rust-msrv".to_string(),
            "项目 MSRV 是 199，edition 2024".to_string(),
            AutoCategory::Decision,
            0.9,
        )
        .await
        .expect("add_entry 应成功");

        let r = MemoryRetrieval::from_stores(&mem, "").await;
        assert!(!r.is_empty());
        let hits = r.search("MSRV 版本要求", 3);
        assert!(!hits.is_empty(), "中文 query 应命中 auto 条目: {hits:?}");

        assert!(
            hits[0].contains("[decision] rust-msrv"),
            "返回整节含标题: {}",
            hits[0]
        );
    }

    #[test]
    fn empty_corpus_search_returns_empty() {
        let r = MemoryRetrieval::new();
        assert!(r.is_empty());
        assert!(r.search("anything", 5).is_empty(), "expected empty: search");
    }

    #[test]
    fn intro_text_before_first_heading_is_indexed() {
        let lt = "全局约定：所有代码走 cargo fmt。\n\n## 其他\n正文\n";
        let r = MemoryRetrieval::from_markdown(lt, "");
        assert_eq!(r.len(), 2, "游离导语文档 + 标题节");
        let hits = r.search("cargo fmt 格式化", 1);
        assert!(!hits.is_empty(), "expected non-empty: hits");
        assert!(hits[0].contains("全局约定"));
    }

    #[tokio::test]
    async fn from_stores_degrades_when_auto_index_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap();
        std::fs::write(dir.join("auto.index.json"), b"broken").unwrap();
        let mem = make(tmp.path());
        let r = MemoryRetrieval::from_stores(&mem, "## note\nlong term only\n").await;
        assert_eq!(r.len(), 1, "损坏索引应降级为仅 long_term 语料");
        assert!(
            !r.search("long term", 1).is_empty(),
            "expected non-empty: search"
        );
    }
}

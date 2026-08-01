//! 敏感数据脱敏（T-M4-11，C-04，见 `security.md` §13.3）。
//!
//! 应用于 `fs.read` 读取 `.env` / 配置 / 凭证文件时的输出，把密钥类字段值替换
//! 为 `***`，避免敏感数据回灌 LLM 上下文或落 jsonl 落盘。
//!
//! ## 内置正则模式
//!
//! 覆盖常见密钥字段命名（不依赖具体值格式，避免误伤）：
//! - `KEY = value` / `KEY: value`（`KEY` 含 `API_KEY`/`TOKEN`/`SECRET`/`PASSWORD`/`PRIVATE_KEY`）
//! - `Authorization: Bearer xxx` / `Bearer xxx`
//! - AWS access key：`AKIA[0-9A-Z]{16}`
//!
//! 用户可通过 `[redact] patterns = ["..."]` 增加自定义正则（M5+ 接入）。

use regex::Regex;

/// 命中即脱敏的关键词（大小写不敏感，匹配字段名子串）。
const SECRET_KEYWORDS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "private_key",
    "access_key",
    "client_secret",
    "refresh_token",
];

/// AWS access key id 前缀（用于兜底匹配无明显字段名的密钥）。
const AWS_AKIA_PATTERN: &str = r"AKIA[0-9A-Z]{16}";

/// `Authorization: Bearer xxx` / `Bearer xxx` 头部脱敏。
const BEARER_PATTERN: &str = r"(?i)(Bearer\s+)([A-Za-z0-9_\-.=:/+]+)";

/// 把敏感字段值替换为 `***`。
///
/// 处理顺序：先脱敏字段赋值（`KEY=value`/`KEY: value`），再脱敏 Bearer token，
/// 最后脱敏 AWS AKIA 模式。多轮匹配避免相互干扰。
#[must_use]
pub fn redact(input: &str) -> String {
    let mut out = String::with_capacity(input.len());

    for line in input.lines() {
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    // 末尾换行处理：原输入若无尾换行，去掉追加的换行
    if !input.ends_with('\n') {
        out.pop();
    }
    out
}

/// 脱敏单行：识别 `KEY=value`/`KEY: value` 模式后整体替换值。
fn redact_line(line: &str) -> String {
    // 1. 字段赋值模式：`KEY = value` 或 `KEY: value`
    if let Some((sep_idx, _is_colon)) = find_secret_assignment(line) {
        return redact_assignment(line, sep_idx);
    }

    // 2. Bearer token
    if let Ok(re) = Regex::new(BEARER_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "${1}***").into_owned();
    }

    // 3. AWS AKIA 模式
    if let Ok(re) = Regex::new(AWS_AKIA_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "***").into_owned();
    }

    line.to_string()
}

/// 在 `line` 中查找敏感字段的赋值分隔符位置（`=` 或 `:`）。
///
/// 返回 `Some((sep_idx, is_colon))`：分隔符索引及是否为冒号分隔。
/// 字段名须包含 `SECRET_KEYWORDS` 中任一关键词（大小写不敏感）。
fn find_secret_assignment(line: &str) -> Option<(usize, bool)> {
    let lower = line.to_lowercase();
    // 优先匹配 `=` 分隔（.env / shell 风格）
    if let Some(eq_idx) = lower.find('=') {
        let key_part = &lower[..eq_idx];
        if is_secret_key(key_part) {
            return Some((eq_idx, false));
        }
    }
    // 再匹配 `:` 分隔（YAML / TOML inline 风格）
    if let Some(colon_idx) = lower.find(':') {
        let key_part = &lower[..colon_idx];
        if is_secret_key(key_part) {
            return Some((colon_idx, true));
        }
    }
    None
}

/// 字段名是否含敏感关键词。
///
/// 把字段名中的 `-`/空白归一化为 `_` 后再匹配（如 `Api-Key` → `api_key`），
/// 避免因命名风格差异（`kebab-case` vs `snake_case`）漏检。
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    let normalized: String = lower
        .chars()
        .map(|c| {
            if c == '-' || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect();
    SECRET_KEYWORDS.iter().any(|kw| normalized.contains(kw))
}

/// 把 `KEY=value` / `KEY: value` 的 value 部分替换为 `***`，保留 KEY 与分隔符。
fn redact_assignment(line: &str, sep_idx: usize) -> String {
    let key_part = &line[..sep_idx];
    // 跳过 `=` / `:` 与可选空白
    let rest = &line[sep_idx + 1..];
    let value_start = match rest.char_indices().find(|(_, c)| !c.is_whitespace()) {
        Some((i, _)) => sep_idx + 1 + i,
        None => line.len(),
    };
    let sep = &line[sep_idx..=sep_idx];
    let whitespace = &line[sep_idx + 1..value_start];
    format!("{key_part}{sep}{whitespace}***")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn redact_env_file() {
        let input = "API_KEY=sk-1234567890\nTOKEN=abc\nPORT=8080\n";
        let out = redact(input);
        assert!(out.contains("API_KEY=***"));
        assert!(out.contains("TOKEN=***"));
        assert!(out.contains("PORT=8080")); // 非敏感字段不脱敏
    }

    #[test]
    fn redact_yaml_style() {
        let input = "password: hunter2\nname: bob\n";
        let out = redact(input);
        assert!(out.contains("password: ***"));
        assert!(out.contains("name: bob"));
    }

    #[test]
    fn redact_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig\n";
        let out = redact(input);
        assert!(out.contains("Bearer ***"));
        // 注意：`Authorization:` 字段名不含 SECRET_KEYWORDS，所以仅 Bearer 部分被替换
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn redact_aws_akia() {
        let input = "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n";
        let out = redact(input);
        // `aws_access_key_id` 含 `access_key` → 整体脱敏为 ***
        assert!(out.contains("aws_access_key_id = ***"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redact_aws_akia_inline() {
        // 字段名非敏感，但值匹配 AKIA 模式
        let input = "key: AKIAIOSFODNN7EXAMPLE\n";
        let out = redact(input);
        // `key` 不含 SECRET_KEYWORDS，但 AKIA 模式匹配兜底
        assert!(!out.contains("AKIA") || out.contains("***"));
    }

    #[test]
    fn redact_no_trailing_newline() {
        let input = "API_KEY=sk-test";
        let out = redact(input);
        assert!(!out.ends_with('\n'));
        assert_eq!(out, "API_KEY=***");
    }

    #[test]
    fn redact_preserves_nonsecret_content() {
        let input = "Hello world\nThis is a normal line\n";
        let out = redact(input);
        assert_eq!(out, input);
    }

    #[test]
    fn redact_mixed_case_key() {
        let input = "Api-Key: sk-test\nSECRET_TOKEN=abc\n";
        let out = redact(input);
        assert!(out.contains("Api-Key: ***"));
        assert!(out.contains("SECRET_TOKEN=***"));
    }

    #[test]
    fn redact_quoted_value() {
        // 引号包裹的值也应脱敏
        let input = "PASSWORD=\"hunter2\"\n";
        let out = redact(input);
        assert!(out.contains("PASSWORD="));
        assert!(out.contains("***"));
        assert!(!out.contains("hunter2"));
    }
}

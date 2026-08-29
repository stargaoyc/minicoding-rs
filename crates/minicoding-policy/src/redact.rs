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

/// 已知凭证前缀（TL-R6-8，2026-08-28 R6 审查）：OpenAI 密钥（`sk-` 开头）、
/// GitHub 令牌（`ghp_` / `github_pat_` 开头）、Slack 令牌（`xoxb-` 开头）。
/// `shell.background` 与 `shell.output` 走 `minicoding_policy::redact`，此前
/// 不含这些前缀——后台命令输出 GitHub 令牌等原样回灌 LLM/前端（前台
/// `shell.run` 的本地 `redact_secrets` 已覆盖，两套规则不同步）。补前缀脱敏：
/// 命中即整体替换为 `***`。
const CREDENTIAL_PREFIX_PATTERN: &str = r"(?i)(sk-[A-Za-z0-9]{8,}|ghp_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,}|xoxb-[A-Za-z0-9-]{8,})";

/// `Authorization: Bearer xxx` / `Bearer xxx` 头部脱敏。
const BEARER_PATTERN: &str = r"(?i)(Bearer\s+)([A-Za-z0-9_\-.=:/+]+)";

/// URL userinfo 脱敏（SEC-14，2026-08-28 R5）：`scheme://user:pass@host` 中
/// userinfo 段的密码是活凭证（`DATABASE_URL=postgres://user:pass@db` 等），
/// 但键名（`DATABASE_URL`）不含 `SECRET_KEYWORDS` 关键词，字段赋值模式漏检。
/// 脱敏 userinfo（`user:pass@` → `user:***@`，保留 user 便于识别归属）；
/// 仅 user:pass 双段形态（单段 user@ 无密码不脱敏——`git@github.com` 等
/// 常见无凭证形态不误伤）。
///
/// SEC-R6-6（2026-08-28 R6 审查）：密码字符集原为 `[^/@\s]+`——密码含 `@`
/// 或 `/` 时（`postgres://user:pass@word@host/db`）只脱敏到第一个 `@`，剩余
/// 段裸露。放宽为 `[^:\s]+`（贪心匹配 + 回溯到 authority 内**最后一个** `@`）：
/// 密码可含 `@`/`/`，整段 userinfo 一并脱敏。仍排除 `:`（端口分隔，避免
/// 吞入 host:port）与空白（避免跨 token）。
const URL_USERINFO_PATTERN: &str = r"(?i)([a-z][a-z0-9+.-]*://)([^/@:\s]+):([^:\s]+)(@)";

/// 把敏感字段值替换为 `***`。
///
/// 处理顺序：先整块脱敏 PEM 私钥（多行，R8 SEC-4），再逐行脱敏字段赋值
/// （`KEY=value`/`KEY: value`）、Bearer token、URL userinfo、AWS AKIA 模式。
/// 多轮匹配避免相互干扰。
#[must_use]
pub fn redact(input: &str) -> String {
    // R8 SEC-4 修复：PEM 私钥块是多行凭证（`-----BEGIN X PRIVATE KEY-----` 起
    // 至 `-----END X PRIVATE KEY-----`），逐行脱敏无法命中——base64 载荷行无
    // KEY=value/Bearer/AKIA 形态。整块脱敏：保留头尾 marker（可辨识），
    // 载荷 base64 全部替换为 `***`。
    if PEM_BLOCK.is_match(input) {
        return PEM_BLOCK
            .replace_all(input, "${marker}\n***\n${marker_end}")
            .into_owned();
    }
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

/// PEM 私钥块（多行凭证，R8 SEC-4）：`-----BEGIN [A-Z ]*PRIVATE KEY-----` 起
/// 至 `-----END [A-Z ]*PRIVATE KEY-----`（RSA/EC/OPENSSH 等变体）。`(?s)` 跨行
/// 匹配；marker 命名捕获供替换保留头尾。
static PEM_BLOCK: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    // `(?s).*?` 跨行非贪心匹配载荷（`[^]` 语法在 regex crate 不合法）
    Regex::new(
        r"(?s)(?P<marker>-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----).*?(?P<marker_end>-----END [A-Z0-9 ]*PRIVATE KEY-----)",
    )
    .expect("PEM block regex is valid")
});

/// 脱敏单行：识别 `KEY=value`/`KEY: value` 模式后整体替换值。
fn redact_line(line: &str) -> String {
    // 1. 字段赋值模式：扫描行内**全部** `KEY=value` / `KEY: value` 赋值对
    //    （2026-08-25 审查 §6.2-S9：此前每行只看第一个分隔符，
    //    `PORT=8080 API_KEY=sk-x` 这类首个字段非敏感的多赋值行整行漏检）
    let assignments = find_secret_assignments(line);
    if !assignments.is_empty() {
        return redact_assignments(line, &assignments);
    }

    // 2. Bearer token
    if let Ok(re) = Regex::new(BEARER_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "${1}***").into_owned();
    }

    // 3. URL userinfo（SEC-14）
    if let Ok(re) = Regex::new(URL_USERINFO_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "${1}${2}:***${4}").into_owned();
    }

    // 4. AWS AKIA 模式
    if let Ok(re) = Regex::new(AWS_AKIA_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "***").into_owned();
    }

    // 5. 已知凭证前缀（TL-R6-8）：sk-/ghp_/github_pat_/xoxb-
    if let Ok(re) = Regex::new(CREDENTIAL_PREFIX_PATTERN)
        && re.is_match(line)
    {
        return re.replace_all(line, "***").into_owned();
    }

    line.to_string()
}

/// 在 `line` 中查找**所有**敏感字段的赋值分隔符位置（`=` 或 `:`）。
///
/// 按空白把行切段，每段内自段首向右扫描分隔符：字段名（段首到分隔符）含
/// `SECRET_KEYWORDS` 任一关键词即命中（每段只取首个命中，避免值区重复处理）。
/// 返回 `(sep_idx, is_colon)` 列表，按出现顺序排列。
fn find_secret_assignments(line: &str) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut seg_start: Option<usize> = None;
    // 以哨兵空白结尾，统一刷新最后一段
    for (i, c) in line
        .char_indices()
        .chain(std::iter::once((line.len(), ' ')))
    {
        if !c.is_whitespace() {
            seg_start.get_or_insert(i);
            continue;
        }
        if let Some(start) = seg_start.take() {
            let seg = &line[start..i];
            for (rel, sc) in seg.char_indices() {
                if sc != '=' && sc != ':' {
                    continue;
                }
                if is_secret_key(&seg[..rel]) {
                    out.push((start + rel, sc == ':'));
                    break;
                }
            }
        }
    }
    out
}

/// 把各敏感赋值的 value 部分替换为 `***`，保留 KEY 与分隔符及前置空白。
///
/// 值的终止边界：下一个敏感赋值所在段的段首；末个赋值为行尾。中间夹带的
/// 非敏感 token 会一并吞入值区——方向是过度脱敏而非泄漏（fail-closed）。
fn redact_assignments(line: &str, assigns: &[(usize, bool)]) -> String {
    let _ = assigns.iter().all(|&(_, _)| true); // 保持签名信息性
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (idx, &(sep_idx, _)) in assigns.iter().enumerate() {
        // 下一个赋值所在段的段首（跳过其 key 与前置空白）；末个为行尾
        let value_end = if idx + 1 < assigns.len() {
            let next_sep = assigns[idx + 1].0;
            line[..next_sep]
                .rfind(char::is_whitespace)
                .map_or(next_sep, |wi| {
                    wi + (line[wi..].len() - line[wi..].trim_start().len())
                })
        } else {
            line.len()
        };
        out.push_str(&line[cursor..sep_idx]);
        let sep = &line[sep_idx..=sep_idx];
        let rest = &line[sep_idx + 1..];
        let ws_len = rest.len() - rest.trim_start().len();
        out.push_str(sep);
        out.push_str(&line[sep_idx + 1..sep_idx + 1 + ws_len]);
        out.push_str("***");
        cursor = value_end;
    }
    out.push_str(&line[cursor.min(line.len())..]);
    out
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

    #[test]
    fn redact_multiple_assignments_on_one_line() {
        // 2026-08-25 审查 §6.2-S9：首个分隔符字段非敏感的多赋值行，
        // 此前整行漏检
        let input = "PORT=8080 API_KEY=sk-secret123\n";
        let out = redact(input);
        assert!(out.contains("PORT=8080"));
        assert!(out.contains("API_KEY="));
        assert!(!out.contains("sk-secret123"));
    }

    #[test]
    fn redact_two_secrets_on_one_line() {
        let input = "API_TOKEN=aaa SECRET_KEY=bbb\n";
        let out = redact(input);
        assert!(out.contains("API_TOKEN="));
        assert!(out.contains("SECRET_KEY="));
        assert!(!out.contains("aaa") && !out.contains("bbb"));
    }

    #[test]
    fn redact_url_userinfo_password() {
        // SEC-14（2026-08-28 R5）：URL 嵌入凭证——键名 DATABASE_URL 不含
        // SECRET_KEYWORDS，字段赋值模式漏检；密码必须脱敏
        let input = "DATABASE_URL=postgres://user:s3cret@db.example.com:5432/app\n";
        let out = redact(input);
        assert!(!out.contains("s3cret"), "URL 密码不得保留: {out}");
        assert!(out.contains("user:***@"), "应保留 user 并脱敏密码: {out}");
    }

    #[test]
    fn redact_url_userinfo_keeps_single_user() {
        // 单段 user@（无密码）形态不误伤：git@github.com 等
        let input = "GIT_URL=git@github.com:user/repo.git\n";
        let out = redact(input);
        assert_eq!(out, input, "无密码的 user@ 形态不应脱敏");
    }

    #[test]
    fn redact_url_userinfo_password_with_at() {
        // SEC-R6-6（2026-08-28 R6 审查）：密码含 `@` 时此前只脱敏到第一个 `@`，
        // `word@host` 段裸露。放宽密码字符集后整段 userinfo 脱敏。
        let input = "DATABASE_URL=postgres://user:pass@word@db.example.com/app\n";
        let out = redact(input);
        assert!(!out.contains("pass@word"), "含 @ 的密码不得保留: {out}");
        assert!(out.contains("user:***@"), "应保留 user 并脱敏: {out}");
        assert!(out.contains("db.example.com"), "host 应保留: {out}");
    }

    #[test]
    fn redact_url_userinfo_password_with_slash() {
        // 密码含 `/`（URL 内嵌凭证常见编码形态）同样整体脱敏
        let input = "DB_URL=postgres://admin:p/ss@host:5432/x\n";
        let out = redact(input);
        assert!(!out.contains("p/ss"), "含 / 的密码不得保留: {out}");
        assert!(out.contains("admin:***@"), "应保留 user 并脱敏: {out}");
    }

    #[test]
    fn redact_pem_private_key_block() {
        // R8 SEC-4：PEM 私钥是多行凭证——逐行脱敏漏检 base64 载荷。
        // 头尾 marker 保留（可辨识），载荷整体替换为 ***。
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA7D+Y\nsuper-secret-base64\n-----END RSA PRIVATE KEY-----\n";
        let out = redact(pem);
        assert!(out.contains("-----BEGIN RSA PRIVATE KEY-----"), "{out}");
        assert!(out.contains("-----END RSA PRIVATE KEY-----"), "{out}");
        assert!(
            !out.contains("MIIEowIBAAKCAQEA7D+Y"),
            "base64 载荷不得保留: {out}"
        );
        assert!(!out.contains("super-secret"), "载荷不得保留: {out}");
        assert!(out.contains("***"), "应整体替换为 ***: {out}");
    }

    #[test]
    fn redact_openssh_private_key_block() {
        // OPENSSH 变体同样命中
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nYWJjZGVmMTIzNDU2Cg==\n-----END OPENSSH PRIVATE KEY-----\n";
        let out = redact(pem);
        assert!(!out.contains("YWJjZGVmMTIzNDU2"), "{out}");
        assert!(out.contains("OPENSSH PRIVATE KEY"), "{out}");
    }

    #[test]
    fn redact_known_credential_prefixes() {
        // TL-R6-8（2026-08-28 R6 审查）：sk-/ghp_/github_pat_/xoxb- 前缀凭证
        // 在 policy::redact 路径（shell.background/output、fs.read）必须脱敏——
        // 此前仅 shell.run 的本地 redact_secrets 覆盖，两套规则不同步。
        for (input, secret) in [
            (
                "ghp_abcdef1234567890abcdef1234\n",
                "ghp_abcdef1234567890abcdef1234",
            ),
            (
                "token: sk-abcdefghijklmnopqrstuvwx\n",
                "sk-abcdefghijklmnopqrstuvwx",
            ),
            ("auth=xoxb-1234-5678-9012\n", "xoxb-1234-5678-9012"),
            (
                "github_pat_11ABC_DEFghijklmnopqrstuvwxyz\n",
                "github_pat_11ABC_DEFghijklmnopqrstuvwxyz",
            ),
        ] {
            let out = redact(input);
            assert!(!out.contains(secret), "前缀凭证不得保留: {out}");
        }
        // 前后文保留：普通日志行不被误伤
        let ctx = redact("build ok at /tmp/x\n");
        assert_eq!(ctx, "build ok at /tmp/x\n");
    }
}

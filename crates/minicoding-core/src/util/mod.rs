//! 通用小工具（无领域语义的共享骨架）。

pub mod circuit_breaker;
pub mod fs_private;
pub mod slash;

pub use circuit_breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig};
pub use fs_private::write_private;
pub use slash::{SlashCommand, parse as parse_slash};

/// 生成 API 鉴权 token（S1）：ULID 两拼接（160bit 表示，80×2 bit 随机）。
///
/// server bin / CLI `serve` / desktop sidecar 三处复用，保证生成策略一致。
#[must_use]
pub fn generate_auth_token() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new())
}

/// API token 掩码（前 4 字符 + `***`，C-04）。
///
/// R8 ARCH-5：单一事实来源——此前 `minicoding-server/main.rs`、
/// `minicoding-cli/commands/serve.rs`、`minicoding-desktop/sidecar.rs` 三处
/// 各自内联同语义掩码（可漂移）。日志/输出脱敏统一走此函数；`len <= 4`
/// 时整体掩码。
#[must_use]
pub fn mask_token(token: &str) -> String {
    let cut = token.char_indices().nth(4).map_or(token.len(), |(i, _)| i);
    if cut == 0 || token.chars().count() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &token[..cut])
    }
}

/// 检测内容是否含指令性模式（C-27：Auto memory 指令注入降级 `Ask`）。
///
/// 单一事实来源（2026-08-23 审查 §8-P2）：此前 `minicoding-policy::builtin`
/// 与 `minicoding-memory::auto` 各持一份同语义实现（领域 crate 不交叉的约束
/// 下只能复制），任一侧加规则另一侧不同步即出现旁路。两 crate 均依赖 core，
/// 下沉至此零新增依赖边。
///
/// CTX-1/SEC-4（2026-08-26 R3 审查）重写匹配算法：旧版仅做**行首字面前缀**
/// 匹配，Markdown 列表（`- Never ...`，AGENTS.md 最常见形态）、有序列表、
/// 加粗强调、多级标题、敬语前缀等常规写法全部漏检——auto.md 构成跨会话全局
/// 持久注入通道。现版本先**剥离 Markdown 修饰前缀**（列表符/引用/任意级标题/
/// 有序列表号），再对剥离后的行首做祈使词匹配；section 头判定同步放宽为
/// 任意标题级别 + 扩充词表。中文 `应` 单字前缀误报率高（"应用服务器"），
/// 收紧为双字以上组合。
///
/// 命中任一模式返回 `true`：
/// - 英文祈使/模态（剥前缀后行首，忽略大小写）：`Always use`/`Never`/`Must`/
///   `Do not`/`Don't`/`Should`；
/// - 中文祈使（剥前缀后行首）：`总是`/`永远`/`禁止`/`必须`/`不要`/`不得`/
///   `应当`/`应该`/`务必`/`切记`；
/// - section 头（剥 `#` 后词首）：`Rules`/`Constraints`/`Guidelines`/
///   `Instructions`/`Conventions`/`规则`/`约束`/`规范`（后随行尾/冒号/空白）。
///
/// 诚实边界：本函数是启发式（无法覆盖 base64 编码、语义级指令改写等变形），
/// 是 C-27 纵深防御的一层而非全部——命中后降级 Ask + 注入侧 `<auto_memory>`
/// data-not-instructions 边界声明共同兜底。
#[must_use]
pub fn contains_directive(content: &str) -> bool {
    for raw in content.lines() {
        // R4（RT4-6）：先做行级归一化——零宽字符（U+200B 等）插入单词内部
        // 会切断 `word_end` 判定（`Ne\u{200b}ver force push` 此前只取到
        // `ne` 漏检，零宽字符是指令注入对抗过滤的经典手法）；连续空白折叠
        // 使 `Do  not` 变体可匹配；整行 HTML 注释是隐藏指令的常见形态。
        let normalized = normalize_directive_line(raw);
        if normalized.is_empty() {
            continue;
        }
        let stripped = strip_markdown_prefixes(&normalized);
        if stripped.is_empty() {
            continue;
        }
        let lower = stripped.to_ascii_lowercase();
        // 词级祈使判定（取首个字母词）：覆盖 `Always use/run/...`、
        // `Never** force push`（强调符紧随）等形态
        let word_end = lower
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(lower.len());
        let first_word = &lower[..word_end];
        if matches!(first_word, "always" | "never" | "must" | "should")
            || lower.starts_with("do not ")
            || lower.starts_with("do not.")
            || lower.starts_with("don't ")
            || lower.starts_with("don't.")
        {
            return true;
        }
        // 中文祈使：双字以上组合（`应` 单字误报率高，CTX-15）
        for kw in [
            "总是", "永远", "禁止", "必须", "不要", "不得", "应当", "应该", "务必", "切记",
        ] {
            if stripped.starts_with(kw) {
                return true;
            }
        }
        // section 头：剥掉标题符后按词首匹配（`## Rules` / `### 规则：` 等）
        if is_section_header(&lower, stripped) {
            return true;
        }
    }
    false
}

/// 指令行归一化（R4 RT4-6）：剥离零宽/格式字符、折叠连续空白、剔除整行
/// HTML 注释。返回 `""` 表示该行为纯注释/空内容，跳过。
fn normalize_directive_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.starts_with("<!--") && trimmed.ends_with("-->") {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_space = false;
    for c in trimmed.chars() {
        // SEC-R6-7（2026-08-28 R6 审查）：剥离所有 Unicode `Cf`（Format）类
        // 字符——此前仅硬编码 5 个零宽字符（ZWNJ/ZWJ/BOM/软连字符），`\u{2060}`
        // WORD JOINER 等约 160 个 `Cf` 类成员可插入指令词中绕过祈使检测
        // （`A\u{2060}lways use sudo` 写进 auto.md，C-27 降级通道被架空）。
        if is_format_control(c) {
            continue;
        }
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// 是否为 Unicode `General_Category = Format (Cf)` 字符。
///
/// 范围按 Unicode 15.0 的 `General_Category=Format` 属性整理（无需依赖
/// unicode-general-category crate，保持 core 轻量；范围集稳定，Unicode 新版本
/// 追加仅影响极端新字符）。`Cf` 类字符不打印、非空白，用于文本格式标记
/// （零宽连接/隔离、BOM、软连字符、不可见运算符等）——攻击者插入指令词中
/// 可绕过词法检测。
#[must_use]
fn is_format_control(c: char) -> bool {
    // 区间表（起点闭、终点闭；单点用 [x,x]）——Unicode 15.0 General_Category=Format
    const RANGES: &[(u32, u32)] = &[
        (0x00AD, 0x00AD), // SOFT HYPHEN
        (0x0600, 0x0605), // ARABIC NUMBER SIGN..MARK
        (0x061C, 0x061C), // ARABIC LETTER MARK
        (0x06DD, 0x06DD), // ARABIC END OF AYAH
        (0x070F, 0x070F), // SYRIAC ABBREVIATION MARK
        (0x0890, 0x0891), // ARABIC POUND/PLUS MARK
        (0x08E2, 0x08E2), // ARABIC DISPUTED END OF AYAH
        (0x180E, 0x180E), // MONGOLIAN VOWEL SEPARATOR
        (0x200B, 0x200F), // ZWSP..RLM
        (0x202A, 0x202E), // LRE..RLRO
        (0x2060, 0x2064), // WORD JOINER..INVISIBLE PLUS
        (0x2066, 0x206F), // LRI..NNBSP(部分 Cf)
        (0xFEFF, 0xFEFF), // BOM / ZWNBSP
        (0xFFF9, 0xFFFB), // INTERLINEAR ANNOTATION..
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x13438),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0001, 0xE0001),
        (0xE0020, 0xE007F),
    ];
    let cp = u32::from(c);
    RANGES.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// 迭代剥离行首 Markdown 修饰：无序列表符（`-`/`*`/`+`）、引用（`>`）、
/// 任意级标题（`#`）、有序列表号（`\d+[.)]`）、强调符号（`*`/`_`/`` ` ``）。
/// 循环直到稳定，处理 `- **必须**...` 这类嵌套修饰。
fn strip_markdown_prefixes(raw: &str) -> &str {
    let mut s = raw.trim();
    loop {
        let before = s;
        s = s.trim_start_matches(['-', '*', '+', '>', '#', '_', '`', ' ']);
        // 有序列表："1." / "12)"
        let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            let rest = &s[digits..];
            if rest.starts_with('.') || rest.starts_with(')') {
                s = rest[1..].trim_start();
            }
        } else {
            s = s.trim_start();
        }
        if s == before {
            return s;
        }
    }
}

/// 判定是否 section 头：英文关键词后随行尾/冒号/空白；中文关键词后随行尾/冒号。
fn is_section_header(lower: &str, original: &str) -> bool {
    for en in [
        "rules",
        "constraints",
        "guidelines",
        "instructions",
        "conventions",
    ] {
        if let Some(rest) = lower.strip_prefix(en)
            && (rest.is_empty()
                || rest.starts_with(':')
                || rest.starts_with(' ')
                || rest.starts_with("：")
                || rest.starts_with("**"))
        {
            return true;
        }
    }
    for zh in ["规则", "约束", "规范"] {
        if let Some(rest) = original.strip_prefix(zh)
            && (rest.is_empty() || rest.starts_with('：') || rest.starts_with(':'))
        {
            return true;
        }
    }
    false
}

/// 测试用当前时间戳（`OffsetDateTime`，供无 `time` 依赖的下游 crate 测试构造
/// `Message` 使用；生产路径一律 `OffsetDateTime::now_utc()`）。
#[must_use]
pub fn test_now_utc() -> time::OffsetDateTime {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    time::OffsetDateTime::from_unix_timestamp(unix.as_secs().cast_signed())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

/// OS keyring 服务名（ARCH-4，2026-08-26 R3 审查：单一事实来源——此前
/// cli/sdk/server/desktop 四处各自私有复制，一处改名即静默 split-brain，
/// CLI 存的 key server 读不到且编译器无法捕获）。C-04 三端共享凭证语义。
pub const KEYRING_SERVICE: &str = "minicoding";

/// OS keyring 账户名（OpenAI API key 条目；见 [`KEYRING_SERVICE`]）。
pub const KEYRING_ACCOUNT: &str = "openai_api_key";

/// 相对路径词法规范化（SEC-3，2026-08-25 R2 审查）：消除 `.`/`..` 段与重复
/// 分隔符，供权限持久化的目录前缀匹配使用。
///
/// 纯词法操作（不触碰文件系统、不解 symlink）：与 [`std::path::Path::components`]
/// 的语义一致——`..` 弹出上一段，栈空时保留 `..`（相对路径语义不变式）。
/// 前缀匹配必须基于规范化后的路径，否则 `src/gen/../secret.txt` 会被裸
/// `starts_with` 误判进已批准的 `src/gen` 目录范围。
#[must_use]
pub fn normalize_lexical_rel_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            _ => parts.push(comp),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod path_tests {
    use super::normalize_lexical_rel_path as norm;

    #[test]
    fn resolves_dot_and_parent_segments() {
        // 关键用例（SEC-3）：`..` 使路径逃出已批准的 src/gen 目录
        assert_eq!(norm("src/gen/../secret.txt"), "src/secret.txt");
        assert_eq!(norm("src/./a.rs"), "src/a.rs");
        assert_eq!(norm("src//b.rs"), "src/b.rs");
    }

    #[test]
    fn leading_parent_segments_are_preserved() {
        // 栈空的 .. 保留——调用方（workdir 包容校验）会拒绝越界路径，
        // 此处只保证规范化不改变语义。
        assert_eq!(norm("../outside/x"), "../outside/x");
        assert_eq!(norm("src/../../x"), "../x");
    }

    #[test]
    fn normal_and_empty_inputs() {
        assert_eq!(norm("src/gen/a.rs"), "src/gen/a.rs");
        assert_eq!(norm(""), "");
        assert_eq!(norm("."), "");
        assert_eq!(norm("a/.."), "");
    }

    // R8 ARCH-5：掩码统一语义（前 4 字符 + ***；短值整体掩码）
    #[test]
    fn mask_token_keeps_first_four_chars() {
        assert_eq!(super::mask_token("abcdef"), "abcd***");
        assert_eq!(super::mask_token("ab"), "***");
        assert_eq!(super::mask_token(""), "***");
        // 多字节字符：按 char 边界截取前 4 个字符
        assert_eq!(super::mask_token("一二三四五六"), "一二三四***");
    }
}

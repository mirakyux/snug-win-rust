//! 拼音搜索（对应 C# 类：`SnugWin.Core.PhoneScreen.PinyinSearch`）。
//!
//! 让中文应用名、窗口标题可以用全拼或首字母搜到，例如 `bwl` → 备忘录、`wx` → 微信。
//! 行为逐条照抄 C# 版：
//! - 字符遍历、大小写、空白处理一致；
//! - 拼音来自 GB2312 一级汉字表，多音字额外展开一个候选读音；
//! - `Matches` / `Score` / `SortKey` 的判定顺序与返回值一致。
//!
//! 与 C# 版的唯一实现差异是拼音表的来历：C# 运行时用 GB2312(CP936) 编码 + 区间表二分，
//! Rust 没有编码依赖，于是把同一算法的结果离线展开成静态表（见 [`gb2312`] 与
//! `tools/gen_pinyin_table.ps1`）。`tests::matches_reference_corpus` 用参考实现导出的
//! 语料逐条对照，保证两边行为一致。

mod gb2312;

use std::collections::HashMap;
use std::sync::OnceLock;

/// 多音字的候选读音（对应 C# `PinyinSearch.Polyphones`）。
///
/// 顺序照抄 C#：第 0 项是 `ExpandVariants` 里替换用的"备选读音"，
/// 基础读音则来自 GB2312 表（例如 重 的基础读音是 zhong，备选是 chong）。
const POLYPHONES: &[(char, [&str; 2])] = &[
    ('乐', ["yue", "le"]),
    ('行', ["hang", "xing"]),
    ('重', ["chong", "zhong"]),
    ('调', ["tiao", "diao"]),
    ('藏', ["cang", "zang"]),
    ('长', ["chang", "zhang"]),
    ('传', ["chuan", "zhuan"]),
];

/// 拼音 → 汉字 的静态表展开成 汉字 → 拼音 的查表（只建一次）。
fn table() -> &'static HashMap<char, &'static str> {
    static TABLE: OnceLock<HashMap<char, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::with_capacity(8192);
        for (pinyin, glyphs) in gb2312::GB2312_GLYPHS {
            for ch in glyphs.chars() {
                map.insert(ch, *pinyin);
            }
        }
        map
    })
}

/// 单个非 ASCII 字符的读音；查不到返回 `None`。
///
/// 对应 C# `ToPinyin` 里先查 `ExtraPinyin`、再查 GB2312 区间表的两次查表；
/// 两张表的结果都已合并进 [`gb2312::GB2312_GLYPHS`]（导出自参考实现本身）。
fn glyph_pinyin(ch: char) -> Option<&'static str> {
    table().get(&ch).copied()
}

/// 多音字的候选读音表，非多音字返回 `None`。
fn polyphone(ch: char) -> Option<&'static [&'static str]> {
    POLYPHONES
        .iter()
        .find(|(c, _)| *c == ch)
        .map(|(_, readings)| readings.as_slice())
}

/// 取音节首字母（表里的音节都非空）。
fn first_letter(pinyin: &str) -> char {
    pinyin.chars().next().unwrap_or('?')
}

/// ASCII 空白判定，对齐 `char.IsWhiteSpace`：多一个垂直制表符 `U+000B`
/// （`char::is_ascii_whitespace` 不含它）。
fn is_ascii_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r')
}

/// 单字符的小写映射，对齐 `char.ToLowerInvariant`：只做一对一映射，
/// 遇到会展开成多个字符的（如 `İ`）保持原样。
fn simple_lower(ch: char) -> char {
    let mut it = ch.to_lowercase();
    match (it.next(), it.next()) {
        (Some(low), None) => low,
        _ => ch,
    }
}

/// 单字符的大写映射，语义同 [`simple_lower`]；无法一对一映射时返回 `None`。
fn simple_upper(ch: char) -> Option<char> {
    let mut it = ch.to_uppercase();
    match (it.next(), it.next()) {
        (Some(up), None) => Some(up),
        _ => None,
    }
}

/// 逐字符比较是否忽略大小写（对应 .NET `StringComparison.OrdinalIgnoreCase`）。
fn eq_ignore_case(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    match (simple_upper(a), simple_upper(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 忽略大小写的子串查找（对应 .NET `string.Contains(q, StringComparison.OrdinalIgnoreCase)`）。
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        return n.len() <= h.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    let h: Vec<char> = haystack.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    n.len() <= h.len() && h.windows(n.len()).any(|w| w.iter().zip(&n).all(|(a, b)| eq_ignore_case(*a, *b)))
}

/// 对应 C# `string.IsNullOrWhiteSpace`。
fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// 对应 C# 的 `query.Trim().ToLowerInvariant()`：先去掉两端 Unicode 空白，再逐字符小写。
fn trim_lower(text: &str) -> String {
    text.trim_matches(char::is_whitespace)
        .chars()
        .map(simple_lower)
        .collect()
}

/// 把一个字符的音（或它本身）追加到全拼与首字母缓冲里。
fn push_char(full: &mut String, initials: &mut String, ch: char) {
    if (ch as u32) <= 127 {
        // C# 对 ASCII 先小写、再跳过空白字符：空格不会进入全拼，也不进入首字母。
        let low = ch.to_ascii_lowercase();
        if !is_ascii_space(low) {
            full.push(low);
            initials.push(low);
        }
        return;
    }

    if let Some(pinyin) = glyph_pinyin(ch) {
        full.push_str(pinyin);
        initials.push(first_letter(pinyin));
        return;
    }

    // 兜底与 C# 相同：查不到拼音就原样追加小写字符（如繁体、emoji 后的代理项等）。
    let low = simple_lower(ch);
    full.push(low);
    initials.push(low);
}

/// 转成全拼与首字母（对应 C# `PinyinSearch.ToPinyin`）。
///
/// 返回 `(全拼, 首字母)`；空输入返回两个空串。
pub fn to_pinyin(text: &str) -> (String, String) {
    if text.is_empty() {
        return (String::new(), String::new());
    }

    let mut full = String::with_capacity(text.len() * 3);
    let mut initials = String::with_capacity(text.len());
    for ch in text.chars() {
        push_char(&mut full, &mut initials, ch);
    }
    (full, initials)
}

/// 基础读音 + 一个多音字备选读音（对应 C# `PinyinSearch.ExpandVariants`）。
fn expand_variants(text: &str) -> Vec<(String, String)> {
    let base = to_pinyin(text);
    if text.is_empty() || !text.chars().any(|ch| polyphone(ch).is_some()) {
        return vec![base];
    }

    let mut alt_full = String::with_capacity(text.len() * 3);
    let mut alt_init = String::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(readings) = polyphone(ch) {
            // C# 用的是 readings[0]（列表第一项），不是"第二个读音"。
            let pinyin = readings[0];
            alt_full.push_str(pinyin);
            alt_init.push(first_letter(pinyin));
        } else {
            push_char(&mut alt_full, &mut alt_init, ch);
        }
    }
    vec![base, (alt_full, alt_init)]
}

/// 文本是否匹配查询（对应 C# `PinyinSearch.Matches`）。
///
/// 空白查询恒匹配；否则先做原文的忽略大小写子串匹配，再用全拼/首字母匹配
/// （查询里 ASCII 空格会被去掉）。
pub fn matches(text: &str, query: &str) -> bool {
    if is_blank(query) {
        return true;
    }
    let query = trim_lower(query);
    if contains_ignore_case(text, &query) {
        return true;
    }

    let compact = query.replace(' ', "");
    for (full, initials) in expand_variants(text) {
        if full.contains(&compact) || initials.contains(&compact) {
            return true;
        }
    }
    false
}

/// 排序键（对应 C# `PinyinSearch.SortKey`）：空白文本返回空串，否则返回全拼。
pub fn sort_key(text: &str) -> String {
    if is_blank(text) {
        return String::new();
    }
    to_pinyin(text).0
}

/// 搜索权重（对应 C# `PinyinSearch.Score`）。
///
/// `2` = 前缀/首字母匹配，`1` = 子串匹配，`0` = 不匹配（空白查询也是 0）。
pub fn score(text: &str, query: &str) -> i32 {
    if is_blank(query) {
        return 0;
    }
    let query = trim_lower(query).replace(' ', "");
    if query.is_empty() {
        return 0;
    }

    let normalized = trim_lower(text).replace(' ', "");
    if normalized.starts_with(&query) {
        return 2;
    }

    let mut best = if normalized.contains(&query) { 1 } else { 0 };
    for (full, initials) in expand_variants(text) {
        if full.starts_with(&query) || initials.starts_with(&query) {
            return 2;
        }
        if best == 0 && (full.contains(&query) || initials.contains(&query)) {
            best = 1;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转写自 C# `PinyinAndProtocolTests.PinyinSearch_MatchesChineseAndInitials`。
    #[test]
    fn matches_chinese_and_initials_ported_from_csharp() {
        let cases = [
            ("备忘录", "bwl", true),
            ("备忘录", "beiwanglu", true),
            ("微信", "wx", true),
            ("微信", "weixin", true),
            ("记事本", "jsb", true),
            ("终端", "zd", true),
            ("Visual Studio Code", "vscode", false),
            ("Visual Studio Code", "studio", true),
        ];
        for (name, query, expected) in cases {
            assert_eq!(matches(name, query), expected, "Matches({name:?}, {query:?})");
        }
    }

    #[test]
    fn to_pinyin_matches_csharp_reference() {
        // 期望值由参考实现（.NET 10 + PinyinSearch.cs）导出。
        let cases = [
            ("备忘录", ("beiwanglu", "bwl")),
            ("微信", ("weixin", "wx")),
            ("记事本", ("jishiben", "jsb")),
            ("终端", ("zhongduan", "zd")),
            ("Visual Studio Code", ("visualstudiocode", "visualstudiocode")),
            // 首尾的 ASCII 空格被丢弃，字间空格也不会进入全拼。
            ("  微 信  ", ("weixin", "wx")),
            ("哔哩哔哩", ("bilibili", "blbl")),
            ("钉钉", ("dingding", "dd")),
            ("剪映", ("jianying", "jy")),
            ("豆瓣", ("douban", "db")),
            ("拼多多", ("pinduoduo", "pdd")),
            ("闲鱼", ("xianyu", "xy")),
        ];
        for (text, (full, initials)) in cases {
            assert_eq!(to_pinyin(text), (full.to_string(), initials.to_string()), "ToPinyin({text:?})");
        }
    }

    #[test]
    fn empty_input_maps_to_empty_strings() {
        assert_eq!(to_pinyin(""), (String::new(), String::new()));
        assert_eq!(sort_key(""), "");
        assert_eq!(sort_key("   "), "");
        assert_eq!(sort_key("\u{3000}"), "");
    }

    #[test]
    fn blank_query_always_matches_but_scores_zero() {
        for text in ["微信", "Visual Studio Code", ""] {
            assert!(matches(text, ""));
            assert!(matches(text, "   "));
            assert!(matches(text, "\u{3000}")); // 全角空格也算空白
            assert_eq!(score(text, ""), 0);
            assert_eq!(score(text, "   "), 0);
            assert_eq!(score(text, "\u{3000}"), 0);
        }
    }

    #[test]
    fn ascii_query_is_case_insensitive_and_ignores_inner_spaces_only_for_pinyin() {
        assert!(matches("微信", "WX"));
        assert!(matches("微信", "weixin"));
        // 查询里的 ASCII 空格会被去掉，所以 "wei xin" 仍能匹配到全拼 weixin。
        assert!(matches("微信", "wei xin"));
        assert!(matches("Notepad", "NOTEPAD"));
        assert!(matches("task manager", "taskman"));
        assert_eq!(score("微信", "wx"), 2);
        assert_eq!(score("微信", "wx "), 2);
    }

    #[test]
    fn fullwidth_space_is_kept_in_pinyin_but_queried_as_blank() {
        // C# 只对 ASCII 空白跳过；全角空格 U+3000 会原样进入全拼与首字母。
        assert_eq!(to_pinyin("微　信"), ("wei　xin".to_string(), "w　x".to_string()));
        // 因此用 "wx" 搜不到它（行为照抄 C#，不必"顺手优化"）。
        assert!(!matches("微　信", "wx"));
        // 但全角空格本身作为查询是空白查询：恒匹配。
        assert!(matches("微　信", "\u{3000}"));
        // 查询里的全角空格不会被去掉（C# 只 Replace(" ")），也不参与原文匹配。
        assert!(!matches("微信", "微　信"));
    }

    #[test]
    fn polyphones_expand_one_alternate_reading() {
        // 基础读音来自 GB2312 表：重 zhong、乐 le、行 xing、长 chang。
        assert_eq!(to_pinyin("重"), ("zhong".to_string(), "z".to_string()));
        assert_eq!(to_pinyin("乐"), ("le".to_string(), "l".to_string()));

        // 备选读音是 C# 列表里的第 0 项：chong / yue / hang / zhang。
        assert!(matches("重庆", "chongqing"));
        assert!(matches("重庆", "zhongqing"));
        assert!(matches("重庆", "cq"));
        assert!(matches("重庆", "zq"));
        assert_eq!(score("重庆", "chong"), 2);
        assert_eq!(score("重庆", "cq"), 2);
        assert_eq!(score("重庆", "zq"), 2);

        assert!(matches("音乐", "yinyue"));
        assert!(matches("银行", "yinhang"));
        assert!(matches("调整", "tiaozheng"));

        // 照抄 C# 版的行为（含它的毛病）：ExpandVariants 取的是 readings[0]，
        // 而 长/藏/传 的 readings[0] 恰好等于 GB2312 表里的基础读音，
        // 于是它们真正想兜住的 zhang / zang / zhuan 反而搜不到。
        // C# 侧同样搜不到（见参考语料 "Q 长 zhang 0 0"），这里如实记录，不顺手修。
        assert!(!matches("长大", "zhangda"));
        assert!(!matches("藏区", "zangqu"));
        assert!(!matches("传奇", "zhuanqi"));
        assert!(matches("长大", "changda"));
        assert!(matches("藏区", "cangqu"));
        assert!(matches("传奇", "chuanqi"));

        // 没有多音字时只有一个读音，不额外展开。
        assert!(matches("微信", "weixin"));
    }

    #[test]
    fn score_ranks_prefix_before_substring() {
        assert_eq!(score("微信", "weixin"), 2);
        assert_eq!(score("微信输入法", "weixin"), 2);
        assert_eq!(score("微信输入法", "shuru"), 1);
        assert_eq!(score("微信输入法", "zzz"), 0);
        assert_eq!(score("微信", "y"), 0);
        assert_eq!(sort_key("微信"), "weixin");
    }

    /// 拼音表本身的结构检查：无重复字、无空读音、数量与参考实现导出的真值一致。
    #[test]
    fn gb2312_table_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for (pinyin, glyphs) in gb2312::GB2312_GLYPHS {
            assert!(!pinyin.is_empty());
            assert!(pinyin.chars().all(|ch| ch.is_ascii_lowercase()));
            assert!(!glyphs.is_empty(), "读音 {pinyin} 没有对应汉字");
            for ch in glyphs.chars() {
                assert!(ch as u32 > 127, "表里混进了 ASCII 字符 {ch:?}");
                assert!(seen.insert(ch), "重复汉字 {ch}");
            }
        }
        // 参考实现（.NET 10 + PinyinSearch.cs）导出 7758 个字，其中含 GB2312 的
        // best-fit 异体字；数量对不上说明表被改坏了。
        assert_eq!(seen.len(), 7758);
        assert_eq!(table().len(), 7758);
        assert_eq!(table().get(&'微'), Some(&"wei"));
        assert_eq!(table().get(&'重'), Some(&"zhong"));
        assert_eq!(table().get(&'哔'), Some(&"bi"));
    }

    /// 与参考实现的语料逐条对照（附带 `sort_key`；对界面排序行为至关重要）。
    #[test]
    fn matches_reference_corpus() {
        let corpus = include_str!("pinyin/corpus.tsv");
        let mut texts = 0;
        let mut queries = 0;
        for line in corpus.lines() {
            let mut parts = line.split('\t');
            match parts.next() {
                Some("T") => {
                    let text = parts.next().unwrap();
                    let full = parts.next().unwrap();
                    let initials = parts.next().unwrap();
                    let key = parts.next().unwrap();
                    assert_eq!(to_pinyin(text), (full.to_string(), initials.to_string()), "ToPinyin({text:?})");
                    assert_eq!(sort_key(text), key, "SortKey({text:?})");
                    texts += 1;
                }
                Some("Q") => {
                    let text = parts.next().unwrap();
                    let query = parts.next().unwrap();
                    let expected_match = parts.next().unwrap() == "1";
                    let expected_score: i32 = parts.next().unwrap().parse().unwrap();
                    assert_eq!(matches(text, query), expected_match, "Matches({text:?}, {query:?})");
                    assert_eq!(score(text, query), expected_score, "Score({text:?}, {query:?})");
                    queries += 1;
                }
                other => panic!("语料格式错误：{other:?} in {line:?}"),
            }
        }
        assert!(texts >= 50, "语料文本太少：{texts}");
        assert!(queries >= 3000, "语料查询太少：{queries}");
    }
}

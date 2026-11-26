//! UTF-16 小工具。Win32 的 `*W` 接口全都吃 UTF-16，几个模块都要用。

/// 转成以 `0` 结尾的 UTF-16 缓冲区，直接喂给 `PCWSTR`。
pub fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从定长 UTF-16 缓冲区取字符串：读到第一个 `0` 为止。
pub fn from_wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// `str::contains` 的大小写不敏感版本（对应 C# `StringComparison.OrdinalIgnoreCase`）。
pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wide_appends_terminator() {
        assert_eq!(to_wide("AB"), vec![65, 66, 0]);
        assert_eq!(to_wide(""), vec![0]);
    }

    #[test]
    fn from_wide_stops_at_terminator() {
        assert_eq!(from_wide(&[65, 66, 0, 67]), "AB");
        assert_eq!(from_wide(&[65, 66]), "AB");
        assert_eq!(from_wide(&[]), "");
    }

    #[test]
    fn ignore_case_matches_like_dotnet() {
        assert!(contains_ignore_case("Virtual Display", "virtual"));
        assert!(contains_ignore_case("GameViewer", "gameviewer"));
        assert!(!contains_ignore_case("DELL U2720Q", "virtual"));
    }
}

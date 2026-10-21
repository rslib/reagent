//! Cuts of text on char boundaries.

/// The longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    &s[..s.floor_char_boundary(max)]
}

/// The longest suffix of `s` that is at most `max` bytes and starts on a char boundary.
pub fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    &s[s.ceil_char_boundary(s.len() - max)..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_short_text_whole() {
        assert_eq!(head("abc", 3), "abc");
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(head("", 0), "");
        assert_eq!(head("", 5), "");
    }

    #[test]
    fn head_cuts_ascii_at_max() {
        assert_eq!(head("abcdef", 4), "abcd");
        assert_eq!(head("abcdef", 0), "");
    }

    #[test]
    fn head_backs_off_a_split_character() {
        assert_eq!(head("a\u{e9}b", 2), "a");
        assert_eq!(head("a\u{e9}b", 3), "a\u{e9}");
        assert_eq!(head("\u{1f600}x", 3), "");
        assert_eq!(head("\u{1f600}x", 4), "\u{1f600}");
        assert_eq!(head("\u{e9}", 0), "");
    }

    #[test]
    fn tail_keeps_short_text_whole() {
        assert_eq!(tail("abc", 3), "abc");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("", 0), "");
        assert_eq!(tail("", 5), "");
    }

    #[test]
    fn tail_cuts_ascii_at_max() {
        assert_eq!(tail("abcdef", 4), "cdef");
        assert_eq!(tail("abcdef", 0), "");
    }

    #[test]
    fn tail_advances_past_a_split_character() {
        assert_eq!(tail("a\u{e9}b", 2), "b");
        assert_eq!(tail("a\u{e9}b", 3), "\u{e9}b");
        assert_eq!(tail("x\u{1f600}", 3), "");
        assert_eq!(tail("x\u{1f600}", 4), "\u{1f600}");
        assert_eq!(tail("\u{e9}", 0), "");
    }
}

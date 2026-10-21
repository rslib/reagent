//! Git's wildmatch glob (as git matches ignore patterns), shared by the
//! ignore rules and the permission rules.

const MATCH: i8 = 0;
const NOMATCH: i8 = 1;
const ABORT_ALL: i8 = -1;
const ABORT_TO_STARSTAR: i8 = -2;

fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// Matches `text` against the glob `pattern` as git does for paths: `*` and
/// `?` stop at `/`, and `**/` matches zero or more folders.
pub fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    dowild(pattern, 0, text, 0) == MATCH
}

fn class_matches(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        b"space" => matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

/// Whether `c` has a meaning in a glob.
pub fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

fn dowild(pat: &[u8], mut p: usize, text: &[u8], mut t: usize) -> i8 {
    loop {
        let mut p_ch = at(pat, p);
        if p_ch == 0 {
            break;
        }
        let mut t_ch = at(text, t);
        if t_ch == 0 && p_ch != b'*' {
            return ABORT_ALL;
        }
        match p_ch {
            b'\\' => {
                p += 1;
                if t_ch != at(pat, p) {
                    return NOMATCH;
                }
            }
            b'?' => {
                if t_ch == b'/' {
                    return NOMATCH;
                }
            }
            b'*' => {
                p += 1;
                let mut match_slash = false;
                if at(pat, p) == b'*' {
                    let first = p - 1;
                    while at(pat, p) == b'*' {
                        p += 1;
                    }
                    let c = at(pat, p);
                    if (first == 0 || pat[first - 1] == b'/')
                        && (c == 0 || c == b'/' || (c == b'\\' && at(pat, p + 1) == b'/'))
                    {
                        // `**/` may match nothing: try the rest right here.
                        if c == b'/' && dowild(pat, p + 1, text, t) == MATCH {
                            return MATCH;
                        }
                        match_slash = true;
                    }
                }
                if at(pat, p) == 0 {
                    if !match_slash && text[t.min(text.len())..].contains(&b'/') {
                        return NOMATCH;
                    }
                    return MATCH;
                } else if !match_slash && at(pat, p) == b'/' {
                    // One star before a slash matches up to the next directory.
                    match text[t.min(text.len())..].iter().position(|b| *b == b'/') {
                        Some(off) => t += off,
                        None => return NOMATCH,
                    }
                } else {
                    loop {
                        if t_ch == 0 {
                            break;
                        }
                        // Skip ahead to the next literal the star is followed by.
                        if !is_glob_special(at(pat, p)) {
                            let want = at(pat, p);
                            loop {
                                t_ch = at(text, t);
                                if t_ch == 0 || (!match_slash && t_ch == b'/') || t_ch == want {
                                    break;
                                }
                                t += 1;
                            }
                            if t_ch != want {
                                return NOMATCH;
                            }
                        }
                        let matched = dowild(pat, p, text, t);
                        if matched != NOMATCH {
                            if !match_slash || matched != ABORT_TO_STARSTAR {
                                return matched;
                            }
                        } else if !match_slash && t_ch == b'/' {
                            return ABORT_TO_STARSTAR;
                        }
                        t += 1;
                        t_ch = at(text, t);
                    }
                    return ABORT_ALL;
                }
            }
            b'[' => {
                p += 1;
                p_ch = at(pat, p);
                if p_ch == b'^' {
                    p_ch = b'!';
                }
                let negated = p_ch == b'!';
                if negated {
                    p += 1;
                    p_ch = at(pat, p);
                }
                let mut prev_ch = 0u8;
                let mut matched = false;
                loop {
                    if p_ch == 0 {
                        return ABORT_ALL;
                    }
                    if p_ch == b'\\' {
                        p += 1;
                        p_ch = at(pat, p);
                        if p_ch == 0 {
                            return ABORT_ALL;
                        }
                        if t_ch == p_ch {
                            matched = true;
                        }
                    } else if p_ch == b'-'
                        && prev_ch != 0
                        && at(pat, p + 1) != 0
                        && at(pat, p + 1) != b']'
                    {
                        p += 1;
                        p_ch = at(pat, p);
                        if p_ch == b'\\' {
                            p += 1;
                            p_ch = at(pat, p);
                            if p_ch == 0 {
                                return ABORT_ALL;
                            }
                        }
                        if t_ch <= p_ch && t_ch >= prev_ch {
                            matched = true;
                        }
                        p_ch = 0;
                    } else if p_ch == b'[' && at(pat, p + 1) == b':' {
                        let s = p + 2;
                        p = s;
                        while at(pat, p) != 0 && at(pat, p) != b']' {
                            p += 1;
                        }
                        if at(pat, p) == 0 {
                            return ABORT_ALL;
                        }
                        if p == s || pat[p - 1] != b':' {
                            // No ":]": the "[" is an ordinary member.
                            p = s - 2;
                            p_ch = b'[';
                            if t_ch == p_ch {
                                matched = true;
                            }
                        } else {
                            match class_matches(&pat[s..p - 1], t_ch) {
                                Some(true) => matched = true,
                                Some(false) => {}
                                None => return ABORT_ALL,
                            }
                            p_ch = 0;
                        }
                    } else if t_ch == p_ch {
                        matched = true;
                    }
                    prev_ch = p_ch;
                    p += 1;
                    p_ch = at(pat, p);
                    if p_ch == b']' {
                        break;
                    }
                }
                if matched == negated || t_ch == b'/' {
                    return NOMATCH;
                }
            }
            _ => {
                if t_ch != p_ch {
                    return NOMATCH;
                }
            }
        }
        p += 1;
        t += 1;
    }
    if at(text, t) != 0 { NOMATCH } else { MATCH }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        wildmatch(pattern.as_bytes(), text.as_bytes())
    }

    #[test]
    fn a_star_stays_in_one_folder() {
        assert!(m("*.rs", "a.rs"));
        assert!(!m("*.rs", "src/a.rs"));
    }

    #[test]
    fn a_double_star_crosses_folders() {
        assert!(m("**/*.rs", "a.rs"));
        assert!(m("**/*.rs", "src/deep/a.rs"));
        assert!(m("secrets/**", "secrets/key"));
        assert!(m("secrets/**", "secrets/a/b"));
        assert!(!m("secrets/**", "public/key"));
    }

    #[test]
    fn classes_and_question_marks_match_one_char() {
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "a/c"));
        assert!(m("[ab].txt", "b.txt"));
        assert!(!m("[!ab].txt", "a.txt"));
    }

    #[test]
    fn glob_special_chars() {
        assert!(is_glob_special(b'*') && is_glob_special(b'['));
        assert!(!is_glob_special(b'a'));
    }
}

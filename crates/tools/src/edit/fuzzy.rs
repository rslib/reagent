//! Whole-line fallback matching, in the order of Codex `seek_sequence`.

/// How loosely a line is compared. Looser tiers run only when stricter ones
/// find nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tier {
    TrimEnd,
    Trim,
    Fold,
}

impl Tier {
    const ALL: [Tier; 3] = [Tier::TrimEnd, Tier::Trim, Tier::Fold];

    /// What the tier did, for the result text.
    pub(super) fn describe(self) -> &'static str {
        match self {
            Tier::TrimEnd => "trimming trailing whitespace",
            Tier::Trim => "trimming whitespace",
            Tier::Fold => "normalising punctuation",
        }
    }

    fn same(self, file: &str, wanted: &str) -> bool {
        match self {
            Tier::TrimEnd => file.trim_end() == wanted.trim_end(),
            Tier::Trim => file.trim() == wanted.trim(),
            Tier::Fold => fold(file.trim()).eq(fold(wanted.trim())),
        }
    }
}

/// A matched run of whole lines in the file.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Span {
    /// Byte range of the lines to replace.
    pub(super) range: std::ops::Range<usize>,
    /// 1-based number of the first line.
    pub(super) line: usize,
}

/// Maps typographic dashes, quotes and spaces to ASCII.
fn fold(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().map(|c| match c {
        '\u{2010}'..='\u{2015}' => '-',
        '\u{2018}'..='\u{201b}' => '\'',
        '\u{201c}'..='\u{201f}' => '"',
        '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        c => c,
    })
}

/// Finds the first tier that matches `old` as whole lines of `original`.
///
/// A trailing newline of `old` makes the span include the line ending of its
/// last line.
pub(super) fn find(original: &str, old: &str) -> Option<(Tier, Vec<Span>)> {
    if old.trim().is_empty() {
        return None;
    }
    let keeps_newline = old.ends_with('\n');
    let wanted: Vec<&str> = old
        .strip_suffix('\n')
        .unwrap_or(old)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    // (start, end of content, end including the line ending)
    let mut lines: Vec<(usize, usize, usize)> = Vec::new();
    let mut start = 0;
    for raw in original.split_inclusive('\n') {
        let end = start + raw.len();
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let content = content.strip_suffix('\r').unwrap_or(content);
        lines.push((start, start + content.len(), end));
        start = end;
    }
    if lines.len() < wanted.len() {
        return None;
    }

    Tier::ALL.into_iter().find_map(|tier| {
        let mut spans = Vec::new();
        let mut i = 0;
        while i + wanted.len() <= lines.len() {
            let hit = wanted
                .iter()
                .enumerate()
                .all(|(k, w)| tier.same(content(original, lines[i + k]), w));
            if hit {
                let last = lines[i + wanted.len() - 1];
                let end = if keeps_newline { last.2 } else { last.1 };
                spans.push(Span {
                    range: lines[i].0..end,
                    line: i + 1,
                });
                i += wanted.len();
            } else {
                i += 1;
            }
        }
        (!spans.is_empty()).then_some((tier, spans))
    })
}

/// The indent the file adds in front of every non-blank line of `old`, when
/// it is the same for all of them and not empty.
pub(super) fn indent_gap<'a>(original: &'a str, span: &Span, old: &str) -> Option<&'a str> {
    let mut gap = None;
    for (file, wanted) in original[span.range.clone()].lines().zip(old.lines()) {
        if wanted.trim().is_empty() {
            continue;
        }
        let this = leading(file).strip_suffix(leading(wanted))?;
        match gap {
            None => gap = Some(this),
            Some(seen) if seen == this => {}
            Some(_) => return None,
        }
    }
    gap.filter(|gap| !gap.is_empty())
}

/// Pushes `text` with `gap` in front of every non-blank line.
pub(super) fn push_indented(out: &mut String, text: &str, gap: &str) {
    for line in text.split_inclusive('\n') {
        if !line.trim().is_empty() {
            out.push_str(gap);
        }
        out.push_str(line);
    }
}

fn leading(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn content(original: &str, line: (usize, usize, usize)) -> &str {
    &original[line.0..line.1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier_of(original: &str, old: &str) -> Option<Tier> {
        find(original, old).map(|(tier, _)| tier)
    }

    #[test]
    fn each_tier_matches() {
        assert_eq!(tier_of("a  \nb\n", "a\nb"), Some(Tier::TrimEnd));
        assert_eq!(tier_of("  a\n b\n", "a\nb"), Some(Tier::Trim));
        assert_eq!(tier_of("\u{2018}a\u{2019}\n", "'a'"), Some(Tier::Fold));
        assert_eq!(tier_of("a\n", "b"), None);
    }

    #[test]
    fn the_strictest_tier_decides() {
        assert_eq!(tier_of("  a\na \n", "a"), Some(Tier::TrimEnd));
    }

    #[test]
    fn spans_cover_the_original_lines() {
        let (_, spans) = find("x\r\n a \r\nb\r\n", "a").unwrap();
        assert_eq!(
            spans,
            vec![Span {
                range: 3..6,
                line: 2
            }]
        );
        let (_, spans) = find("x\n a\ny\n", "a\n").unwrap();
        assert_eq!(
            spans,
            vec![Span {
                range: 2..5,
                line: 2
            }]
        );
    }

    #[test]
    fn matches_do_not_overlap() {
        let (_, spans) = find(" a\n a\n a\n", "a\na").unwrap();
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn blank_text_never_matches() {
        assert_eq!(tier_of("a\n\n b\n", "\n"), None);
    }
}

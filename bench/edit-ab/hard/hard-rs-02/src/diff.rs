//! Line based diffs: a shortest edit script, hunks with context and unified text.

use std::fmt;

/// Split text into lines. A final newline does not add an empty last line.
pub fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text;
    body.split('\n').collect()
}

/// One step of an edit script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit<'a> {
    Equal(&'a str),
    Insert(&'a str),
    Delete(&'a str),
}

impl<'a> Edit<'a> {
    pub fn text(&self) -> &'a str {
        match self {
            Edit::Equal(t) | Edit::Insert(t) | Edit::Delete(t) => t,
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, Edit::Equal(_))
    }

    /// The character that starts the line in unified output.
    pub fn marker(&self) -> char {
        match self {
            Edit::Equal(_) => ' ',
            Edit::Insert(_) => '+',
            Edit::Delete(_) => '-',
        }
    }
}

// -- the edit script -------------------------------------------------------------

/// How many lines two slices share at the start.
pub fn common_prefix(a: &[&str], b: &[&str]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// How many lines two slices share at the end.
pub fn common_suffix(a: &[&str], b: &[&str]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// The edit script that turns `a` into `b`. On a tie a delete comes before an
/// insert, so replaced lines show as deletes followed by inserts.
pub fn diff_lines<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Edit<'a>> {
    let prefix = common_prefix(a, b);
    let suffix = common_suffix(&a[prefix..], &b[prefix..]);
    let middle_a = &a[prefix..a.len() - suffix];
    let middle_b = &b[prefix..b.len() - suffix];
    let mut out: Vec<Edit<'a>> = a[..prefix].iter().map(|l| Edit::Equal(l)).collect();
    out.extend(diff_middle(middle_a, middle_b));
    out.extend(a[a.len() - suffix..].iter().map(|l| Edit::Equal(l)));
    out
}

/// Table of longest common subsequence lengths of the tails of two slices.
fn lcs_table(a: &[&str], b: &[&str]) -> Vec<Vec<u32>> {
    let mut table = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

fn diff_middle<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Edit<'a>> {
    let table = lcs_table(a, b);
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len() + b.len());
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(Edit::Equal(a[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(Edit::Delete(a[i]));
            i += 1;
        } else {
            out.push(Edit::Insert(b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Edit::Delete(l)));
    out.extend(b[j..].iter().map(|l| Edit::Insert(l)));
    out
}

/// Diff two texts line by line.
pub fn diff_text<'a>(old: &'a str, new: &'a str) -> Vec<Edit<'a>> {
    diff_lines(&lines(old), &lines(new))
}

/// Length of the longest common subsequence of lines.
pub fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let prefix = common_prefix(a, b);
    let suffix = common_suffix(&a[prefix..], &b[prefix..]);
    let table = lcs_table(&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    table[0][0] as usize + prefix + suffix
}

/// Rebuild the new text from an edit script.
pub fn new_side(edits: &[Edit<'_>]) -> Vec<String> {
    edits
        .iter()
        .filter(|e| !matches!(e, Edit::Delete(_)))
        .map(|e| e.text().to_string())
        .collect()
}

/// Rebuild the old text from an edit script.
pub fn old_side(edits: &[Edit<'_>]) -> Vec<String> {
    edits
        .iter()
        .filter(|e| !matches!(e, Edit::Insert(_)))
        .map(|e| e.text().to_string())
        .collect()
}

// -- numbers about a diff -----------------------------------------------------------

/// Number of inserted lines.
pub fn added(edits: &[Edit<'_>]) -> usize {
    edits.iter().filter(|e| matches!(e, Edit::Insert(_))).count()
}

/// Number of deleted lines.
pub fn removed(edits: &[Edit<'_>]) -> usize {
    edits.iter().filter(|e| matches!(e, Edit::Insert(_))).count()
}

/// Number of lines that did not change.
pub fn unchanged(edits: &[Edit<'_>]) -> usize {
    edits.iter().filter(|e| matches!(e, Edit::Equal(_))).count()
}

/// How alike two texts are, from 0.0 (nothing shared) to 1.0 (same lines).
pub fn similarity(old: &str, new: &str) -> f64 {
    let a = lines(old);
    let b = lines(new);
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    2.0 * lcs_len(&a, &b) as f64 / (a.len() + b.len()) as f64
}

/// A one line summary such as `+3 -1 =10`.
pub fn summary(edits: &[Edit<'_>]) -> String {
    format!("+{} -{} ={}", added(edits), removed(edits), unchanged(edits))
}

/// True when the edit script has no change at all.
pub fn is_identical(edits: &[Edit<'_>]) -> bool {
    edits.iter().all(|e| !e.is_change())
}

/// Side by side view of an edit script, useful in test output. Each row is
/// the old line, the new line and a marker.
pub fn side_by_side(edits: &[Edit<'_>], width: usize) -> Vec<String> {
    let cut = |s: &str| -> String { s.chars().take(width).collect() };
    edits
        .iter()
        .map(|e| match e {
            Edit::Equal(t) => format!("{:<w$} | {:<w$}", cut(t), cut(t), w = width),
            Edit::Delete(t) => format!("{:<w$} < {:<w$}", cut(t), "", w = width),
            Edit::Insert(t) => format!("{:<w$} > {:<w$}", "", cut(t), w = width),
        })
        .collect()
}

/// Positions (0 based, in the new text) of the lines that were inserted.
pub fn inserted_lines(edits: &[Edit<'_>]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 0;
    for e in edits {
        match e {
            Edit::Equal(_) => at += 1,
            Edit::Insert(_) => {
                out.push(at);
                at += 1;
            }
            Edit::Delete(_) => {}
        }
    }
    out
}

/// Positions (0 based, in the old text) of the lines that were deleted.
pub fn deleted_lines(edits: &[Edit<'_>]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 0;
    for e in edits {
        match e {
            Edit::Equal(_) => at += 1,
            Edit::Delete(_) => {
                out.push(at);
                at += 1;
            }
            Edit::Insert(_) => {}
        }
    }
    out
}

// -- inside one line ----------------------------------------------------------------------

/// Split a line into words and runs of spaces, keeping every character.
pub fn tokens(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut last_space: Option<bool> = None;
    for (i, c) in line.char_indices() {
        let space = c.is_whitespace();
        if let Some(prev) = last_space {
            if prev != space {
                out.push(&line[start..i]);
                start = i;
            }
        }
        last_space = Some(space);
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

/// Diff two lines word by word.
pub fn diff_words<'a>(old: &'a str, new: &'a str) -> Vec<Edit<'a>> {
    diff_lines(&tokens(old), &tokens(new))
}

/// Mark the changed words of a line with `[-removed-]` and `{+added+}`.
pub fn mark_words(old: &str, new: &str) -> String {
    let mut out = String::new();
    for e in diff_words(old, new) {
        match e {
            Edit::Equal(t) => out.push_str(t),
            Edit::Delete(t) => {
                out.push_str("[-");
                out.push_str(t);
                out.push_str("-]");
            }
            Edit::Insert(t) => {
                out.push_str("{+");
                out.push_str(t);
                out.push_str("+}");
            }
        }
    }
    out
}

/// The longest run of consecutive changed edits.
pub fn longest_change_run(edits: &[Edit<'_>]) -> usize {
    let (mut best, mut run) = (0, 0);
    for e in edits {
        if e.is_change() {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// Share of edits that are changes, from 0.0 to 1.0.
pub fn churn(edits: &[Edit<'_>]) -> f64 {
    if edits.is_empty() {
        return 0.0;
    }
    edits.iter().filter(|e| e.is_change()).count() as f64 / edits.len() as f64
}

// -- grouping changes into hunks -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk<'a> {
    /// First old line, counting from 1. For an empty range this is the line
    /// before the range, so it can be 0.
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub edits: Vec<Edit<'a>>,
}

impl fmt::Display for Hunk<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

/// Group an edit script into hunks. Each hunk holds some changes and up to
/// `context` unchanged lines before and after them. Changes whose context
/// would touch or overlap end up in the same hunk.
pub fn hunks<'a>(edits: &[Edit<'a>], context: usize) -> Vec<Hunk<'a>> {
    let changes: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_change())
        .map(|(i, _)| i)
        .collect();
    if changes.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &i in &changes {
        match groups.last_mut() {
            Some((_, last)) if i - *last - 1 < 2 * context => *last = i,
            _ => groups.push((i, i)),
        }
    }
    let mut old_pos = vec![0usize; edits.len() + 1];
    let mut new_pos = vec![0usize; edits.len() + 1];
    for (k, e) in edits.iter().enumerate() {
        old_pos[k + 1] = old_pos[k] + usize::from(!matches!(e, Edit::Insert(_)));
        new_pos[k + 1] = new_pos[k] + usize::from(!matches!(e, Edit::Delete(_)));
    }
    groups
        .iter()
        .map(|&(first, last)| {
            let from = first.saturating_sub(context);
            let to = (last + 1 + context).min(edits.len());
            let old_len = old_pos[to] - old_pos[from];
            let new_len = new_pos[to] - new_pos[from];
            Hunk {
                old_start: if old_len == 0 { old_pos[from] } else { old_pos[from] + 1 },
                old_len,
                new_start: if new_len == 0 { new_pos[from] } else { new_pos[from] + 1 },
                new_len,
                edits: edits[from..to].to_vec(),
            }
        })
        .collect()
}

/// Unified diff text for two texts, or an empty string when they match.
pub fn unified(old_name: &str, new_name: &str, old: &str, new: &str, context: usize) -> String {
    let edits = diff_text(old, new);
    let groups = hunks(&edits, context);
    if groups.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {}\n+++ {}\n", old_name, new_name);
    for hunk in &groups {
        out.push_str(&hunk.to_string());
        out.push('\n');
        for edit in &hunk.edits {
            out.push(edit.marker());
            out.push_str(edit.text());
            out.push('\n');
        }
    }
    out
}

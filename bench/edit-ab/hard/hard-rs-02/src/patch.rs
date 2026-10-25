//! Read unified diff text, apply it to a file and turn it around.

use crate::diff::{lines, Edit, Hunk};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Context(String),
    Add(String),
    Del(String),
}

impl Line {
    pub fn text(&self) -> &str {
        match self {
            Line::Context(t) | Line::Add(t) | Line::Del(t) => t,
        }
    }

    pub fn marker(&self) -> char {
        match self {
            Line::Context(_) => ' ',
            Line::Add(_) => '+',
            Line::Del(_) => '-',
        }
    }

    fn on_old_side(&self) -> bool {
        !matches!(self, Line::Add(_))
    }

    fn on_new_side(&self) -> bool {
        !matches!(self, Line::Del(_))
    }

    fn inverted(&self) -> Line {
        match self {
            Line::Context(t) => Line::Context(t.clone()),
            Line::Add(t) => Line::Del(t.clone()),
            Line::Del(t) => Line::Add(t.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub old_name: String,
    pub new_name: String,
    pub hunks: Vec<PatchHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// The text does not start with `---` and `+++` lines.
    MissingHeader,
    /// An `@@` line that cannot be read.
    BadHunkHeader(String),
    /// A line in a hunk that starts with none of ` `, `+` or `-`.
    BadLine { line: usize, text: String },
    /// A hunk with a different number of lines than its header says.
    CountMismatch { hunk: usize },
    /// The old side of a hunk is not in the file. `line` counts from 1.
    Mismatch { hunk: usize, line: usize },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::MissingHeader => write!(f, "missing --- and +++ lines"),
            PatchError::BadHunkHeader(t) => write!(f, "bad hunk header {t:?}"),
            PatchError::BadLine { line, text } => write!(f, "line {line}: bad patch line {text:?}"),
            PatchError::CountMismatch { hunk } => write!(f, "hunk {hunk} has the wrong line count"),
            PatchError::Mismatch { hunk, line } => {
                write!(f, "hunk {hunk} does not match the file near line {line}")
            }
        }
    }
}

impl std::error::Error for PatchError {}

// -- reading ------------------------------------------------------------------------

/// Read `@@ -a,b +c,d @@`. A missing length means 1.
fn parse_hunk_header(text: &str) -> Result<(usize, usize, usize, usize), PatchError> {
    let bad = || PatchError::BadHunkHeader(text.to_string());
    let inner = text.strip_prefix("@@ ").ok_or_else(bad)?;
    let inner = inner.split(" @@").next().ok_or_else(bad)?;
    let (old, new) = inner.split_once(' ').ok_or_else(bad)?;
    let range = |part: &str, sign: char| -> Result<(usize, usize), PatchError> {
        let body = part.strip_prefix(sign).ok_or_else(bad)?;
        match body.split_once(',') {
            Some((start, len)) => Ok((
                start.parse().map_err(|_| bad())?,
                len.parse().map_err(|_| bad())?,
            )),
            None => Ok((body.parse().map_err(|_| bad())?, 1)),
        }
    };
    let (old_start, old_len) = range(old, '-')?;
    let (new_start, new_len) = range(new, '+')?;
    Ok((old_start, old_len, new_start, new_len))
}

/// Parse the unified diff of one file.
pub fn parse(text: &str) -> Result<Patch, PatchError> {
    let all: Vec<&str> = lines(text);
    let mut pos = 0;
    let old_name = all
        .get(pos)
        .and_then(|l| l.strip_prefix("--- "))
        .ok_or(PatchError::MissingHeader)?;
    pos += 1;
    let new_name = all
        .get(pos)
        .and_then(|l| l.strip_prefix("+++ "))
        .ok_or(PatchError::MissingHeader)?;
    pos += 1;
    let mut hunks = Vec::new();
    while pos < all.len() {
        let (old_start, old_len, new_start, new_len) = parse_hunk_header(all[pos])?;
        pos += 1;
        let mut body = Vec::new();
        let (mut seen_old, mut seen_new) = (0, 0);
        while pos < all.len() && !all[pos].starts_with("@@ ") {
            let raw = all[pos];
            pos += 1;
            if raw.starts_with('\\') {
                continue;
            }
            let line = match raw.chars().next() {
                Some(' ') => Line::Context(raw[1..].to_string()),
                Some('+') => Line::Add(raw[1..].to_string()),
                Some('-') => Line::Del(raw[1..].to_string()),
                None => Line::Context(String::new()),
                _ => {
                    return Err(PatchError::BadLine {
                        line: pos,
                        text: raw.to_string(),
                    })
                }
            };
            seen_old += usize::from(line.on_old_side());
            seen_new += usize::from(line.on_new_side());
            body.push(line);
        }
        if seen_old != old_len || seen_new != new_len {
            return Err(PatchError::CountMismatch { hunk: hunks.len() });
        }
        hunks.push(PatchHunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: body,
        });
    }
    Ok(Patch {
        old_name: old_name.to_string(),
        new_name: new_name.to_string(),
        hunks,
    })
}

impl Patch {
    /// Build a patch from the hunks of a diff.
    pub fn from_hunks(old_name: &str, new_name: &str, hunks: &[Hunk<'_>]) -> Patch {
        let convert = |e: &Edit<'_>| match e {
            Edit::Equal(t) => Line::Context(t.to_string()),
            Edit::Insert(t) => Line::Add(t.to_string()),
            Edit::Delete(t) => Line::Del(t.to_string()),
        };
        Patch {
            old_name: old_name.to_string(),
            new_name: new_name.to_string(),
            hunks: hunks
                .iter()
                .map(|h| PatchHunk {
                    old_start: h.old_start,
                    old_len: h.old_len,
                    new_start: h.new_start,
                    new_len: h.new_len,
                    lines: h.edits.iter().map(convert).collect(),
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// (added, removed) line counts over all hunks.
    pub fn counts(&self) -> (usize, usize) {
        let mut added = 0;
        let mut removed = 0;
        for h in &self.hunks {
            for l in &h.lines {
                match l {
                    Line::Add(_) => added += 1,
                    Line::Del(_) => removed += 1,
                    Line::Context(_) => {}
                }
            }
        }
        (added, removed)
    }
}

impl fmt::Display for Patch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "--- {}", self.old_name)?;
        writeln!(f, "+++ {}", self.new_name)?;
        for h in &self.hunks {
            writeln!(
                f,
                "@@ -{},{} +{},{} @@",
                h.old_start, h.old_len, h.new_start, h.new_len
            )?;
            for l in &h.lines {
                writeln!(f, "{}{}", l.marker(), l.text())?;
            }
        }
        Ok(())
    }
}

// -- applying ------------------------------------------------------------------------------

/// Where the old side of a hunk sits in the file, trying `0, -1, +1, -2, +2 ...`
/// lines away from where the header says, up to `fuzz` lines.
fn locate(
    src: &[&str],
    old_side: &[&str],
    base: usize,
    drift: isize,
    fuzz: usize,
    cursor: usize,
) -> Option<usize> {
    let mut tries: Vec<isize> = vec![0];
    for d in 1..=fuzz as isize {
        tries.push(-d);
        tries.push(d);
    }
    for d in tries {
        let at = base as isize + drift + d;
        if at < cursor as isize {
            continue;
        }
        let at = at as usize;
        if at + old_side.len() > src.len() {
            continue;
        }
        if &src[at..at + old_side.len()] == old_side {
            return Some(at);
        }
    }
    None
}

/// Apply a patch to the text it was made from.
pub fn apply(patch: &Patch, original: &str) -> Result<String, PatchError> {
    apply_with_fuzz(patch, original, 0)
}

/// Apply a patch to a file that may have shifted by up to `fuzz` lines.
pub fn apply_with_fuzz(patch: &Patch, original: &str, fuzz: usize) -> Result<String, PatchError> {
    let src = lines(original);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut drift: isize = 0;
    for (n, h) in patch.hunks.iter().enumerate() {
        // A hunk with an empty old side names the line it goes after.
        let base = h.old_start.saturating_sub(1);
        let old_side: Vec<&str> = h
            .lines
            .iter()
            .filter(|l| l.on_old_side())
            .map(|l| l.text())
            .collect();
        let at = locate(&src, &old_side, base, drift, fuzz, cursor)
            .ok_or(PatchError::Mismatch { hunk: n, line: base + 1 })?;
        drift = at as isize - base as isize;
        out.extend(src[cursor..at].iter().map(|s| s.to_string()));
        for l in h.lines.iter().filter(|l| l.on_new_side()) {
            out.push(l.text().to_string());
        }
        cursor = at + old_side.len();
    }
    out.extend(src[cursor..].iter().map(|s| s.to_string()));
    if out.is_empty() {
        Ok(String::new())
    } else {
        Ok(out.join("\n") + "\n")
    }
}

/// True when the patch applies cleanly with no shift allowed.
pub fn applies(patch: &Patch, original: &str) -> bool {
    apply(patch, original).is_ok()
}

/// The patch that undoes this one.
pub fn reverse(patch: &Patch) -> Patch {
    Patch {
        old_name: patch.new_name.clone(),
        new_name: patch.old_name.clone(),
        hunks: patch
            .hunks
            .iter()
            .map(|h| PatchHunk {
                old_start: h.new_start,
                old_len: h.old_len,
                new_start: h.old_start,
                new_len: h.new_len,
                lines: h.lines.iter().map(Line::inverted).collect(),
            })
            .collect(),
    }
}

/// Apply several patches one after another to the same text.
pub fn apply_series(patches: &[Patch], original: &str) -> Result<String, PatchError> {
    let mut text = original.to_string();
    for p in patches {
        text = apply(p, &text)?;
    }
    Ok(text)
}

/// Old line numbers (from 1) that the patch removes or changes.
pub fn touched_lines(patch: &Patch) -> Vec<usize> {
    let mut out = Vec::new();
    for h in &patch.hunks {
        let mut at = if h.old_len == 0 { h.old_start + 1 } else { h.old_start };
        for l in &h.lines {
            match l {
                Line::Context(_) => at += 1,
                Line::Del(_) => {
                    out.push(at);
                    at += 1;
                }
                Line::Add(_) => {}
            }
        }
    }
    out
}

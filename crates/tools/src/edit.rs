//! `edit`: replace text in a file, with helpful errors for near misses.

use std::borrow::Cow;
use std::collections::BinaryHeap;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Effects, Repaired, Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::repair::{Kind, Touched};
use crate::{atomic, resolve};

mod fuzzy;

/// The maximum number of lines shown in a nearest-match error hint.
const HINT_LINES: usize = 20;

/// The most lines the echo at the end of a result prints.
const ECHO_LINES: usize = 40;

/// The most changed regions the echo shows before it counts the rest.
const ECHO_REGIONS: usize = 3;

#[cfg(test)]
thread_local! {
    /// How many not-found hints this thread built. A hint walks the whole
    /// file, so a test can prove that an edit which is recovered, or already
    /// applied, builds none.
    static HINTS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Arguments for one `edit` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    /// The file to edit, relative to the working directory.
    #[serde(borrow)]
    path: Cow<'a, str>,
    #[serde(default, borrow)]
    old_string: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    new_string: Option<Cow<'a, str>>,
    /// Replace every occurrence instead of requiring a single match.
    replace_all: Option<bool>,
    /// A 1-based line near the intended match, which picks one of several.
    line: Option<usize>,
    /// Several replacements; each one matches the file text as it was read.
    #[serde(default, borrow)]
    edits: Option<Vec<Replacement<'a>>>,
}

/// The arguments an effect check reads: the path, and the shape of the edits.
/// The texts have to be strings, but they are skipped, not decoded again.
#[derive(Debug, Deserialize)]
struct EffectArgs<'a> {
    #[serde(borrow)]
    path: Cow<'a, str>,
    #[serde(default, borrow)]
    old_string: Option<&'a RawValue>,
    #[serde(default, borrow)]
    new_string: Option<&'a RawValue>,
    #[serde(default, borrow)]
    edits: Option<Vec<EffectEdit<'a>>>,
}

/// One replacement of an `edits` list, both of its texts checked as strings.
#[derive(Debug, Deserialize)]
struct EffectEdit<'a> {
    #[serde(borrow)]
    old_string: &'a RawValue,
    #[serde(borrow)]
    new_string: &'a RawValue,
}

/// Whether the arguments are a JSON object, the only form `run` reads: serde's
/// derived visitor would also take a bare array, in its struct-from-seq shape.
fn is_object(args: &str) -> bool {
    args.trim_start().starts_with('{')
}

/// Whether `raw` holds a JSON string, read from the value's own text so the
/// string is never unescaped or copied.
fn is_string(raw: &RawValue) -> bool {
    raw.get().starts_with('"')
}

/// One replacement in an `edits` list.
#[derive(Debug, Deserialize)]
pub struct Replacement<'a> {
    #[serde(borrow)]
    old_string: Cow<'a, str>,
    #[serde(borrow)]
    new_string: Cow<'a, str>,
    /// Replace every occurrence of this text instead of requiring one match.
    replace_all: Option<bool>,
    /// A 1-based line near the intended match, which picks one of several.
    line: Option<usize>,
}

/// The error for a call that gives neither form, or both.
const INVALID_FORM: &str = "invalid arguments: give old_string and new_string, or an edits list";

/// The replacements a call asks for, after the shape check.
enum Plan<'a> {
    Single {
        old: &'a str,
        new: &'a str,
        replace_all: bool,
        hint: Option<usize>,
    },
    Many {
        edits: &'a [Replacement<'a>],
    },
}

impl<'a> Plan<'a> {
    /// Checks that the call gives exactly one of the two forms.
    fn of(args: &'a Args<'a>) -> Result<Self, String> {
        let edits = args.edits.as_deref().filter(|edits| !edits.is_empty());
        match (
            args.old_string.as_deref(),
            args.new_string.as_deref(),
            edits,
        ) {
            (Some(old), Some(new), None) => Ok(Self::Single {
                old,
                new,
                replace_all: args.replace_all.unwrap_or(false),
                hint: args.line,
            }),
            (None, None, Some(edits)) => Ok(Self::Many { edits }),
            _ => Err(INVALID_FORM.into()),
        }
    }
}

/// The text one replacement made, the number of matches it replaced, the
/// rest of the success line that follows the path, and the lines it changed.
struct Applied {
    text: String,
    count: usize,
    suffix: String,
    changed: Changed,
}

/// One replacement to make: the span that matched, and the index of the text
/// that replaces it in the edit's replacement list.
struct Hit {
    /// Byte range in the text the span was matched against.
    range: Range<usize>,
    /// Index into the replacement texts of the resolution that made this hit.
    new: usize,
}

/// How one edit of a list fared against a text.
enum Resolution<'a> {
    /// Spans to replace, their replacement texts, plus the reply note when a
    /// fallback tier matched.
    ///
    /// The texts sit once per edit rather than once per hit, so a hit costs no
    /// allocation even when the replacement had to be built (a CRLF file, or a
    /// stripped read prefix).
    Found {
        hits: Vec<Hit>,
        news: Vec<Cow<'a, str>>,
        note: Option<String>,
    },
    /// Not found in this text: the second try or the already-applied check may
    /// still save it. The needle is kept so that the hint, which walks the
    /// whole file, is built at the end and only for an edit that is still
    /// missing.
    Missing { needle: Cow<'a, str> },
    /// A failure, with its reason for the reply.
    Failed(String),
}

/// What an `edits` list did: the new file text, the reply and its error flag.
struct ListOutcome {
    /// The new text, or `None` when no edit applied.
    text: Option<String>,
    content: String,
    is_error: bool,
}

/// Applies an exact-text replacement to a file, atomically.
#[derive(Debug)]
pub struct Edit {
    spec: ToolSpec,
    working_dir: PathBuf,
    touched: Touched,
}

impl Edit {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
            touched: Touched::default(),
        }
    }

    /// Shares `touched`, the files the run's file tools read or changed,
    /// which a repaired call may use.
    pub fn with_touched(mut self, touched: Touched) -> Self {
        self.touched = touched;
        self
    }

    async fn run_inner(&self, args: &Args<'_>) -> Result<ToolOutput, String> {
        let plan = Plan::of(args)?;
        let path = resolve(&self.working_dir, &args.path)?;
        let original = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;

        let (text, content, is_error) = match plan {
            Plan::Single {
                old,
                new,
                replace_all,
                hint,
            } => {
                let applied = apply(&original, &path, old, new, replace_all, hint)?;
                let content = success_line(&path, &applied);
                (Some(applied.text), content, false)
            }
            Plan::Many { edits } => {
                let done = apply_edits(&path, &original, edits);
                (done.text, done.content, done.is_error)
            }
        };

        if let Some(text) = text {
            atomic::write(&path, text.as_bytes()).await?;
        }
        Ok(if is_error {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        })
    }
}

/// Applies an `edits` list: every edit matches the text as it was read, and the
/// edits that matched are applied together in one pass.
fn apply_edits(path: &Path, original: &str, edits: &[Replacement<'_>]) -> ListOutcome {
    let mut resolutions: Vec<Resolution<'_>> = edits
        .iter()
        .map(|edit| match_edit(original, path, edit))
        .collect();
    fail_overlaps(&mut resolutions);

    let mut entries: Vec<Option<String>> = vec![None; edits.len()];
    let mut missing: Vec<(usize, Cow<'_, str>)> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    let mut news: Vec<Cow<'_, str>> = Vec::new();
    let mut applied = 0usize;
    let mut first_start: Option<usize> = None;

    for (index, resolution) in resolutions.into_iter().enumerate() {
        match resolution {
            Resolution::Found {
                hits: found,
                news: replacements,
                note,
            } => {
                applied += 1;
                entries[index] = note.map(|note| edit_line(index, &note));
                if let Some(start) = leftmost(&found) {
                    first_start = Some(first_start.map_or(start, |seen| seen.min(start)));
                }
                let news_base = news.len();
                news.extend(replacements);
                hits.extend(found.into_iter().map(|hit| Hit {
                    range: hit.range,
                    new: news_base + hit.new,
                }));
            }
            Resolution::Failed(reason) => entries[index] = Some(edit_line(index, &reason)),
            Resolution::Missing { needle } => missing.push((index, needle)),
        }
    }
    hits.sort_by_key(|hit| hit.range.start);
    let mut text: Option<String> = None;
    let mut changed = Changed::default();
    if !hits.is_empty() {
        let (built, regions) = build(original, &hits, &news);
        text = Some(built);
        changed = regions;
    }

    // Second try, in order: an edit may depend on another edit's new text.
    // With no change from the first pass the text is still the original, so a
    // second try would miss exactly as the first did; it is skipped.
    if text.is_some() {
        let mut retried: Vec<Hit> = Vec::new();
        let mut still: Vec<(usize, Cow<'_, str>)> = Vec::new();
        for (index, needle) in std::mem::take(&mut missing) {
            // A change of an earlier retry is in the text a later one reads.
            if !retried.is_empty() {
                rebuild(&mut text, &mut changed, &retried, &news);
                retried.clear();
            }
            let base = text.as_deref().unwrap_or(original);
            match match_edit(base, path, &edits[index]) {
                Resolution::Found {
                    hits: found,
                    news: replacements,
                    note,
                } => {
                    applied += 1;
                    entries[index] = note.map(|note| edit_line(index, &note));
                    // A change of the second try counts only when the first
                    // pass changed nothing, since its offsets are in the new
                    // text.
                    first_start = first_start.or_else(|| leftmost(&found));
                    let news_base = news.len();
                    news.extend(replacements);
                    retried.extend(found.into_iter().map(|hit| Hit {
                        range: hit.range,
                        new: news_base + hit.new,
                    }));
                }
                Resolution::Failed(reason) => entries[index] = Some(edit_line(index, &reason)),
                // Still missing: it keeps the first try's needle, whose hint
                // quotes the file as it was read.
                Resolution::Missing { .. } => still.push((index, needle)),
            }
        }
        if !retried.is_empty() {
            rebuild(&mut text, &mut changed, &retried, &news);
        }
        missing = still;
    }

    let mut already = 0usize;
    if !missing.is_empty() {
        // One scan of the file decides the CRLF question for every hint.
        let crlf = original.contains("\r\n");
        for (index, needle) in missing {
            if already_applied(crlf, original, &edits[index]) {
                already += 1;
            } else {
                entries[index] = Some(edit_line(index, &not_found_hint(path, original, &needle)));
            }
        }
    }

    let mut content = format!(
        "edited {}: {applied} of {} edits applied",
        path.display(),
        edits.len()
    );
    if applied > 0
        && let Some(start) = first_start
    {
        content.push_str(&format!(
            ", first change at line {}",
            line_at(original, start)
        ));
    }
    if already > 0 {
        content.push_str(&format!(", {already} already applied"));
    }
    for entry in entries.into_iter().flatten() {
        content.push('\n');
        content.push_str(&entry);
    }
    // Every successful edit shows what the file holds now, the list form
    // included, so the model never reads the file back to see its change.
    if let Some(text) = text.as_deref()
        && !changed.regions.is_empty()
    {
        content.push('\n');
        changed.write(&mut content, text);
    }

    ListOutcome {
        text,
        is_error: applied == 0 && already == 0,
        content,
    }
}

/// Applies one more build on top of `text`, and moves the regions the echo
/// already holds into the text that build makes.
fn rebuild(text: &mut Option<String>, changed: &mut Changed, hits: &[Hit], news: &[Cow<'_, str>]) {
    let base = text.take().unwrap_or_default();
    let (built, regions) = build(&base, hits, news);
    // The regions of the earlier builds are offsets in `base`.
    let mut earlier = std::mem::take(&mut changed.regions);
    for region in &mut earlier {
        let start = shifted(region.start, hits, news);
        region.end = shifted(region.end, hits, news).max(start);
        region.start = start;
    }
    *text = Some(built);
    changed.absorb(text.as_deref().unwrap_or_default(), earlier);
    changed.more += regions.more;
}

/// Maps an offset of the text a build reads to the same text after the build.
fn shifted(offset: usize, hits: &[Hit], news: &[Cow<'_, str>]) -> usize {
    let mut delta = 0isize;
    for hit in hits {
        if hit.range.end <= offset {
            delta += news[hit.new].len() as isize - hit.range.len() as isize;
        } else if hit.range.start < offset {
            // Inside the span the hit replaces: the offset moves to its start.
            return (hit.range.start as isize + delta).max(0) as usize;
        } else {
            break;
        }
    }
    (offset as isize + delta).max(0) as usize
}

/// Turns `\\`, `\'` and `\"` into `\`, `'` and `"`; any other backslash stays.
fn unescape_once(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
            && matches!(next, '\\' | '\'' | '"')
        {
            out.push(next);
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// `old` and `new` with one level of escapes removed, and where the text holds
/// that `old`.
fn unescaped_match(text: &str, old: &str, new: &str) -> Option<(String, String, usize)> {
    if !old.contains('\\') {
        return None;
    }
    let old_text = unescape_once(old);
    if old_text == old {
        return None;
    }
    let new_text = unescape_once(new);
    let (o, n) = line_endings_of(text, &old_text, &new_text);
    let found = first_match(text, &o)?;
    Some((o.into_owned(), n.into_owned(), found))
}

/// Matches one edit against `text` with the tiers in order: exact, the read
/// prefix strip, one level of backslash escapes removed, then fuzzy.
fn match_edit<'a>(text: &str, path: &Path, edit: &'a Replacement<'a>) -> Resolution<'a> {
    let old = edit.old_string.as_ref();
    if old.is_empty() {
        return Resolution::Failed("invalid arguments: old_string must not be empty".into());
    }
    if old == edit.new_string.as_ref() {
        return Resolution::Failed("changes nothing".into());
    }
    let replace_all = edit.replace_all.unwrap_or(false);
    let (mut old, mut new) = line_endings_of(text, old, &edit.new_string);
    let mut first = first_match(text, &old);
    let mut stripped = false;

    if first.is_none()
        && let Some(old_text) = strip_read_prefixes(&old)
    {
        let new_text = strip_some_read_prefixes(&new);
        let (o, n) = line_endings_of(text, &old_text, &new_text);
        old = Cow::Owned(o.into_owned());
        new = Cow::Owned(n.into_owned());
        first = first_match(text, &old);
        stripped = true;
    }

    let mut unescaped = false;
    if first.is_none()
        && let Some((o, n, found)) = unescaped_match(text, &old, &new)
    {
        old = Cow::Owned(o);
        new = Cow::Owned(n);
        first = Some(found);
        unescaped = true;
        if old == new {
            return Resolution::Failed("changes nothing".into());
        }
    }

    if let Some(first) = first {
        let note = match (stripped, unescaped) {
            (true, true) => Some(
                "matched after removing read line numbers and one level of backslash escapes"
                    .to_string(),
            ),
            (false, true) => {
                Some("matched after removing one level of backslash escapes".to_string())
            }
            (true, false) => Some("matched after removing read line numbers".to_string()),
            (false, false) => None,
        };
        // A line hint picks one of several matches instead of failing on them.
        if !replace_all
            && let Some(hint) = edit.line
            && text[first + old.len()..].contains(old.as_ref())
        {
            let start = nearest_match(text, &old, hint);
            let hits = vec![Hit {
                range: start..start + old.len(),
                new: 0,
            }];
            return Resolution::Found {
                hits,
                news: vec![new],
                note,
            };
        }
        // One pass collects the spans: a second match is looked for only
        // without replace_all, where it is an error.
        let mut hits = Vec::new();
        let mut search = first;
        while let Some(offset) = text[search..].find(old.as_ref()) {
            let start = search + offset;
            hits.push(Hit {
                range: start..start + old.len(),
                new: 0,
            });
            search = start + old.len();
            if !replace_all {
                if text[search..].contains(old.as_ref()) {
                    return Resolution::Failed(multiple_matches(path, text, &old));
                }
                break;
            }
        }
        return Resolution::Found {
            hits,
            news: vec![new],
            note,
        };
    }

    let Some((tier, spans)) = fuzzy::find(text, &old) else {
        return Resolution::Missing { needle: old };
    };
    let chosen: &[fuzzy::Span] = if replace_all {
        &spans[..]
    } else if spans.len() == 1 {
        &spans[..1]
    } else if let Some(line) = edit.line {
        let index = nearest_span(&spans, line);
        &spans[index..index + 1]
    } else {
        let lines: Vec<usize> = spans.iter().map(|span| span.line).collect();
        return Resolution::Failed(multiple_matches_at(path, &lines));
    };
    let mut hits = Vec::with_capacity(chosen.len());
    // The plain replacement is written once, however many spans take it.
    let mut news: Vec<Cow<'a, str>> = Vec::new();
    let mut plain: Option<usize> = None;
    let mut indented = false;
    for span in chosen {
        let index = match fuzzy::indent_gap(text, span, &old) {
            Some(gap) => {
                let mut out = String::with_capacity(new.len() + gap.len() * new.lines().count());
                fuzzy::push_indented(&mut out, &new, gap);
                indented = true;
                news.push(Cow::Owned(out));
                news.len() - 1
            }
            None => match plain {
                Some(index) => index,
                None => {
                    news.push(new.clone());
                    let index = news.len() - 1;
                    plain = Some(index);
                    index
                }
            },
        };
        hits.push(Hit {
            range: span.range.clone(),
            new: index,
        });
    }
    let (note, joiner) = if stripped {
        ("removing read line numbers", " and ")
    } else {
        ("", "")
    };
    let indent = if indented {
        ", new_string indented to match"
    } else {
        ""
    };
    Resolution::Found {
        hits,
        news,
        note: Some(format!(
            "matched after {note}{joiner}{}{indent}; the file text differs from old_string, so read the file before the next edit",
            tier.describe()
        )),
    }
}

/// The reply line for one edit of a list.
fn edit_line(index: usize, text: &str) -> String {
    format!("edit {}: {text}", index + 1)
}

/// The start offset of the leftmost of `hits`.
fn leftmost(hits: &[Hit]) -> Option<usize> {
    hits.iter().map(|hit| hit.range.start).min()
}

/// Fails the higher-numbered edit of two first-pass spans that overlap.
///
/// `spans` is sorted by start, so one left-to-right sweep finds every conflict:
/// a span whose start is before the furthest end seen so far overlaps that
/// span, and no other earlier span can overlap it without overlapping that
/// one, which the sweep already settled.
fn fail_overlaps(resolutions: &mut [Resolution<'_>]) {
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    let mut found = 0usize;
    for (index, resolution) in resolutions.iter().enumerate() {
        if let Resolution::Found { hits, .. } = resolution {
            found += 1;
            spans.extend(
                hits.iter()
                    .map(|hit| (hit.range.start, hit.range.end, index)),
            );
        }
    }
    // One edit's own spans never overlap, so a lone match cannot conflict.
    if found < 2 {
        return;
    }
    spans.sort_by_key(|&(start, _, _)| start);

    // For each failed edit, the lower-numbered edit it conflicts with.
    let mut failed: Vec<Option<usize>> = vec![None; resolutions.len()];
    // The end of the live span that reaches furthest right so far, and its
    // edit. A span of a failed edit is skipped and popped, so a dead span
    // stops counting.
    let mut ends: BinaryHeap<(usize, usize)> = BinaryHeap::with_capacity(spans.len());
    for (start, end, edit) in spans {
        if failed[edit].is_some() {
            continue;
        }
        while ends
            .peek()
            .is_some_and(|&(_, owner)| failed[owner].is_some())
        {
            ends.pop();
        }
        if let Some(&(reach, owner)) = ends.peek()
            && start < reach
            && owner != edit
        {
            let (later, earlier) = (edit.max(owner), edit.min(owner));
            failed[later] = Some(earlier);
            if later == edit {
                continue;
            }
        }
        ends.push((end, edit));
    }

    for (index, other) in failed.into_iter().enumerate() {
        if let Some(other) = other {
            resolutions[index] = Resolution::Failed(format!(
                "overlaps edit {}; merge them into one edit or pick separate text",
                other + 1
            ));
        }
    }
}

/// Whether an edit that matched nowhere is already in the file.
fn already_applied(crlf: bool, original: &str, edit: &Replacement<'_>) -> bool {
    let new = edit.new_string.as_ref();
    if new.is_empty() {
        return false;
    }
    let (converted, _) = line_endings_in(crlf, new, new);
    original.contains(converted.as_ref())
}

/// Builds the result of applying `hits` to `text` as one String, and the
/// regions it changed for the echo.
///
/// The hits are sorted by start and do not overlap; `hit.new` indexes `news`.
fn build(text: &str, hits: &[Hit], news: &[Cow<'_, str>]) -> (String, Changed) {
    let mut len = text.len();
    for hit in hits {
        len = len - hit.range.len() + news[hit.new].len();
    }
    let mut out = String::with_capacity(len);
    let mut changed = Changed::default();
    let mut copied = 0;
    for hit in hits {
        out.push_str(&text[copied..hit.range.start]);
        let at = out.len();
        out.push_str(&news[hit.new]);
        changed.push(&out, at, out.len());
        copied = hit.range.end;
    }
    out.push_str(&text[copied..]);
    (out, changed)
}

/// The 1-based line number of `offset` in `text`.
fn line_at(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// The number of line ends in `text`.
fn newlines(text: &str) -> usize {
    text.matches('\n').count()
}

/// The success line for one replacement, with the lines it changed.
fn success_line(path: &Path, applied: &Applied) -> String {
    let mut content = format!(
        "replaced {} occurrence(s) in {}{}",
        applied.count,
        path.display(),
        applied.suffix
    );
    applied.changed.write(&mut content, &applied.text);
    content
}

/// Applies one replacement to `text`, or returns that replacement's error.
///
/// `hint`, a 1-based line, picks one of several matches when the call replaces
/// a single place.
fn apply(
    text: &str,
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
    hint: Option<usize>,
) -> Result<Applied, String> {
    if old.is_empty() {
        return Err("invalid arguments: old_string must not be empty".into());
    }
    let (mut old, mut new) = line_endings_of(text, old, new);
    let mut first = first_match(text, &old);
    let mut stripped = false;

    if first.is_none()
        && let Some(old_text) = strip_read_prefixes(&old)
    {
        let new_text = strip_some_read_prefixes(&new);
        let (o, n) = line_endings_of(text, &old_text, &new_text);
        old = Cow::Owned(o.into_owned());
        new = Cow::Owned(n.into_owned());
        first = first_match(text, &old);
        stripped = true;
    }

    let mut unescaped = false;
    if first.is_none()
        && let Some((o, n, found)) = unescaped_match(text, &old, &new)
    {
        old = Cow::Owned(o);
        new = Cow::Owned(n);
        first = Some(found);
        unescaped = true;
    }

    let note = if stripped {
        "removing read line numbers"
    } else {
        ""
    };

    let Some(first) = first else {
        let Some((tier, spans)) = fuzzy::find(text, &old) else {
            return Err(not_found_hint(path, text, &old));
        };
        let chosen: &[fuzzy::Span] = if replace_all {
            &spans[..]
        } else if spans.len() == 1 {
            &spans[..1]
        } else if let Some(line) = hint {
            let index = nearest_span(&spans, line);
            &spans[index..index + 1]
        } else {
            let lines: Vec<usize> = spans.iter().map(|span| span.line).collect();
            return Err(multiple_matches_at(path, &lines));
        };
        let mut result = String::with_capacity(text.len());
        let mut copied = 0;
        let mut changed = Changed::default();
        let mut indented = false;
        for span in chosen {
            result.push_str(&text[copied..span.range.start]);
            let at = result.len();
            match fuzzy::indent_gap(text, span, &old) {
                Some(gap) => {
                    fuzzy::push_indented(&mut result, &new, gap);
                    indented = true;
                }
                None => result.push_str(&new),
            }
            changed.push(&result, at, result.len());
            copied = span.range.end;
        }
        result.push_str(&text[copied..]);

        let joiner = if stripped { " and " } else { "" };
        let indent = if indented {
            ", new_string indented to match"
        } else {
            ""
        };
        return Ok(Applied {
            count: chosen.len(),
            text: result,
            suffix: format!(
                " (matched after {note}{joiner}{}{indent}); the file text differs from old_string, so read the file before the next edit",
                tier.describe()
            ),
            changed,
        });
    };

    let suffix = if unescaped {
        let and = if stripped { " and" } else { "" };
        format!(" (matched after {note}{and} removing one level of backslash escapes)")
    } else if stripped {
        format!(" (matched after {note})")
    } else {
        String::new()
    };

    // A line hint picks one of several matches instead of failing on them.
    if !replace_all
        && let Some(hint) = hint
        && text[first + old.len()..].contains(old.as_ref())
    {
        let start = nearest_match(text, &old, hint);
        let mut result = String::with_capacity(text.len());
        result.push_str(&text[..start]);
        result.push_str(&new);
        result.push_str(&text[start + old.len()..]);
        let mut changed = Changed::default();
        changed.push(&result, start, start + new.len());
        return Ok(Applied {
            text: result,
            count: 1,
            suffix,
            changed,
        });
    }

    // One pass copies the file and writes the replacement: a second match
    // without replace_all stops it, and its message lists every line.
    let mut result = String::with_capacity(text.len());
    // The text before the first match, which is where the first run starts.
    result.push_str(&text[..first]);
    let mut copied = first;
    let mut count = 0usize;
    let mut search = first;
    let mut changed = Changed::default();
    // A gap of original text breaks a run of written text: the breaks are
    // recorded (the run's end and the next run's start, both in the new text),
    // so a `replace_all` over matches in a row finds none and pays a compare.
    let mut breaks: Vec<(usize, usize)> = Vec::new();
    while let Some(offset) = text[search..].find(old.as_ref()) {
        let start = search + offset;
        if !replace_all && count > 0 {
            return Err(multiple_matches(path, text, &old));
        }
        if start != copied {
            let at = result.len();
            breaks.push((at, at + (start - copied)));
        }
        result.push_str(&text[copied..start]);
        result.push_str(&new);
        copied = start + old.len();
        search = copied;
        count += 1;
    }
    if count > 0 {
        // The runs of the loop, from the text before the first match onwards.
        let mut from = first;
        for (end, next) in breaks {
            changed.push(&result, from, end);
            from = next;
        }
        changed.push(&result, from, result.len());
    }
    result.push_str(&text[copied..]);

    Ok(Applied {
        text: result,
        count,
        suffix,
        changed,
    })
}

/// Removes a read prefix (`N<TAB>`) from every line, or returns `None` when
/// any line lacks one.
fn strip_read_prefixes(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        out.push_str(line[digits..].strip_prefix('\t')?);
    }
    Some(out)
}

/// Removes a read prefix (`N<TAB>`) from each line that has one.
fn strip_some_read_prefixes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        match line[digits..].strip_prefix('\t') {
            Some(rest) if digits > 0 => out.push_str(rest),
            _ => out.push_str(line),
        }
    }
    out
}

/// Converts `old` and `new` to CRLF when the file uses CRLF and `old` does
/// not, since the model writes `\n` even for a CRLF file.
fn line_endings_of<'a>(original: &str, old: &'a str, new: &'a str) -> (Cow<'a, str>, Cow<'a, str>) {
    // The cheap checks on the model's text come first: only an edit that spans
    // lines can need the file's line endings, and finding them scans the file.
    if !old.contains('\n') || old.contains('\r') {
        return (Cow::Borrowed(old), Cow::Borrowed(new));
    }
    line_endings_in(original.contains("\r\n"), old, new)
}

/// [`line_endings_of`] with the file's CRLF answer already at hand.
fn line_endings_in<'a>(crlf: bool, old: &'a str, new: &'a str) -> (Cow<'a, str>, Cow<'a, str>) {
    if !crlf || !old.contains('\n') || old.contains('\r') {
        return (Cow::Borrowed(old), Cow::Borrowed(new));
    }
    let converted = Cow::Owned(old.replace('\n', "\r\n"));
    let new = if new.contains('\n') {
        Cow::Owned(new.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(new)
    };
    (converted, new)
}

/// The offset of the first match of `needle` in `haystack`, or `None` when the
/// needle is empty or absent.
fn first_match(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        None
    } else {
        haystack.find(needle)
    }
}

/// Returns the 1-based line numbers that contain `needle`.
fn match_lines(haystack: &str, needle: &str) -> Vec<usize> {
    let line_starts: Vec<usize> = haystack.match_indices('\n').map(|(i, _)| i + 1).collect();
    let mut lines = Vec::new();
    let mut search = haystack;
    let mut base = 0usize;
    while let Some(idx) = search.find(needle) {
        let abs = base + idx;
        let line = line_starts.partition_point(|&s| s <= abs) + 1;
        lines.push(line);
        let skip = abs + needle.len().max(1);
        base = skip;
        search = &haystack[skip..];
    }
    lines
}

/// Builds the error for zero matches: a window near the closest text.
fn not_found_hint(path: &Path, original: &str, needle: &str) -> String {
    #[cfg(test)]
    HINTS_BUILT.with(|built| built.set(built.get() + 1));
    let lines: Vec<&str> = original.lines().collect();
    let nearest = nearest_line(&lines, needle);
    let start = nearest.saturating_sub(HINT_LINES / 2);
    let mut window = String::new();
    let end = lines.len().min(start + HINT_LINES);
    for (index, line) in lines[start..end].iter().enumerate() {
        window.push_str(&format!("{}\t{line}\n", start + index + 1));
    }
    let line_no = nearest + 1;
    format!(
        "text not found in {}; closest match is near line {line_no}:\n{window}",
        path.display()
    )
}

/// Finds the index of the line closest to `needle` (fewest differing words).
fn nearest_line(lines: &[&str], needle: &str) -> usize {
    let needle_words: Vec<&str> = needle.split_whitespace().collect();
    let needle_len = needle_words.len();
    let mut best = 0;
    let mut best_score = usize::MAX;
    for (i, line) in lines.iter().enumerate() {
        let line_words: Vec<&str> = line.split_whitespace().collect();
        // Shared words between the needle and this line, penalty for length gap.
        let shared = needle_words
            .iter()
            .filter(|w| line_words.contains(w))
            .count();
        let score = needle_len.saturating_sub(shared) * 2
            + (line_words.len() as isize - needle_len as isize).unsigned_abs();
        if score < best_score {
            best_score = score;
            best = i;
        }
    }
    best
}

/// Builds the error for more than one match without `replace_all`.
fn multiple_matches(path: &Path, original: &str, needle: &str) -> String {
    multiple_matches_at(path, &match_lines(original, needle))
}

/// Builds the multiple-match error from the 1-based line numbers of the matches.
fn multiple_matches_at(path: &Path, lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(3).map(|n| n.to_string()).collect();
    format!(
        "text matches {} time(s) in {} at line(s) {}; add context to old_string or pass replace_all=true, or pass line to pick one",
        lines.len(),
        path.display(),
        shown.join(", ")
    )
}

/// The lines of context the echo prints around a changed region.
const ECHO_CONTEXT: usize = 1;

/// The furthest apart two changed regions can be, in lines, and still show
/// their windows as one run, so that a `...` line only appears where lines are
/// skipped: two windows of [`ECHO_CONTEXT`] lines meet at `last + 2 * context
/// + 1`.
const ECHO_MERGE: usize = ECHO_CONTEXT * 2 + 1;

/// The changed regions of the new text, for the echo at the end of a result.
///
/// A region is the byte range an edit wrote in the new text. Regions whose
/// windows would touch merge, so no changed line prints twice, and only the
/// first [`ECHO_REGIONS`] are kept: the rest are counted, never shown.
#[derive(Debug, Default)]
struct Changed {
    regions: Vec<Range<usize>>,
    /// The regions past the first [`ECHO_REGIONS`], which the echo counts.
    more: usize,
}

impl Changed {
    /// Records the text `start..end` of `text`, the new text, that an edit
    /// wrote. `start` must not come before the last region's end.
    ///
    /// The text a call writes usually comes right after the text of the last
    /// region, so that case returns before any line is counted: a
    /// `replace_all` over a file full of matches pays a compare per match.
    #[inline]
    fn push(&mut self, text: &str, start: usize, end: usize) {
        if let Some(seen) = self.regions.last_mut()
            && start <= seen.end
        {
            seen.end = seen.end.max(end);
            return;
        }
        self.push_apart(text, start, end);
    }

    /// Records text that starts after the last region ends, merging it when
    /// their windows would touch. The merge check counts at most
    /// [`ECHO_MERGE`] + 1 line ends, so a region costs no scan of the text
    /// that separates two far apart regions.
    #[inline(never)]
    fn push_apart(&mut self, text: &str, start: usize, end: usize) {
        let end = end.max(start);
        if let Some(seen) = self.regions.last_mut()
            && joins(text, seen, start)
        {
            seen.end = end.max(seen.end);
            return;
        }
        if self.regions.len() == ECHO_REGIONS {
            self.more += 1;
            return;
        }
        self.regions.push(start..end);
    }

    /// Adds the regions of an earlier build, already mapped into `text`, and
    /// the regions this one records. The two lists come in ascending order.
    fn absorb(&mut self, text: &str, mut earlier: Vec<Range<usize>>) {
        if earlier.is_empty() {
            return;
        }
        let mut more = std::mem::take(&mut self.more);
        earlier.append(&mut self.regions);
        earlier.sort_by_key(|region| (region.start, region.end));
        for region in earlier {
            self.push(text, region.start, region.end);
        }
        more += self.more;
        self.more = more;
    }

    /// Appends `; now:` and the changed lines of `text`, each region with one
    /// line of context, the echo cut at [`ECHO_LINES`] lines and `more`
    /// regions past the shown ones counted at the end.
    ///
    /// The line numbers of the shown regions come from one forward pass over
    /// the new text, which stops at the last shown region.
    fn write(&self, out: &mut String, text: &str) {
        if self.regions.is_empty() {
            return;
        }
        out.push_str("; now:\n");
        // One reservation, so the echo does not grow its buffer line by line;
        // a line longer than the guess grows it once more.
        out.reserve(ECHO_LINES * 48 + 32);
        let bytes = text.as_bytes();
        let mut scan = LineScan::new(bytes);
        let mut left = ECHO_LINES;
        let mut shown = false;
        let mut cut = false;
        for region in &self.regions {
            // Only the first line of the region is counted: where it ends is
            // the region's own offset, so a long region past the echo cap is
            // never walked.
            let first = scan.line_of(region.start);
            if shown {
                out.push_str("...\n");
            }
            shown = true;
            // Print the window from the start of its first context line.
            let back = ECHO_CONTEXT.min(first - 1);
            let mut at = line_start_before(bytes, region.start, back);
            let mut number = first - back;
            let mut after = ECHO_CONTEXT;
            while at < bytes.len() {
                if left == 0 {
                    cut = true;
                    break;
                }
                left -= 1;
                let stop =
                    memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |found| at + found + 1);
                let line = &text[at..stop];
                let line = line.strip_suffix('\n').unwrap_or(line);
                let line = line.strip_suffix('\r').unwrap_or(line);
                let _ = writeln!(out, "{number}\t{line}");
                at = stop;
                number += 1;
                // Past the region, the window holds its context lines only.
                if at >= region.end {
                    match after == 0 {
                        true => break,
                        false => after -= 1,
                    }
                }
            }
            if cut {
                break;
            }
        }
        if cut {
            out.push_str("...\n");
        }
        if self.more > 0 {
            let more = self.more;
            let _ = writeln!(out, "... ({more} more)");
        }
    }
}

/// Whether a change that starts at `start` is close enough to `seen` that
/// their windows of [`ECHO_CONTEXT`] lines would touch.
fn joins(text: &str, seen: &Range<usize>, start: usize) -> bool {
    if start <= seen.end {
        return true;
    }
    let bytes = text.as_bytes();
    // The first line of the new region sits `line ends in the gap` lines below
    // the last line of `seen`, one line higher still when the seen text ends
    // with a line end of its own.
    let mut lines = usize::from(seen.end > seen.start && bytes[seen.end - 1] == b'\n');
    let mut gap = &bytes[seen.end..start];
    while lines <= ECHO_MERGE {
        match memchr::memchr(b'\n', gap) {
            Some(at) => {
                lines += 1;
                gap = &gap[at + 1..];
            }
            None => return true,
        }
    }
    false
}

/// Walks a text forward, giving the 1-based line number of each offset asked
/// for, in ascending order, with one pass over the bytes.
struct LineScan<'a> {
    bytes: &'a [u8],
    at: usize,
    line: usize,
}

impl<'a> LineScan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            line: 1,
        }
    }

    /// The 1-based line that holds `offset`, which must not come before the
    /// last offset asked for.
    fn line_of(&mut self, offset: usize) -> usize {
        while self.at < offset {
            match memchr::memchr(b'\n', &self.bytes[self.at..offset]) {
                Some(found) => {
                    self.at += found + 1;
                    self.line += 1;
                }
                None => self.at = offset,
            }
        }
        self.line
    }
}

/// The byte offset of the start of the line `lines` lines before the one that
/// holds `offset`, or 0 when fewer lines come before it.
fn line_start_before(bytes: &[u8], offset: usize, lines: usize) -> usize {
    let previous = |at: usize| memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |found| found + 1);
    let mut at = previous(offset);
    for _ in 0..lines {
        if at == 0 {
            break;
        }
        at = previous(at - 1);
    }
    at
}

/// The start of the match of `needle` whose first line is nearest `line`, ties
/// going to the earlier match.
fn nearest_match(text: &str, needle: &str, line: usize) -> usize {
    let mut best = 0usize;
    let mut best_gap = usize::MAX;
    let mut number = 1usize;
    let mut at = 0usize;
    while let Some(offset) = text[at..].find(needle) {
        let start = at + offset;
        number += newlines(&text[at..start]);
        let gap = number.abs_diff(line);
        if gap < best_gap {
            best_gap = gap;
            best = start;
        }
        at = start + needle.len();
        // The scan resumes after the match, so its own newlines count here.
        number += newlines(needle);
    }
    best
}

/// The index of the span whose first line is nearest `line`, ties going to the
/// earlier span.
fn nearest_span(spans: &[fuzzy::Span], line: usize) -> usize {
    let mut best = 0usize;
    let mut best_gap = usize::MAX;
    for (index, span) in spans.iter().enumerate() {
        let gap = span.line.abs_diff(line);
        if gap < best_gap {
            best_gap = gap;
            best = index;
        }
    }
    best
}

impl Tool for Edit {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn main_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn repair<'a>(
        &'a self,
        args: &'a str,
    ) -> Option<futures_util::future::BoxFuture<'a, Repaired>> {
        crate::repair::repair(Kind::Edit, &self.working_dir, &self.touched, args)
    }

    fn effects(&self, args: &str) -> Effects {
        // A JSON object is the only form `run` accepts: serde's derived visitor
        // would also read a bare array, in its struct-from-seq shape.
        if !is_object(args) {
            return Effects::default();
        }
        let Ok(arguments) = parse_args::<EffectArgs<'_>>(args) else {
            return Effects::default();
        };
        // Every text has to be a string for `run` to take the call, but each
        // one is skipped, never built.
        let single = [arguments.old_string, arguments.new_string]
            .into_iter()
            .flatten();
        let many = arguments
            .edits
            .iter()
            .flatten()
            .flat_map(|edit| [edit.old_string, edit.new_string]);
        if !single.chain(many).all(is_string) {
            return Effects::default();
        }
        let Ok(path) = resolve(&self.working_dir, &arguments.path) else {
            return Effects::default();
        };
        Effects {
            writes: vec![path],
            ..Effects::default()
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            match self.run_inner(&arguments).await {
                Ok(output) => {
                    if !output.is_error {
                        let path = std::path::Path::new(arguments.path.as_ref());
                        self.touched.note(&self.working_dir, path);
                    }
                    output
                }
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

/// The name of this tool.
pub const NAME: &str = "edit";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Replace exact text in a file. `old_string` must match one place exactly, ",
            "whitespace included, unless `replace_all` is true or `line` picks the nearest match. ",
            "Use `edits` for several places. ",
            "Do not include the line-number prefix that read prints."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file." },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence." },
                "line": { "type": "integer" },
                "edits": {
                    "type": "array",
                    "description": "Each matched against the file as read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean" },
                            "line": { "type": "integer" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(tmp: &tempfile::TempDir) -> Edit {
        Edit::new(tmp.path())
    }

    async fn write(tmp: &tempfile::TempDir, name: &str, contents: &str) {
        tokio::fs::write(tmp.path().join(name), contents)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "aaa bbb ccc\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"bbb","new_string":"BBB"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "aaa BBB ccc\n");
    }

    #[tokio::test]
    async fn multiple_matches_without_replace_all_is_an_error_with_counts() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x foo\ny foo\nz foo\na\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"foo","new_string":"bar"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("3 time"), "{text}");
        assert!(text.contains("1, 2, 3"), "{text}");
        // File is unchanged.
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "x foo\ny foo\nz foo\na\n");
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x foo\ny foo\nz foo\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"foo","new_string":"bar","replace_all":true}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "x bar\ny bar\nz bar\n");
    }

    #[tokio::test]
    async fn not_found_errors_with_nearest_lines_and_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp,
            "f.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven potato\n",
        )
        .await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"potato nowhere","new_string":"nope"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("not found"), "{text}");
        assert!(text.contains("7\tseven potato"), "{text}");
    }

    #[tokio::test]
    async fn bad_args_are_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp).run("not json", &CancellationToken::new()).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("invalid arguments"));
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"nope.txt","old_string":"a","new_string":"b"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn an_empty_old_string_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a")
            .await
            .unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"","new_string":"b"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("old_string must not be empty"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn edits_a_crlf_file_with_lf_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        tokio::fs::write(&path, "a\r\nb\r\nc\r\n").await.unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"a\nb","new_string":"x\ny"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "x\r\ny\r\nc\r\n"
        );
    }

    async fn run_edit(tmp: &tempfile::TempDir, args: serde_json::Value) -> ToolOutput {
        tool(tmp)
            .run(&args.to_string(), &CancellationToken::new())
            .await
    }

    async fn read(tmp: &tempfile::TempDir, name: &str) -> String {
        tokio::fs::read_to_string(tmp.path().join(name))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn prefix_strip_matches_after_removing_read_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "alpha\nbeta\ngamma\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"2\tbeta\n3\tgamma","new_string":"2\tBETA\n3\tGAMMA"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("read line numbers"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "alpha\nBETA\nGAMMA\n");
    }

    #[tokio::test]
    async fn prefix_is_removed_from_the_new_string_lines_that_have_one() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "alpha\nbeta\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"2\tbeta","new_string":"2\tBETA\nplain"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "alpha\nBETA\nplain\n");
    }

    #[tokio::test]
    async fn prefix_strip_then_fallback_keeps_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\r\n  b  \r\n  c\r\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"2\tb\n3\tc","new_string":"2\tB\n3\tC"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("removing read line numbers and trimming whitespace"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "a\r\n  B\r\n  C\r\n");
    }

    #[tokio::test]
    async fn escape_level_removed_matches_a_doubled_backslash() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.rs", "if s != \"\\n\" {\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.rs",
                "old_string": "if s != \"\\\\n\" {",
                "new_string": "if s == \"\\\\n\" {"
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("removing one level of backslash escapes"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.rs").await, "if s == \"\\n\" {\n");
    }

    #[tokio::test]
    async fn escape_level_removed_matches_an_escaped_quote() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.py", "x = 'yes'\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.py",
                "old_string": "x = \\'yes\\'",
                "new_string": "x = \\'no\\'"
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("removing one level of backslash escapes"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.py").await, "x = 'no'\n");
    }

    #[tokio::test]
    async fn escape_level_no_match_still_fails() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.rs", "if s != \"\\n\" {\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.rs",
                "old_string": "if t != \"\\\\n\" {",
                "new_string": "if t == \"\\\\n\" {"
            }),
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("text not found"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.rs").await, "if s != \"\\n\" {\n");
    }

    #[tokio::test]
    async fn prefix_mixed_old_string_is_not_stripped() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "alpha\nbeta\ngamma\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"2\tbeta\ngamma","new_string":"x"}),
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "alpha\nbeta\ngamma\n");
    }

    #[tokio::test]
    async fn prefix_exact_match_with_number_text_is_used_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "1\tfoo\nbar\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"1\tfoo","new_string":"1\tbaz"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            !output.content.contains("read line numbers"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "1\tbaz\nbar\n");
    }

    #[tokio::test]
    async fn fallback_trailing_whitespace_matches_tier_one() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a  \nb\t\nc\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X\nY"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("trimming trailing whitespace"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("read the file"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "X\nY\nc\n");
    }

    #[tokio::test]
    async fn fallback_indentation_replaces_the_original_lines() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "fn f() {\n        a();\n        b();\n}\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a();\nb();\n","new_string":"c();\n"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("matched after trimming whitespace"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("indented to match"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "fn f() {\n        c();\n}\n");
    }

    #[tokio::test]
    async fn fallback_indentation_keeps_relative_indent() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.py", "def f():\n    if x:\n        y()\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.py","old_string":"if x:\n    y()","new_string":"if x:\n    z()\n\n    w()"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            read(&tmp, "f.py").await,
            "def f():\n    if x:\n        z()\n\n        w()\n"
        );
    }

    #[tokio::test]
    async fn fallback_uneven_indent_inserts_new_string_as_given() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "  a\n    b\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"c"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(!output.content.contains("indented"), "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "c\n");
    }

    #[tokio::test]
    async fn fallback_punctuation_matches_tier_three() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp,
            "f.txt",
            "say \u{201c}hi\u{201d} \u{2014} ok\u{a0}now\nnext\n",
        )
        .await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"say \"hi\" - ok now","new_string":"done"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("normalising punctuation"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "done\nnext\n");
    }

    #[tokio::test]
    async fn fallback_tiers_run_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "  a\n  b\na \nb \n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("trimming trailing whitespace"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "  a\n  b\nX\n");
    }

    #[tokio::test]
    async fn fallback_multiple_matches_is_an_error_with_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x\n a\n b\ny\n a\n b\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}),
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("2 time"), "{}", output.content);
        assert!(output.content.contains("2, 5"), "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "x\n a\n b\ny\n a\n b\n");
    }

    #[tokio::test]
    async fn fallback_replace_all_replaces_every_match_at_the_tier() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x\n a\n b\ny\n a\n b\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X","replace_all":true}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("replaced 2 occurrence"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "x\n X\ny\n X\n");
    }

    #[tokio::test]
    async fn fallback_keeps_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\r\n  b \r\nc\r\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"x\ny"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "x\r\ny\r\nc\r\n");
    }

    #[tokio::test]
    async fn fallback_is_not_used_for_an_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a","new_string":"X"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            !output.content.contains("matched after"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn fallback_not_found_keeps_the_hint() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"zzz","new_string":"X"}),
        )
        .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("text not found"),
            "{}",
            output.content
        );
    }

    #[test]
    fn edit_effects_are_a_write() {
        let edit = Edit::new("/work");
        assert_eq!(edit.main_arg(), Some("path"));
        let effects = edit.effects(r#"{"path":"a.txt","old_string":"a","new_string":"b"}"#);
        assert_eq!(effects.writes, vec![PathBuf::from("/work/a.txt")]);
        assert!(effects.reads.is_empty());
    }

    #[test]
    fn edit_bad_args_have_no_effects() {
        let edit = Edit::new("/work");
        assert_eq!(edit.effects("{}"), Effects::default());
        // Every text has to be a string and the call an object: `run` takes
        // none of these, so the effects report nothing.
        assert_eq!(
            edit.effects(r#"{"path":"a.txt","old_string":1,"new_string":"b"}"#),
            Effects::default()
        );
        assert_eq!(
            edit.effects(r#"{"path":"a.txt","edits":[{"old_string":[],"new_string":"b"}]}"#),
            Effects::default()
        );
        assert_eq!(edit.effects(r#"["a.txt","a","b"]"#), Effects::default());
    }

    #[tokio::test]
    async fn multi_edit_applies_two_replacements() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\nthree\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "three", "new_string": "3"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "1\ntwo\n3\n");
        let path = tmp.path().join("f.txt");
        assert_eq!(
            output.content.as_ref(),
            format!(
                "edited {}: 2 of 2 edits applied, first change at line 1\n; now:\n1\t1\n2\ttwo\n3\t3\n",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn multi_edit_later_edits_apply_to_earlier_results() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nx\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "b"},
                    {"old_string": "b", "new_string": "c"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "c\nx\n");
    }

    #[tokio::test]
    async fn partial_edit_failure_keeps_the_edits_that_matched() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "missing", "new_string": "x"}
                ]
            }),
        )
        .await;
        let path = tmp.path().join("f.txt");
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.starts_with(&format!(
                "edited {}: 1 of 2 edits applied, first change at line 1",
                path.display()
            )),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("\nedit 2: text not found"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "1\ntwo\n");
    }

    #[tokio::test]
    async fn multi_edit_rejects_both_forms_and_neither_form() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let both = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "old_string": "one",
                "new_string": "1",
                "edits": [{"old_string": "two", "new_string": "2"}]
            }),
        )
        .await;
        assert!(both.is_error, "{}", both.content);
        assert_eq!(both.content.as_ref(), super::INVALID_FORM);
        let neither = run_edit(&tmp, serde_json::json!({"path": "f.txt"})).await;
        assert!(neither.is_error, "{}", neither.content);
        assert_eq!(neither.content.as_ref(), super::INVALID_FORM);
        let half = run_edit(
            &tmp,
            serde_json::json!({"path": "f.txt", "old_string": "one"}),
        )
        .await;
        assert!(half.is_error, "{}", half.content);
        assert_eq!(half.content.as_ref(), super::INVALID_FORM);
        assert_eq!(read(&tmp, "f.txt").await, "one\ntwo\n");
    }

    #[tokio::test]
    async fn multi_edit_empty_list_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let output = run_edit(&tmp, serde_json::json!({"path": "f.txt", "edits": []})).await;
        assert!(output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), super::INVALID_FORM);
        assert_eq!(read(&tmp, "f.txt").await, "one\ntwo\n");
    }

    #[tokio::test]
    async fn multi_edit_list_replace_all_works() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x foo\ny foo\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "foo", "new_string": "bar", "replace_all": true}]
            }),
        )
        .await;
        let path = tmp.path().join("f.txt");
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.starts_with(&format!(
                "edited {}: 1 of 1 edits applied, first change at line 1",
                path.display()
            )),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, "x bar\ny bar\n");
    }

    #[tokio::test]
    async fn multi_edit_single_form_output_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "aaa bbb ccc\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path": "f.txt", "old_string": "bbb", "new_string": "BBB"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content.as_ref(),
            format!(
                "replaced 1 occurrence(s) in {}; now:\n1\taaa BBB ccc\n",
                tmp.path().join("f.txt").display()
            )
        );
        assert_eq!(read(&tmp, "f.txt").await, "aaa BBB ccc\n");
    }

    #[test]
    fn multi_edit_effects_report_the_same_path() {
        let edit = Edit::new("/work");
        let effects = edit.effects(
            r#"{"path":"a.txt","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d"}]}"#,
        );
        assert_eq!(effects.writes, vec![PathBuf::from("/work/a.txt")]);
        assert!(effects.reads.is_empty());
    }

    #[tokio::test]
    async fn partial_edit_one_miss_among_three_names_the_failed_edit() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\nthree\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "nowhere", "new_string": "x"},
                    {"old_string": "three", "new_string": "3"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(text.starts_with(&format!(
            "edited {}: 2 of 3 edits applied, first change at line 1",
            tmp.path().join("f.txt").display()
        )));
        assert!(text.contains("\nedit 2: text not found"), "{text}");
        assert!(!text.contains("edit 1:"), "{text}");
        assert!(!text.contains("edit 3:"), "{text}");
        assert_eq!(read(&tmp, "f.txt").await, "1\ntwo\n3\n");
    }

    #[tokio::test]
    async fn partial_edit_swap_proves_original_matching() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "b"},
                    {"old_string": "b", "new_string": "a"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(read(&tmp, "f.txt").await, "b\na\n");
    }

    #[tokio::test]
    async fn partial_edit_dependent_edit_gets_a_second_try() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nkeep\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "X"},
                    {"old_string": "X", "new_string": "Y"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(text.contains("2 of 2 edits applied"), "{text}");
        assert_eq!(read(&tmp, "f.txt").await, "Y\nkeep\n");
    }

    #[tokio::test]
    async fn partial_edit_already_applied_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x\ny\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "gone", "new_string": "x"}]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(
            text.starts_with(&format!(
                "edited {}: 0 of 1 edits applied, 1 already applied",
                tmp.path().join("f.txt").display()
            )),
            "{text}"
        );
        assert_eq!(read(&tmp, "f.txt").await, "x\ny\n");
    }

    #[tokio::test]
    async fn partial_edit_overlap_refuses_the_later_edit() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "abc").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "ab", "new_string": "X"},
                    {"old_string": "bc", "new_string": "Y"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(text.contains("1 of 2 edits applied"), "{text}");
        assert!(
            text.contains(
                "\nedit 2: overlaps edit 1; merge them into one edit or pick separate text"
            ),
            "{text}"
        );
        assert_eq!(read(&tmp, "f.txt").await, "Xc");
    }

    #[tokio::test]
    async fn partial_edit_no_change_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "a", "new_string": "a"}]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(output.is_error, "{text}");
        assert!(text.contains("\nedit 1: changes nothing"), "{text}");
        assert_eq!(read(&tmp, "f.txt").await, "a\n");
    }

    #[tokio::test]
    async fn partial_edit_fuzzy_two_matches_lists_both_lines() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x\n a\n b\ny\n a\n b\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "a\nb", "new_string": "X"}]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(output.is_error, "{text}");
        assert!(text.contains("\nedit 1: text matches 2 time"), "{text}");
        assert!(text.contains("2, 5"), "{text}");
        assert_eq!(read(&tmp, "f.txt").await, "x\n a\n b\ny\n a\n b\n");
    }

    #[tokio::test]
    async fn partial_edit_fuzzy_match_keeps_its_note() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x\n a\n b\ny\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "a\nb", "new_string": "X"}]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(
            text.contains("\nedit 1: matched after trimming whitespace"),
            "{text}"
        );
        assert!(text.contains("indented to match"), "{text}");
        assert!(
            text.contains("read the file before the next edit"),
            "{text}"
        );
        assert_eq!(read(&tmp, "f.txt").await, "x\n X\ny\n");
    }

    #[tokio::test]
    async fn partial_edit_all_failed_is_an_error_and_keeps_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "zzz", "new_string": "qqq"},
                    {"old_string": "yyy", "new_string": "www"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(output.is_error, "{text}");
        assert!(text.contains("0 of 2 edits applied"), "{text}");
        assert!(text.contains("\nedit 1: text not found"), "{text}");
        assert!(text.contains("\nedit 2: text not found"), "{text}");
        let got = tokio::fs::read(tmp.path().join("f.txt")).await.unwrap();
        assert_eq!(got.as_slice(), b"one\ntwo\n");
    }

    #[tokio::test]
    async fn partial_edit_reply_reports_the_first_change_line() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\nthree\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "three", "new_string": "3"},
                    {"old_string": "two", "new_string": "2"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert_eq!(
            text,
            &format!(
                "edited {}: 2 of 2 edits applied, first change at line 2\n; now:\n1\tone\n2\t2\n3\t3\n",
                tmp.path().join("f.txt").display()
            )
        );
        assert_eq!(read(&tmp, "f.txt").await, "one\n2\n3\n");
    }

    /// How many not-found hints this thread has built so far.
    fn hints_built() -> usize {
        HINTS_BUILT.with(std::cell::Cell::get)
    }

    #[tokio::test]
    async fn hot_edit_overlap_check_scales() {
        let tmp = tempfile::tempdir().unwrap();
        // Two edits that match 100000 times each: 200000 spans for the overlap
        // check, which must not compare every span with every other one.
        write(&tmp, "f.txt", &"ab\n".repeat(100_000)).await;
        let start = std::time::Instant::now();
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "x", "replace_all": true},
                    {"old_string": "b", "new_string": "y", "replace_all": true}
                ]
            }),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "took {elapsed:?}"
        );
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(text.contains("2 of 2 edits applied"), "{text}");
        assert_eq!(
            read(&tmp, "f.txt").await,
            "xy\n".repeat(100_000),
            "the two edits did not both land"
        );
    }

    #[tokio::test]
    async fn hot_edit_overlaps_still_fail_the_later_edit() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "abcdef").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "ab", "new_string": "X"},
                    {"old_string": "bc", "new_string": "Y"},
                    {"old_string": "de", "new_string": "Z"}
                ]
            }),
        )
        .await;
        let text = output.content.as_ref();
        assert!(!output.is_error, "{text}");
        assert!(text.contains("2 of 3 edits applied"), "{text}");
        assert!(
            text.contains(
                "\nedit 2: overlaps edit 1; merge them into one edit or pick separate text"
            ),
            "{text}"
        );
        assert!(!text.contains("edit 1:"), "{text}");
        assert!(!text.contains("edit 3:"), "{text}");
        assert_eq!(read(&tmp, "f.txt").await, "XcZf");
    }

    #[tokio::test]
    async fn hot_edit_recovered_edit_builds_no_hint() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nkeep\n").await;
        let before = hints_built();
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "X"},
                    {"old_string": "X", "new_string": "Y"}
                ]
            }),
        )
        .await;
        assert_eq!(
            hints_built(),
            before,
            "an edit that the second try recovers must not build a hint"
        );
        assert_eq!(
            output.content.as_ref(),
            format!(
                "edited {}: 2 of 2 edits applied, first change at line 1\n; now:\n1\tY\n2\tkeep\n",
                tmp.path().join("f.txt").display()
            )
        );
        assert_eq!(read(&tmp, "f.txt").await, "Y\nkeep\n");
    }

    /// A file of `count` lines of `filler`, with `text` on each line in `marks`.
    fn marked(count: usize, marks: &[usize], text: &str) -> String {
        (1..=count)
            .map(|n| {
                if marks.contains(&n) {
                    format!("{text}\n")
                } else {
                    "filler\n".to_string()
                }
            })
            .collect()
    }

    /// A file of `count` lines, `line N` each.
    fn numbered(count: usize) -> String {
        let mut out = String::new();
        for n in 1..=count {
            let _ = writeln!(out, "line {n}");
        }
        out
    }

    /// The lines of a file, for a test that checks which one changed.
    async fn lines_of(tmp: &tempfile::TempDir, name: &str) -> Vec<String> {
        read(tmp, name).await.lines().map(str::to_string).collect()
    }

    #[tokio::test]
    async fn edit_line_hint_picks_the_match_nearest_the_line() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(40, &[10, 30], "target")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"target","new_string":"hit","line":28}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("replaced 1 occurrence"),
            "{}",
            output.content
        );
        let lines = lines_of(&tmp, "f.txt").await;
        assert_eq!(lines[9], "target");
        assert_eq!(lines[29], "hit");
    }

    #[tokio::test]
    async fn edit_line_hint_counts_lines_inside_a_multi_line_match() {
        // The block starts on lines 10 and 20, so line 15 is a tie and the
        // earlier copy wins. Counting the second copy from the end of the
        // first, without the two newlines inside it, put it on line 18.
        let tmp = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for n in 1..=30 {
            match n {
                10 | 20 => text.push_str("a\n"),
                11 | 21 => text.push_str("b\n"),
                12 | 22 => text.push_str("c\n"),
                _ => text.push_str(&format!("line {n}\n")),
            }
        }
        write(&tmp, "f.txt", &text).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb\nc","new_string":"x\ny\nz","line":15}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let lines = lines_of(&tmp, "f.txt").await;
        assert_eq!(&lines[9..12], ["x", "y", "z"]);
        assert_eq!(&lines[19..22], ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn edit_line_hint_tie_goes_to_the_earlier_match() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(40, &[10, 30], "target")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"target","new_string":"hit","line":20}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let lines = lines_of(&tmp, "f.txt").await;
        assert_eq!(lines[9], "hit");
        assert_eq!(lines[29], "target");
    }

    #[tokio::test]
    async fn edit_line_hint_is_ignored_when_one_place_matches() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(40, &[5], "target")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"target","new_string":"hit","line":30}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let lines = lines_of(&tmp, "f.txt").await;
        assert_eq!(lines[4], "hit");
        assert_eq!(lines[29], "filler");
    }

    #[tokio::test]
    async fn edit_line_hint_absent_names_the_flag() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(40, &[10, 30], "target")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"target","new_string":"hit"}),
        )
        .await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("or pass line to pick one"),
            "{}",
            output.content
        );
        assert_eq!(read(&tmp, "f.txt").await, marked(40, &[10, 30], "target"));
    }

    #[tokio::test]
    async fn edit_line_hint_picks_inside_an_edits_list() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(40, &[3, 30], "target")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{"old_string": "target", "new_string": "hit", "line": 28}]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output.content.contains("1 of 1 edits applied"),
            "{}",
            output.content
        );
        let lines = lines_of(&tmp, "f.txt").await;
        assert_eq!(lines[2], "target");
        assert_eq!(lines[29], "hit");
    }

    #[tokio::test]
    async fn edit_echo_shows_the_changed_lines_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &numbered(12)).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"line 10","new_string":"LINE TEN"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let path = tmp.path().join("f.txt");
        assert_eq!(
            output.content.as_ref(),
            format!(
                "replaced 1 occurrence(s) in {}; now:\n9\tline 9\n10\tLINE TEN\n11\tline 11\n",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn edit_echo_shows_a_multi_line_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &numbered(12)).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"line 3\nline 4","new_string":"x\ny\nz"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let path = tmp.path().join("f.txt");
        assert_eq!(
            output.content.as_ref(),
            format!(
                "replaced 1 occurrence(s) in {}; now:\n2\tline 2\n3\tx\n4\ty\n5\tz\n6\tline 5\n",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn edit_echo_keeps_the_cr_of_a_crlf_file_out_of_the_text() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\r\nb\r\nc\r\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"b","new_string":"B"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let path = tmp.path().join("f.txt");
        assert_eq!(
            output.content.as_ref(),
            format!(
                "replaced 1 occurrence(s) in {}; now:\n1\ta\n2\tB\n3\tc\n",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn edit_echo_replace_all_shows_three_regions_and_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &marked(60, &[5, 15, 25, 35, 45], "needle")).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"needle","new_string":"thread","replace_all":true}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("replaced 5 occurrence"), "{text}");
        assert_eq!(
            text.lines().filter(|line| line.contains('\t')).count(),
            9,
            "{text}"
        );
        assert!(text.contains("\n...\n"), "{text}");
        assert!(text.contains("... (2 more)"), "{text}");
        assert!(!text.contains("35\tthread"), "{text}");
    }

    #[tokio::test]
    async fn edit_echo_caps_the_lines_at_forty() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &numbered(200)).await;
        let mut many = String::new();
        for n in 1..=100 {
            let _ = writeln!(many, "new {n}");
        }
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"line 3\nline 4","new_string":many}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert_eq!(
            text.lines().filter(|line| line.contains('\t')).count(),
            40,
            "{text}"
        );
        assert!(text.ends_with("...\n"), "{text}");
    }

    /// The `edits` list form shows what it changed too, with the same one line
    /// of context and the same `...` between regions.
    #[tokio::test]
    async fn edit_echo_the_edits_list_shows_what_it_changed() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", &numbered(12)).await;
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "line 3", "new_string": "THREE"},
                    {"old_string": "line 10", "new_string": "TEN"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content.as_ref(),
            format!(
                "edited {}: 2 of 2 edits applied, first change at line 3\n; now:\n2\tline 2\n3\tTHREE\n4\tline 4\n...\n9\tline 9\n10\tTEN\n11\tline 11\n",
                tmp.path().join("f.txt").display()
            )
        );
    }

    /// Two changed lines three apart share a context line, so their windows
    /// merge and no `...` is printed; four apart, one line is skipped.
    #[tokio::test]
    async fn edit_echo_windows_that_touch_merge() {
        let near = tempfile::tempdir().unwrap();
        write(&near, "f.txt", &numbered(12)).await;
        let output = run_edit(
            &near,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "line 4", "new_string": "A"},
                    {"old_string": "line 7", "new_string": "B"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("; now:\n3\tline 3\n4\tA\n5\tline 5\n6\tline 6\n7\tB\n8\tline 8\n"),
            "{}",
            output.content
        );

        let far = tempfile::tempdir().unwrap();
        write(&far, "f.txt", &numbered(12)).await;
        let output = run_edit(
            &far,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "line 4", "new_string": "A"},
                    {"old_string": "line 8", "new_string": "B"}
                ]
            }),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("; now:\n3\tline 3\n4\tA\n5\tline 5\n...\n7\tline 7\n8\tB\n9\tline 9\n"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn edit_echo_keeps_the_fuzzy_note_before_it() {
        let tmp = tempfile::tempdir().unwrap();
        // The trailing space of line 2 makes the exact match miss and the
        // trailing-whitespace tier take it.
        write(&tmp, "f.txt", "x\na \nb\ny\n").await;
        let output = run_edit(
            &tmp,
            serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        let note = text
            .find("matched after trimming trailing whitespace")
            .expect("the note");
        let now = text.find("; now:").expect("the echo");
        assert!(note < now, "{text}");
        assert!(text.contains("\n1\tx\n2\tX\n3\ty\n"), "{text}");
    }

    #[tokio::test]
    async fn hot_edit_a_missing_edit_still_builds_one_hint() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "one\ntwo\n").await;
        let before = hints_built();
        let output = run_edit(
            &tmp,
            serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "one", "new_string": "1"},
                    {"old_string": "nowhere", "new_string": "x"}
                ]
            }),
        )
        .await;
        assert_eq!(hints_built() - before, 1, "one hint, built once");
        assert!(
            output.content.contains("\nedit 2: text not found"),
            "{}",
            output.content
        );
    }
}

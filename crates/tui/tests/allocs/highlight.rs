//! Allocation counts for one line of a fenced code block.
//!
//! [`tokenize`] reports every span as a byte range of the line and writes it
//! into the `Vec` the caller hands in, which it clears first, so a line costs
//! no heap block once that `Vec` has its capacity. A line a block comment
//! carries into the next one costs no block either: the state it leaves is a
//! copy of a `Copy` type, and the whole line is one span of the same `Vec`.
//!
//! dhat allows one profiler per process, so every test locks [`SERIAL`] for its
//! whole body: a test that ran beside a profiler would be counted in its
//! numbers.

use std::sync::{MutexGuard, PoisonError};

use dhat::{HeapStats, Profiler};
use reagent_tui::highlight::{Lang, Span, State, Token, tokenize};

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many times a measurement runs. dhat counts the blocks of every thread of
/// the process, so one sample can carry a block that is not the work's; the
/// smallest of several is the work's own count.
const SAMPLES: usize = 3;

/// The Rust lines a measurement tokenizes: enough that the span `Vec` has grown
/// to the widest of them and kept the capacity.
const RUST_LINES: usize = 100;

/// The lines the C block comment runs over, so the state carries the open
/// comment past the end of fifty lines.
const COMMENT_LINES: usize = 50;

/// The blocks a warm span buffer costs per line: none.
const LINE_BLOCKS: u64 = 0;

/// The smallest number of blocks `body` allocates over [`SAMPLES`] runs.
fn blocks(mut body: impl FnMut()) -> u64 {
    (0..SAMPLES)
        .map(|_| {
            let profiler = Profiler::builder().testing().build();
            body();
            let stats = HeapStats::get();
            drop(profiler);
            stats.total_blocks
        })
        .min()
        .expect("at least one sample")
}

/// [`RUST_LINES`] lines of Rust, each with a keyword, a call, a type, a number,
/// a string and a comment, so one line fills the span `Vec` many times over.
fn rust_lines() -> Vec<String> {
    (0..RUST_LINES)
        .map(|n| {
            format!(
                "fn line_{n}(width: usize) -> usize {{ let s = \"text\"; /* gap */ width + {n} }}"
            )
        })
        .collect()
}

/// [`COMMENT_LINES`] lines of C that open a block comment on the first and close
/// it on the last, so every line between them starts inside the comment.
fn comment_lines() -> Vec<String> {
    let mut lines = Vec::with_capacity(COMMENT_LINES);
    lines.push("/* a block comment that runs past the end of its line".to_owned());
    for n in 2..COMMENT_LINES {
        lines.push(format!(" * line {n} of the comment, still open"));
    }
    lines.push(" */".to_owned());
    lines
}

/// The spans of `lines` from an empty state, into `out`.
fn tokenize_all(lang: &Lang, lines: &[String], out: &mut Vec<Span>) {
    let mut state = State::default();
    for line in lines {
        tokenize(lang, line, &mut state, out);
    }
}

/// Tokenizing a hundred lines of Rust twice into the same `Vec` allocates
/// nothing the second time: the line is not copied, and a span only grows the
/// byte range of the last one when the tokens match.
#[test]
fn tokenize_allocates_nothing_on_a_warm_buffer() {
    let _serial = serial();
    let lang = Lang::from_tag("rust").expect("rust is in the table");
    let lines = rust_lines();
    let mut out = Vec::new();
    // The warm-up run takes the capacity the widest of the lines needs.
    tokenize_all(&lang, &lines, &mut out);
    let spans = out.len();
    assert!(spans > 1, "a line of Rust is more than one span");
    let last = lines.last().expect("a last line");
    assert_eq!(
        out.last().map(|span| span.end as usize),
        Some(last.len()),
        "the spans of a line cover it whole"
    );

    // A cold buffer allocates, so the zero below is the warm one's work and not
    // a profiler that counts nothing.
    let mut cold = Vec::new();
    let profiler = Profiler::builder().testing().build();
    tokenize_all(&lang, &lines, &mut cold);
    let grew = HeapStats::get().total_blocks;
    drop(profiler);
    assert!(grew > 0, "a cold span buffer allocates nothing either");

    let measured = blocks(|| tokenize_all(&lang, &lines, &mut out));
    eprintln!("blocks: {measured} for {RUST_LINES} lines of Rust, {spans} spans each");
    assert_eq!(
        measured, LINE_BLOCKS,
        "{measured} blocks for {RUST_LINES} warm lines, over {LINE_BLOCKS}"
    );
    assert_eq!(out.last().map(|span| span.end as usize), Some(last.len()));
}

/// A block comment that stays open over fifty lines allocates nothing once the
/// buffer is warm: every line between the open and the close is one comment span
/// of the line it was given, and the state that carries it is a copy.
#[test]
fn tokenize_carries_state_without_allocating() {
    let _serial = serial();
    let lang = Lang::from_tag("c").expect("c is in the table");
    let lines = comment_lines();
    let mut out = Vec::new();
    tokenize_all(&lang, &lines, &mut out);
    // The last line closes the comment, so the state comes back to nothing and
    // the count below is the comment's own spans.
    assert_eq!(out.last().map(|span| span.end as usize), Some(3));

    // A middle line opens inside the comment: one span covers it whole.
    let mut state = State::default();
    let middle = &lines[COMMENT_LINES / 2];
    for line in &lines[..=COMMENT_LINES / 2] {
        tokenize(&lang, line, &mut state, &mut out);
    }
    assert_eq!(
        out.as_slice(),
        [Span {
            start: 0,
            end: middle.len() as u32,
            token: Token::Comment,
        }],
        "a line inside the comment is one comment span"
    );

    let measured = blocks(|| tokenize_all(&lang, &lines, &mut out));
    eprintln!("blocks: {measured} for a {COMMENT_LINES} line block comment");
    assert_eq!(
        measured, LINE_BLOCKS,
        "{measured} blocks for a warm {COMMENT_LINES} line comment, over {LINE_BLOCKS}"
    );
}

//! Allocation counts for a streamed markdown block.
//!
//! A delta that does not finish a block re-parses and re-lays out only the open
//! block, and reuses every buffer it filled before, so the reply's length must
//! not show up in the count. dhat allows one profiler per process, so every test
//! locks [`SERIAL`] for its whole body.

use std::sync::MutexGuard;

use dhat::{HeapStats, Profiler};
use reagent_tui::markdown::Markdown;

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many times a measurement runs. dhat counts the blocks of every thread of
/// the process, so one sample can carry a few blocks that are not the work's;
/// the smallest of several is the work's own count.
const SAMPLES: usize = 3;

/// The blocks `body` allocates, once.
fn once(body: impl FnOnce()) -> u64 {
    let profiler = Profiler::builder().testing().build();
    body();
    let stats = HeapStats::get();
    drop(profiler);
    stats.total_blocks
}

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

/// A reply that ends in an open paragraph: a finished block, so the frozen path
/// is exercised as well as the open one.
fn reply(paragraphs: usize) -> String {
    let mut text = String::from("# Report\n\n");
    for i in 0..paragraphs {
        text.push_str(&format!(
            "Paragraph {i} says the suite fails on `cargo test` and that **nextest** \
             is the way to run it.\n\n"
        ));
    }
    text.push_str("- first item\n- second item\n\n");
    text.push_str("The open paragraph keeps growing while the reply streams");
    text
}

/// A delta of five bytes: a word and the space after it, which no construct can
/// start or close, so the open block is never parsed again.
const DELTA: &str = "word ";

/// The columns the reply is laid out at, wide enough that a run of [`DELTA`]s
/// stays on one row.
const WIDTH: u16 = 2000;

/// The deltas appended before the measured run, so the text buffer, the rows and
/// the pieces of every row have their capacity.
const WARM: usize = 2000;

/// The deltas one measurement appends.
const DELTAS: usize = 200;

/// The Rust code lines of the block a measured delta streams into: 6 KB of code,
/// so the block behind the open line is long.
const CODE_LINES: usize = 280;

/// A delta that does not finish a block allocates nothing once the buffers are
/// warm: the text is one `String` push, the last span only grows its byte range,
/// and the live rows reuse the piece vector of every row they replace.
#[test]
fn perf_markdown_delta_allocates_nothing() {
    let _serial = serial();
    let mut markdown = Markdown::new(WIDTH);
    markdown.push(&reply(4));
    for _ in 0..WARM {
        markdown.push(DELTA);
    }
    let before = markdown.text().len();
    let measured = blocks(|| {
        for _ in 0..DELTAS {
            markdown.push(DELTA);
        }
    });
    eprintln!(
        "blocks: {measured} for {DELTAS} deltas = {:.3} per delta",
        measured as f64 / DELTAS as f64
    );
    // 4 measured. Each block is a row the run grew into, not the delta: the
    // `Line` pushed into the rows and the first push into its piece vector, so
    // two rows cost 4. A delta that stays on its row costs none.
    const DELTA_BLOCKS: u64 = 4;
    assert!(
        measured <= DELTA_BLOCKS,
        "{measured} blocks for {DELTAS} deltas, over {DELTA_BLOCKS}"
    );
    assert_eq!(
        markdown.text().len(),
        before + SAMPLES * DELTAS * DELTA.len()
    );
}

/// A delta into an open fence costs the delta, not the fence: the same work goes
/// into a 4 KB fence as into a 40 KB one, because the delta is appended to the
/// last code line without a parse.
#[test]
fn perf_markdown_fence_delta_cost_does_not_follow_the_block() {
    let _serial = serial();
    let delta = "if x > 1 { y(); }";
    let small = fence_blocks(280, delta);
    let big = fence_blocks(2_800, delta);
    eprintln!("blocks: {small} per delta into a 4 KB fence, {big} into a 40 KB one");
    // 0 measured: the last code line only grows its byte range. The bound leaves
    // room for the rows a long line wraps into the first time it reaches them and
    // for a block another thread allocates under the profiler.
    const BLOCKS: u64 = 2;
    assert!(big <= BLOCKS, "{big} blocks per delta, over {BLOCKS}");
    assert!(
        big <= small + 2,
        "{big} blocks per delta into a 40 KB fence against {small} into a 4 KB one"
    );
}

/// A backtick inside a line of code is not a closing fence, so it does not
/// re-parse the block: the delta costs the delta, not the fence behind it.
#[test]
fn perf_markdown_backtick_delta_does_not_follow_the_block() {
    let _serial = serial();
    let delta = "echo `date` and `hostname` ";
    let small = fence_blocks(280, delta);
    let big = fence_blocks(2_800, delta);
    eprintln!("blocks: {small} per backtick delta into a 4 KB fence, {big} into a 40 KB one");
    // 0 measured: the last code line only grows its byte range, as with a plain
    // delta, since only a line of three fence characters can close the block.
    const BLOCKS: u64 = 2;
    assert!(big <= BLOCKS, "{big} blocks per delta, over {BLOCKS}");
    assert!(
        big <= small + 2,
        "{big} blocks per delta into a 40 KB fence against {small} into a 4 KB one"
    );
}

/// The blocks `DELTAS` deltas into an open fence of `lines` code lines cost.
fn fence_blocks(lines: usize, delta: &str) -> u64 {
    let mut markdown = Markdown::new(WIDTH);
    markdown.push("```rust\n");
    let code = "let x = 1;\n".repeat(lines);
    markdown.push(&code);
    blocks(|| {
        for _ in 0..DELTAS {
            markdown.push(delta);
        }
    }) / DELTAS as u64
}

/// A delta streamed into the open line of a long Rust block costs the delta: the
/// line is not parsed again, so its tokens are not found again and the code
/// above it is not laid out again.
#[test]
fn a_streamed_delta_in_a_code_block_keeps_its_bound() {
    let _serial = serial();
    let mut markdown = Markdown::new(WIDTH);
    markdown.push("```rust\n");
    markdown.push(&"let width = line.len();\n".repeat(CODE_LINES));
    for _ in 0..WARM {
        markdown.push(DELTA);
    }
    let before = markdown.text().len();
    let measured = blocks(|| markdown.push(DELTA));
    eprintln!("blocks: {measured} for one delta into a {CODE_LINES} line Rust block");
    // The bound of the fence cases, kept: the open code line only grows the byte
    // range of its last piece, and the row it fills is the one it filled before.
    const BLOCKS: u64 = 2;
    assert!(
        measured <= BLOCKS,
        "{measured} blocks for one delta into a {CODE_LINES} line block, over {BLOCKS}"
    );
    assert_eq!(
        markdown.text().len(),
        before + SAMPLES * DELTA.len(),
        "every sample appended one delta"
    );
}

/// A structural delta re-parses the open block and lays it out again, but the
/// buffers it fills are the ones it filled before: the cost follows the open
/// block's size, not the length of the reply behind it.
#[test]
fn perf_markdown_free_delta_cost_does_not_follow_the_reply() {
    let _serial = serial();
    let delta = " `code` and **bold** ";
    let short_blocks = (0..SAMPLES)
        .map(|_| {
            let mut short = Markdown::new(WIDTH);
            short.push("open paragraph");
            once(|| short.push(delta))
        })
        .min()
        .expect("at least one sample");
    let long_blocks = (0..SAMPLES)
        .map(|_| {
            let mut long = Markdown::new(WIDTH);
            long.push(&reply(200));
            once(|| long.push(delta))
        })
        .min()
        .expect("at least one sample");
    eprintln!("blocks: {short_blocks} after a short reply, {long_blocks} after a long one");
    // 10 measured after a long reply: the parser's own buffers, the event
    // iterator, one vector of spans and the rows laid out again. None of it
    // follows the reply's length.
    const BLOCKS: u64 = 10;
    assert!(
        long_blocks <= BLOCKS,
        "{long_blocks} blocks after a long reply, over {BLOCKS}"
    );
    assert!(
        long_blocks <= short_blocks + 32,
        "{long_blocks} blocks after a long reply against {short_blocks} after a short one"
    );
}

/// Finishing a block lays out the block it froze once, so its cost follows that
/// block, not the reply behind it: a 200 paragraph reply and a one paragraph
/// reply freeze the open paragraph for the same number of blocks.
#[test]
fn perf_markdown_finish_block_cost_does_not_follow_the_reply() {
    let _serial = serial();
    let finish = "\n\nthe next block starts\n";
    let short_blocks = (0..SAMPLES)
        .map(|_| {
            let mut short = Markdown::new(80);
            short.push("one paragraph of the reply");
            once(|| short.push(finish))
        })
        .min()
        .expect("at least one sample");
    let long_blocks = (0..SAMPLES)
        .map(|_| {
            let mut long = Markdown::new(80);
            long.push(&reply(200));
            once(|| long.push(finish))
        })
        .min()
        .expect("at least one sample");
    eprintln!("blocks: {short_blocks} to finish a short reply, {long_blocks} a long one");
    // 14 measured: the parse of the open paragraph, its spans, and the rows of
    // the one block it froze.
    const BLOCKS: u64 = 14;
    assert!(
        long_blocks <= BLOCKS,
        "{long_blocks} blocks to finish a block of a long reply, over {BLOCKS}"
    );
    assert!(
        long_blocks <= short_blocks + 16,
        "{long_blocks} blocks against {short_blocks}: it grows with the reply"
    );
}

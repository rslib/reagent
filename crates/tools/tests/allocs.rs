//! Allocation counts for the hot paths of `edit` and `read`.
//!
//! Both tools stream a file that can be large, so the work they do must not
//! allocate per match or per line. dhat allows one profiler at a time, so the
//! two cases run in sequence inside one test.

use dhat::{Alloc, HeapStats, Profiler};

use reagent_agent::tool::Tool;
use reagent_tools::edit::Edit;
use reagent_tools::read::Read;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Lines in the measured file of each case.
const LINES: usize = 10_000;

/// The most blocks one `replace_all` edit over `LINES` matches of a CRLF file
/// may allocate: about twice the 52 measured. It cannot scale with the file.
const EDIT_BLOCKS: u64 = 120;
const _: () = assert!(EDIT_BLOCKS < 200);

/// The most blocks one read of `LINES` lines may allocate: about three times
/// the 19 measured, and well under one block per line.
const READ_BLOCKS: u64 = 60;
const _: () = assert!(READ_BLOCKS < 1000);

/// Runs `body` under a fresh profiler and returns the blocks it allocated.
fn blocks(body: impl FnOnce()) -> u64 {
    let profiler = Profiler::builder().testing().build();
    body();
    let stats = HeapStats::get();
    drop(profiler);
    stats.total_blocks
}

#[test]
fn edit_and_read_allocate_per_file_not_per_match() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let cancel = CancellationToken::new();

    // A CRLF file where one replace_all edit matches on every line. The
    // edit spans a line end, so its texts take the file's CRLF form.
    let name = "crlf.txt";
    let text = "needle\r\n".repeat(LINES);
    runtime
        .block_on(tokio::fs::write(tmp.path().join(name), text))
        .expect("write the file");
    let edit = Edit::new(tmp.path());
    let args = serde_json::json!({
        "path": name,
        "edits": [{"old_string": "needle\n", "new_string": "thread\n", "replace_all": true}]
    })
    .to_string();
    let mut edited = None;
    let edit_blocks = blocks(|| {
        let output = runtime.block_on(edit.run(&args, &cancel));
        edited = Some(output);
    });
    let edited = edited.expect("the edit ran");
    assert!(!edited.is_error, "{}", edited.content);
    assert!(
        edited.content.contains("1 of 1 edits applied"),
        "{}",
        edited.content
    );

    // A read of a file whose lines each get a number in the output.
    let name = "lines.txt";
    let mut text = String::with_capacity(LINES * 12);
    for n in 0..LINES {
        text.push_str(&format!("line {n}\n"));
    }
    runtime
        .block_on(tokio::fs::write(tmp.path().join(name), text))
        .expect("write the file");
    let read = Read::new(tmp.path());
    let args = serde_json::json!({ "path": name }).to_string();
    let mut read_output = None;
    let read_blocks = blocks(|| {
        let output = runtime.block_on(read.run(&args, &cancel));
        read_output = Some(output);
    });
    let read_output = read_output.expect("the read ran");
    assert!(!read_output.is_error, "{}", read_output.content);
    assert!(
        read_output.content.starts_with("1\tline 0\n"),
        "{}",
        read_output.content
    );

    eprintln!("blocks: {edit_blocks} for {LINES} edit matches, {read_blocks} for {LINES} lines");
    assert!(
        edit_blocks < EDIT_BLOCKS,
        "{edit_blocks} blocks for {LINES} matches, over {EDIT_BLOCKS}"
    );
    assert!(
        read_blocks < READ_BLOCKS,
        "{read_blocks} blocks for {LINES} lines, over {READ_BLOCKS}"
    );
}

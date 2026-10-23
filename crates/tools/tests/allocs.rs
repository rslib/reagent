//! Allocation counts for the hot paths of `edit` and `read`.
//!
//! Both tools stream a file that can be large, so the work they do must not
//! allocate per match or per line. dhat allows one profiler per process, so
//! every test locks [`SERIAL`] for its whole body: a test that ran beside a
//! profiler would be counted in its numbers.

use std::sync::{Mutex, MutexGuard};

use dhat::{Alloc, HeapStats, Profiler};

use reagent_agent::tool::Tool;
use reagent_tools::bash::Bash;
use reagent_tools::edit::Edit;
use reagent_tools::find::Find;
use reagent_tools::grep::Grep;
use reagent_tools::read::Read;
use reagent_tools::write::Write;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Held by every test for its whole body, since dhat allows only one profiler
/// per process and another test's allocations would land in these numbers.
static SERIAL: Mutex<()> = Mutex::new(());

/// Locks the profiler for one test.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// Files a walk of one subfolder of a repository lists.
const WALK_FILES: usize = 5_000;

/// The most blocks one listed file may allocate, in tenths: 70 before a file
/// also kept a joined `PathBuf` and the ignore chain joined a path per entry,
/// and about 30 now.
const WALK_BLOCKS_PER_FILE_X10: u64 = 35;
const _: () = assert!(WALK_BLOCKS_PER_FILE_X10 < 50);

/// Runs `body` under a fresh profiler and returns the blocks and bytes it
/// allocated.
fn stats(body: impl FnOnce()) -> HeapStats {
    let profiler = Profiler::builder().testing().build();
    body();
    let stats = HeapStats::get();
    drop(profiler);
    stats
}

/// Runs `body` under a fresh profiler and returns the blocks it allocated.
fn blocks(body: impl FnOnce()) -> u64 {
    stats(body).total_blocks
}

#[test]
fn edit_and_read_allocate_per_file_not_per_match() {
    let _serial = serial();
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

/// `find` lists a walk of a subfolder of a repository: the ignore file above
/// the walk root gives the chain a prefix to match, and every listed file keeps
/// its name. One block per file is left for that name; a full `PathBuf` next to
/// it doubles the count.
#[test]
fn perf_walk_find_lists_a_file_per_block() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    runtime.block_on(async {
        tokio::fs::create_dir(tmp.path().join(".git"))
            .await
            .expect("a .git folder");
        tokio::fs::write(tmp.path().join(".gitignore"), "*.log\n")
            .await
            .expect("the ignore file");
        let sub = tmp.path().join("sub");
        tokio::fs::create_dir(&sub).await.expect("the sub folder");
        for n in 0..WALK_FILES {
            tokio::fs::write(sub.join(format!("f{n}.rs")), "")
                .await
                .expect("a file");
        }
    });

    let find = Find::new(tmp.path());
    let cancel = CancellationToken::new();
    let mut found = None;
    let walk_blocks = blocks(|| {
        found = Some(runtime.block_on(find.run(r#"{"pattern":".rs","path":"sub"}"#, &cancel)));
    });
    let found = found.expect("the find ran");
    assert!(!found.is_error, "{}", found.content);
    assert!(
        found
            .content
            .contains(&format!("[{WALK_FILES} files matched")),
        "{}",
        found.content
    );

    let per_file_x10 = walk_blocks * 10 / WALK_FILES as u64;
    eprintln!("blocks: {walk_blocks} for {WALK_FILES} files, {per_file_x10}/10 per file");
    assert!(
        per_file_x10 <= WALK_BLOCKS_PER_FILE_X10,
        "{walk_blocks} blocks for {WALK_FILES} files, {per_file_x10}/10 per file over \
         {WALK_BLOCKS_PER_FILE_X10}/10"
    );
}

/// The most bytes one effect check may allocate: a path and a `Vec` of one.
const EFFECT_BYTES: u64 = 8 * 1024;
/// The text of an effect check is skipped, not decoded: a `write` or an `edit`
/// of a large escaped text must not parse that text a second time.
#[test]
fn effects_skip_the_text_they_do_not_read() {
    let _serial = serial();
    // 100k escaped newlines: 200 KB of JSON that decode to a 100 KB string.
    let text = "\\n".repeat(100_000);
    assert_eq!(text.len(), 200_000);

    let write = Write::new("/work");
    let args = format!(r#"{{"path":"a.txt","content":"{text}"}}"#);
    let mut writes = None;
    let write_stats = stats(|| writes = Some(write.effects(&args)));
    assert_eq!(
        writes.expect("the write ran").writes,
        vec![std::path::PathBuf::from("/work/a.txt")]
    );

    let edit = Edit::new("/work");
    let args = format!(r#"{{"path":"a.txt","old_string":"{text}","new_string":"{text}"}}"#);
    let mut edits = None;
    let edit_stats = stats(|| edits = Some(edit.effects(&args)));
    assert_eq!(
        edits.expect("the edit ran").writes,
        vec![std::path::PathBuf::from("/work/a.txt")]
    );

    eprintln!(
        "bytes: {} for write, {} for edit",
        write_stats.total_bytes, edit_stats.total_bytes
    );
    for (name, bytes) in [
        ("write", write_stats.total_bytes),
        ("edit", edit_stats.total_bytes),
    ] {
        assert!(
            bytes < EFFECT_BYTES,
            "{name} allocated {bytes} bytes for a 200 KB call, over {EFFECT_BYTES}"
        );
    }
}

/// Bytes the capture case pushes through a `bash` call.
const CAPTURE_BYTES: u64 = 8 * 1024 * 1024;

/// The most blocks one 8 MB `bash` call may allocate: 597 measured, all of
/// them around the 128 spill writes and the pipes, none per byte. A copy per
/// chunk on top of that would put the count over this bound.
const CAPTURE_BLOCKS: u64 = 750;
const _: () = assert!(CAPTURE_BLOCKS < 2000);

/// 8 MB through the capture: the head and the tail are kept with one copy per
/// chunk and the spill file is written in whole buffers, so a call that spills
/// 8 MB must not allocate per chunk or per byte.
#[test]
fn perf_capture_eight_megabytes_does_not_allocate_per_chunk() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bash = Bash::new(tmp.path());
    let args = format!(
        r#"{{"command":"head -c {CAPTURE_BYTES} /dev/zero | tr '\\000' x","timeout_ms":60000}}"#
    );
    let cancel = CancellationToken::new();
    let mut output = None;
    let capture_blocks = blocks(|| {
        output = Some(runtime.block_on(bash.run(&args, &cancel)));
    });
    let output = output.expect("the bash call ran");
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains("[output cut: 8388608 bytes"),
        "{}",
        &output.content[..output.content.len().min(200)]
    );

    eprintln!("blocks: {capture_blocks} for {CAPTURE_BYTES} bytes through the capture");
    assert!(
        capture_blocks < CAPTURE_BLOCKS,
        "{capture_blocks} blocks for {CAPTURE_BYTES} bytes through the capture, over \
         {CAPTURE_BLOCKS}"
    );
}

/// Bytes the shown-whole view case prints: under the 16 KiB cut, so nothing
/// is spilled and the text is all the view builds.
const VIEW_BYTES: u64 = 16_000;

/// The most bytes one shown-whole `bash` view may allocate, over the whole
/// call. Most of it is the spawn and the pipes, not the view: with the head
/// clone and the re-owning `from_utf8_lossy(..).into_owned()` this case
/// measured 277,504 bytes, and one buffer 252,928. The bound sits between.
const VIEW_ALLOC_BYTES: u64 = 262_144;
const _: () = assert!(VIEW_ALLOC_BYTES < 277_504);

/// An output under the cut is shown whole: the text comes from one buffer, not
/// from a clone of the head plus a re-owning of the borrowed decode, so the
/// bytes allotted stay near the size of the output.
#[test]
fn perf_bash_view_one_copy() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let bash = Bash::new(tmp.path());
    let args = format!(
        r#"{{"command":"head -c {VIEW_BYTES} /dev/zero | tr '\\000' x","timeout_ms":60000}}"#
    );
    let cancel = CancellationToken::new();
    let mut output = None;
    let view_stats = stats(|| output = Some(runtime.block_on(bash.run(&args, &cancel))));
    let output = output.expect("the bash call ran");
    assert!(!output.is_error, "{}", output.content);
    assert!(
        !output.content.contains("[output cut"),
        "the output was cut"
    );
    assert_eq!(
        output.content.len(),
        VIEW_BYTES as usize + "\nexit code: 0".len()
    );

    eprintln!(
        "bytes: {} for {VIEW_BYTES} bytes shown whole",
        view_stats.total_bytes
    );
    assert!(
        view_stats.total_bytes < VIEW_ALLOC_BYTES,
        "{} bytes for {VIEW_BYTES} bytes shown whole, over {VIEW_ALLOC_BYTES}",
        view_stats.total_bytes
    );
}

/// Files the grep case searches.
const GREP_FILES: usize = 500;

/// The most blocks per searched file, in tenths: measured 4.5 on macOS and 5.4
/// on Linux, where reading a folder allocates more.
const GREP_BLOCKS_PER_FILE_X10: u64 = 65;
const _: () = assert!(GREP_BLOCKS_PER_FILE_X10 < 100);

/// `grep` over 500 small files: each is opened, probed and matched once, so the
/// blocks must scale with the files, not with what they hold.
#[test]
fn perf_grep_searches_a_file_with_few_blocks() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    runtime.block_on(async {
        let sub = tmp.path().join("sub");
        tokio::fs::create_dir(&sub).await.expect("the sub folder");
        for n in 0..GREP_FILES {
            tokio::fs::write(sub.join(format!("f{n}.txt")), "needle\n")
                .await
                .expect("a file");
        }
    });

    let grep = Grep::new(tmp.path());
    let cancel = CancellationToken::new();
    let mut found = None;
    let grep_blocks = blocks(|| {
        found = Some(runtime.block_on(grep.run(r#"{"pattern":"needle","path":"sub"}"#, &cancel)));
    });
    let found = found.expect("the grep ran");
    assert!(!found.is_error, "{}", found.content);
    assert!(
        found.content.contains("sub/f0.txt:1:needle")
            && found
                .content
                .contains(&format!("[{GREP_FILES} lines matched")),
        "{}",
        found.content
    );

    let per_file_x10 = grep_blocks * 10 / GREP_FILES as u64;
    eprintln!("blocks: {grep_blocks} for {GREP_FILES} files, {per_file_x10}/10 per file");
    assert!(
        per_file_x10 <= GREP_BLOCKS_PER_FILE_X10,
        "{grep_blocks} blocks for {GREP_FILES} files, {per_file_x10}/10 per file over \
         {GREP_BLOCKS_PER_FILE_X10}/10"
    );
}

/// `read` calls in the repeated read case.
const READ_CALLS: u64 = 20;

/// The most blocks one `read` call of a small file may allocate, in tenths:
/// 160/10 before the re-read note; now 180/10 on macOS and 183/10 on Linux,
/// whose folder reads allocate more: the path text the note carries and the
/// boxed `Seen`, which keeps every other tool result small. The file's identity
/// comes out of the metadata of the handle the read already had open, so one
/// more block per call, such as a path looked up again, fails here.
const READ_CALL_BLOCKS_X10: u64 = 186;

/// A read reports the lines it showed without costing a block more per call
/// than it did before the note, so no `seen` field, identity or note bookkeeping
/// allocates per read.
#[test]
fn reread_read_call_blocks_do_not_grow() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    runtime
        .block_on(tokio::fs::write(tmp.path().join("f.txt"), "a\nb\n"))
        .expect("write the file");
    let read = Read::new(tmp.path());
    let args = r#"{"path":"f.txt"}"#;
    let cancel = CancellationToken::new();
    // One unmeasured call, so lazy one-time setup is not counted.
    let _ = runtime.block_on(read.run(args, &cancel));

    let mut output = None;
    let calls = blocks(|| {
        for _ in 0..READ_CALLS {
            output = Some(runtime.block_on(read.run(args, &cancel)));
        }
    });
    let output = output.expect("the read ran");
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.seen.is_some(),
        "the measured read reports the lines it showed"
    );

    let per_call_x10 = calls * 10 / READ_CALLS;
    eprintln!("blocks: {calls} for {READ_CALLS} reads, {per_call_x10}/10 per call");
    assert!(
        per_call_x10 <= READ_CALL_BLOCKS_X10,
        "{calls} blocks for {READ_CALLS} reads, {per_call_x10}/10 per call over \
         {READ_CALL_BLOCKS_X10}/10"
    );
}

/// Lines in the file of the echo case.
const ECHO_LINES_OF_FILE: usize = 200;

/// The most lines the echo of one edit prints: the tool's forty line cap.
const ECHO_SHOWN: usize = 40;

/// The most blocks one single-edit replace may allocate: measured 42 for a
/// 200-line file, and a block per echoed line would add forty more, since the
/// echo writes into the result's own buffer.
const ECHO_BLOCKS: u64 = 55;

/// A file of `count` numbered lines.
fn numbered(count: usize) -> String {
    let mut text = String::with_capacity(count * 12);
    for n in 0..count {
        text.push_str(&format!("line {n}\n"));
    }
    text
}

/// The echo of a single edit slices the new text it already holds: one
/// reservation for the reply, and no block for each line it prints or for a
/// copy of the changed text.
#[test]
fn perf_edit_echo_does_not_allocate_per_line() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let text = numbered(ECHO_LINES_OF_FILE);
    runtime
        .block_on(tokio::fs::write(tmp.path().join("f.txt"), &text))
        .expect("write the file");
    // A replacement of 100 lines: the echo prints its 40 line cap, so a block
    // per printed line would show as 40 blocks over the ones the edit needs.
    let mut many = String::new();
    for n in 1..=100 {
        many.push_str(&format!("new {n}\n"));
    }
    let edit = Edit::new(tmp.path());
    let args = serde_json::json!({
        "path": "f.txt",
        "old_string": "line 100\n",
        "new_string": many
    })
    .to_string();
    let cancel = CancellationToken::new();
    let mut output = None;
    let measured = stats(|| output = Some(runtime.block_on(edit.run(&args, &cancel))));
    let output = output.expect("the edit ran");
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output
            .content
            .contains("; now:\n100\tline 99\n101\tnew 1\n"),
        "{}",
        &output.content[output.content.len().saturating_sub(120)..]
    );
    assert_eq!(
        output
            .content
            .lines()
            .filter(|line| line.contains('\t'))
            .count(),
        ECHO_SHOWN
    );
    assert!(output.content.ends_with("...\n"), "{}", output.content);

    eprintln!(
        "blocks: {} and bytes: {} for one echoed edit",
        measured.total_blocks, measured.total_bytes
    );
    assert!(
        measured.total_blocks < ECHO_BLOCKS,
        "{} blocks for one echoed edit, over {ECHO_BLOCKS}",
        measured.total_blocks
    );
}

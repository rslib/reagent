//! Allocation counts for a nested `codemode` call and the size of a result.
//!
//! QuickJS allocates through its own C allocator, so dhat counts only the Rust
//! side: the relay around a nested call and the result it carries. dhat allows
//! one profiler per process, so every test locks [`SERIAL`] for its whole body:
//! a test that ran beside a profiler would be counted in its numbers.

use std::sync::{Mutex, MutexGuard};

use dhat::{Alloc, HeapStats, Profiler};

use reagent_agent::guard::Guard;
use reagent_agent::tool::{Registry, Tool, ToolOutput};
use reagent_config::permissions::Rules;
use reagent_script::codemode::Codemode;
use reagent_tools::read::Read;
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

/// The most bytes one tool result may take: the text's pointer pair, the error
/// flag, and the three boxed extras, which keep every other result small.
const TOOL_OUTPUT_BYTES: usize = 48;

/// A result is a pointer pair, a flag and three options of a box: the `object`
/// a script reads in place of the text took the last pointer, and the images of
/// the result take one more.
#[test]
fn tool_output_grows_by_one_pointer() {
    let _serial = serial();
    let size = std::mem::size_of::<ToolOutput>();
    eprintln!("bytes: {size} for one tool result");
    assert!(
        size <= TOOL_OUTPUT_BYTES,
        "{size} bytes for one tool result, over {TOOL_OUTPUT_BYTES}"
    );
}

/// `read` calls in each measured case.
const READS: u64 = 50;

/// The most blocks one nested `read` call may cost over a direct call: about
/// twice the 9.8 measured (1392 blocks for 50 nested reads against 900 for the
/// same 50 made directly), which is the request, the reply and the waiting
/// promise of each call. A copy of the result per call would put it far higher.
const NESTED_BLOCKS_PER_CALL: u64 = 20;
const _: () = assert!(NESTED_BLOCKS_PER_CALL < 40);

/// A script calls `read` `READS` times in a row, and the blocks one call of
/// that costs over the same call made directly: the relay between the engine
/// and the tools must stay a fixed cost per call, not grow with the result.
#[test]
fn nested_read_blocks_per_call_are_bounded() {
    let _serial = serial();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut text = String::with_capacity(10 * 12);
    for n in 0..10 {
        text.push_str(&format!("line {n}\n"));
    }
    runtime
        .block_on(tokio::fs::write(tmp.path().join("a.txt"), &text))
        .expect("write the file");
    let cancel = CancellationToken::new();

    let read = Read::new(tmp.path());
    let args = r#"{"path":"a.txt"}"#;
    // One unmeasured call, so lazy one-time setup is not counted.
    let _ = runtime.block_on(read.run(args, &cancel));
    let mut direct_output = None;
    let direct = blocks(|| {
        for _ in 0..READS {
            direct_output = Some(runtime.block_on(read.run(args, &cancel)));
        }
    });
    let direct_output = direct_output.expect("the read ran");
    assert!(!direct_output.is_error, "{}", direct_output.content);
    assert!(
        direct_output.content.contains("line 9"),
        "{}",
        direct_output.content
    );

    let mut inner = Registry::new();
    inner.add(Read::new(tmp.path()));
    let codemode = Codemode::new(std::sync::Arc::new(inner))
        .with_guard(Guard::new(Rules::none()))
        .with_wall_time(std::time::Duration::from_secs(600));
    let warmup = serde_json::json!({ "code": "await tools.read({path: \"a.txt\"})" }).to_string();
    // One unmeasured script, so the engine's own setup is not counted.
    let _ = runtime.block_on(codemode.run(&warmup, &cancel));
    let code =
        format!("for (let i = 0; i < {READS}; i++) {{ await tools.read({{path: \"a.txt\"}}); }}");
    let args = serde_json::json!({ "code": code }).to_string();
    let mut nested_output = None;
    let nested = blocks(|| {
        nested_output = Some(runtime.block_on(codemode.run(&args, &cancel)));
    });
    let nested_output = nested_output.expect("the script ran");
    assert!(!nested_output.is_error, "{}", nested_output.content);
    assert_eq!(
        nested_output.content.matches("read ok").count() as u64,
        READS,
        "{}",
        &nested_output.content[..nested_output.content.len().min(200)]
    );

    let per_call = (nested - direct) / READS;
    eprintln!("blocks: {direct} for {READS} direct reads, {nested} for {READS} nested reads");
    assert!(
        per_call < NESTED_BLOCKS_PER_CALL,
        "{nested} blocks for {READS} nested reads over {direct} direct, {per_call} per call, over \
         {NESTED_BLOCKS_PER_CALL}"
    );
}

//! Allocation counts for decoding one worker line into a frame.
//!
//! [`decode`] reads a line with one flat struct: no tag is buffered, and text
//! with no escape borrows from the line. So a line costs no heap block, except
//! the rules list of an approval request, which is the request's own data.
//!
//! dhat allows one profiler per process, so every test locks [`SERIAL`] for its
//! whole body: a test that ran beside a profiler would be counted in its
//! numbers.

use std::borrow::Cow;
use std::sync::{MutexGuard, PoisonError};

use dhat::{HeapStats, Profiler};
use reagent_tui::client::{Frame, decode};

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many times a measurement runs; the smallest count is the work's own.
const SAMPLES: usize = 3;

/// The blocks one line without escapes costs: none.
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

/// One line of every frame kind a turn streams, with params before the method
/// in one of them, since a worker may write the keys in any order.
const LINES: [&str; 11] = [
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"turn_start"}}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"Hello there"}}"#,
    r#"{"jsonrpc":"2.0","params":{"text":"why","type":"thinking_delta"},"method":"event"}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_start","id":"call_1","name":"bash"}}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_1","is_error":false,"bytes":12}}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_1","is_error":false,"bytes":9000,"output":"the last line","earlier":298}}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"usage","input":10,"output":4,"cost":0.01}}"#,
    r#"{"jsonrpc":"2.0","method":"event","params":{"type":"committed","record":7}}"#,
    r#"{"jsonrpc":"2.0","method":"settings","params":{"model":"p/m","context_window":1000,"level":"high","mode":"ask"}}"#,
    r#"{"jsonrpc":"2.0","method":"client_joined","params":{"client":2,"name":"web"}}"#,
    r#"{"jsonrpc":"2.0","method":"approval_resolved","params":{"id":3}}"#,
];

#[test]
fn a_line_without_escapes_decodes_with_no_block() {
    let _serial = serial();
    for line in LINES {
        let measured = blocks(|| {
            let frame = decode(line.as_bytes());
            assert!(
                frame.is_some_and(|frame| !matches!(frame, Frame::Unknown)),
                "{line} did not decode"
            );
        });
        assert_eq!(
            measured, LINE_BLOCKS,
            "{measured} blocks to decode {line}, over {LINE_BLOCKS}"
        );
    }
}

#[test]
fn a_tool_result_borrows_the_tail_of_its_output() {
    let _serial = serial();
    let line = br#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_1","is_error":false,"bytes":9000,"output":"the last line","earlier":298}}"#;
    let Some(Frame::ToolResult {
        output: Some(output),
        earlier,
        ..
    }) = decode(line)
    else {
        panic!("not a tool result");
    };
    assert!(
        matches!(output, Cow::Borrowed("the last line")),
        "the tail was copied or lost: {output:?}"
    );
    assert_eq!(earlier, 298, "the lines the cut took off");
}

#[test]
fn a_delta_borrows_its_text_from_the_line() {
    let _serial = serial();
    let line = br#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"Hello there"}}"#;
    let Some(Frame::TextDelta(text)) = decode(line) else {
        panic!("not a text delta");
    };
    assert!(
        matches!(text, std::borrow::Cow::Borrowed("Hello there")),
        "the text was copied: {text:?}"
    );
}

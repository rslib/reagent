//! Allocation counts for one streamed text delta, per provider.
//!
//! A text delta must own its text, so one block is the floor. Everything the
//! stream glue does per line - splitting the chunk, parsing the line and
//! queueing the event - must add no block of its own, so the total stays near
//! that floor. The bytes go through the real glue with
//! [`reagent_ai::fake::parse_chunks`].
//!
//! dhat allows one profiler at a time, so the three providers run in sequence
//! inside one test.

use bytes::Bytes;
use dhat::{Alloc, HeapStats, Profiler};

use reagent_ai::StreamEvent;
use reagent_ai::fake::{Wire, parse_chunks};

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// How many text deltas the measured body carries.
const DELTAS: usize = 1000;

/// The most blocks one text delta may allocate.
const BLOCKS_PER_DELTA: f64 = 1.2;

/// Splits `body` into one `Bytes` chunk per line, as a socket would.
fn chunks(body: &str) -> Vec<Bytes> {
    body.split_inclusive('\n')
        .map(|line| Bytes::copy_from_slice(line.as_bytes()))
        .collect()
}

fn anthropic_body() -> String {
    "event: content_block_delta\n\
     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n"
        .repeat(DELTAS)
        + "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
}

fn responses_body() -> String {
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"x\"}\n\n"
        .repeat(DELTAS)
        + "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
}

fn chat_body() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n"
        .repeat(DELTAS)
        + "data: [DONE]\n\n"
}

/// The blocks and the text deltas that `chunks` cost through the real stream
/// glue, excluding the building of the chunks themselves.
fn measure(wire: Wire, chunks: Vec<Bytes>) -> (u64, usize) {
    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let items = parse_chunks(wire, chunks);
    let stats = HeapStats::get();
    drop(profiler);
    let mut deltas = 0;
    for item in &items {
        match item {
            Ok(StreamEvent::TextDelta(_)) => deltas += 1,
            Ok(_) => {}
            Err(error) => panic!("{wire:?}: the body parses: {error}"),
        }
    }
    (stats.total_blocks, deltas)
}

#[test]
fn a_text_delta_allocates_at_most_one_point_two_blocks() {
    let anthropic = measure(Wire::Anthropic, chunks(&anthropic_body()));
    let responses = measure(Wire::OpenAiResponses, chunks(&responses_body()));
    let chat = measure(Wire::OpenAiChat, chunks(&chat_body()));

    for (name, (blocks, events)) in [
        ("anthropic", anthropic),
        ("openai-responses", responses),
        ("openai-chat", chat),
    ] {
        assert_eq!(
            events, DELTAS,
            "{name}: the body carries one text delta per line"
        );
        let per_delta = blocks as f64 / DELTAS as f64;
        eprintln!("{name}: {blocks} blocks for {events} deltas = {per_delta:.3} per delta");
        assert!(
            per_delta <= BLOCKS_PER_DELTA,
            "{name}: {blocks} blocks for {events} deltas = {per_delta:.3} per delta, over {BLOCKS_PER_DELTA}"
        );
    }
}

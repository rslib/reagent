//! Allocation counts for one streamed text delta, per provider.
//!
//! A text delta must own its text, so one block is the floor. Everything the
//! stream glue does per line - splitting the chunk, parsing the line and
//! queueing the event - must add no block of its own, so the total stays near
//! that floor. The bytes go through the real glue with
//! [`reagent_ai::fake::parse_chunks`].
//!
//! dhat allows one profiler at a time, so the three providers run in sequence
//! inside one test, and every case takes its own profiler in turn.

use bytes::Bytes;
use dhat::{Alloc, HeapStats, Profiler};

use reagent_ai::StreamEvent;
use reagent_ai::anthropic::request::{
    MessageBody as AnthropicBody, RequestBuilder as AnthropicBuilder,
};
use reagent_ai::fake::{Wire, parse_chunks};
use reagent_ai::openai::chat::{MessageBody as ChatBody, RequestBuilder as ChatBuilder};
use reagent_ai::openai::responses::request::{
    MessageBody as ResponsesBody, RequestBuilder as ResponsesBuilder,
};
use reagent_ai::{Error, MaxTokensField, Message, ReasoningField};

/// dhat counts every thread, so one test at a time measures, with no other
/// test's setup running beside it.
static MEASURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MEASURE.lock().unwrap_or_else(|error| error.into_inner())
}

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// How many text deltas the measured body carries.
const DELTAS: usize = 1000;

/// The most blocks one text delta may allocate.
const BLOCKS_PER_DELTA: f64 = 1.2;

/// The most blocks one escaped text delta may allocate.
///
/// serde_json unescapes into a scratch buffer of its own, one per line, so an
/// escaped delta costs that block and the block of the text it carries: 2.
/// The text used to be unescaped into a `String` and copied into `Arc<str>`,
/// which cost that third block.
const ESCAPED_BLOCKS_PER_DELTA: f64 = 2.3;

/// The most blocks one escaped tool argument delta may allocate on the wires
/// that index the call they belong to.
///
/// The argument is `{"path":"a"}`, whose four escapes make serde_json's scratch
/// unescape buffer grow twice, so the floor is those two blocks and the one
/// block of the text the delta carries: 3. The copied text used to add a fourth.
const TOOL_BLOCKS_PER_DELTA: f64 = 3.3;

/// The most blocks one escaped tool argument delta may allocate on the chat
/// wire, where every line also builds one Vec for its tool call list: 4 before
/// the copied text was dropped.
const CHAT_BLOCKS_PER_DELTA: f64 = 4.3;

/// Splits `body` into one `Bytes` chunk per line, as a socket would.
fn chunks(body: &str) -> Vec<Bytes> {
    body.split_inclusive('\n')
        .map(|line| Bytes::copy_from_slice(line.as_bytes()))
        .collect()
}

/// Splits `body` so that every line arrives in two chunks, as a socket that
/// hands over part of a line does.
fn split_chunks(body: &str) -> Vec<Bytes> {
    let mut out = Vec::new();
    for line in body.split_inclusive('\n') {
        let at = line.len().saturating_sub(1).min(10);
        let (head, tail) = line.split_at(at);
        out.push(Bytes::copy_from_slice(head.as_bytes()));
        out.push(Bytes::copy_from_slice(tail.as_bytes()));
    }
    out
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

/// The same three bodies, each text delta carrying the escaped `a\nb`.
fn escaped_bodies() -> [(&'static str, Wire, String); 3] {
    let anthropic = "event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\\nb\"}}\n\n"
        .repeat(DELTAS)
        + "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let responses = "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"a\\nb\"}\n\n"
        .repeat(DELTAS)
        + "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
    let chat = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\\nb\"},\"finish_reason\":null}]}\n\n"
        .repeat(DELTAS)
        + "data: [DONE]\n\n";
    [
        ("anthropic", Wire::Anthropic, anthropic),
        ("openai-responses", Wire::OpenAiResponses, responses),
        ("openai-chat", Wire::OpenAiChat, chat),
    ]
}

/// One tool call whose every argument delta carries the escaped `{"path":"a"}`.
fn tool_bodies() -> [(&'static str, Wire, String); 3] {
    let anthropic = "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"edit\"}}\n\n"
        .to_owned()
        + &"event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\n"
            .repeat(DELTAS)
        + "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
           event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let responses = "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"edit\"}}\n\n"
        .to_owned()
        + &"data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"a\\\"}\"}\n\n"
            .repeat(DELTAS)
        + "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\"}}\n\n\
           data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
    let chat = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"edit\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n"
        .to_owned()
        + &"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]},\"finish_reason\":null}]}\n\n"
            .repeat(DELTAS)
        + "data: [DONE]\n\n";
    [
        ("anthropic", Wire::Anthropic, anthropic),
        ("openai-responses", Wire::OpenAiResponses, responses),
        ("openai-chat", Wire::OpenAiChat, chat),
    ]
}

/// The blocks and the text deltas that `chunks` cost through the real stream
/// glue, excluding the building of the chunks themselves.
fn measure(wire: Wire, chunks: Vec<Bytes>) -> (u64, usize) {
    let (blocks, items) = run(wire, chunks);
    let mut deltas = 0;
    for item in &items {
        match item {
            Ok(StreamEvent::TextDelta(_)) => deltas += 1,
            Ok(_) => {}
            Err(error) => panic!("{wire:?}: the body parses: {error}"),
        }
    }
    (blocks, deltas)
}

/// The blocks one streamed body costs, and the items it yields.
fn run(wire: Wire, chunks: Vec<Bytes>) -> (u64, Vec<Result<StreamEvent, Error>>) {
    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let items = parse_chunks(wire, chunks);
    let stats = HeapStats::get();
    drop(profiler);
    (stats.total_blocks, items)
}

/// The blocks one piece of work allocates, with its result kept alive until
/// after the counting.
fn blocks_of<T>(work: impl FnOnce() -> T) -> u64 {
    allocated_of(work).0
}

/// The blocks and the bytes one piece of work allocates, with its result kept
/// alive until after the counting.
fn allocated_of<T>(work: impl FnOnce() -> T) -> (u64, u64) {
    let profiler = Profiler::builder().testing().build();
    let value = work();
    let stats = HeapStats::get();
    drop(value);
    drop(profiler);
    (stats.total_blocks, stats.total_bytes)
}

#[test]
fn a_text_delta_allocates_at_most_one_point_two_blocks() {
    let _guard = lock();
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

/// An escaped text delta unescapes straight into the text it carries, so it
/// costs the one block of that text and no second copy.
#[test]
fn an_escaped_text_delta_allocates_at_most_two_point_three_blocks() {
    let _guard = lock();
    for (name, wire, body) in escaped_bodies() {
        let (blocks, items) = run(wire, chunks(&body));
        let mut deltas = 0;
        for item in &items {
            match item {
                Ok(StreamEvent::TextDelta(text)) => {
                    assert_eq!(&**text, "a\nb", "{name}: the delta is unescaped");
                    deltas += 1;
                }
                Ok(_) => {}
                Err(error) => panic!("{name}: the body parses: {error}"),
            }
        }
        assert_eq!(deltas, DELTAS, "{name}: one text delta per line");
        let per_delta = blocks as f64 / DELTAS as f64;
        eprintln!("{name} escaped: {blocks} blocks for {deltas} deltas = {per_delta:.3}");
        assert!(
            per_delta <= ESCAPED_BLOCKS_PER_DELTA,
            "{name}: {blocks} blocks for {deltas} escaped deltas = {per_delta:.3} per delta, over {ESCAPED_BLOCKS_PER_DELTA}"
        );
    }
}

/// An escaped tool argument delta costs one block on the wires that index the
/// call they belong to, and one more on the chat wire, whose every line builds
/// a `Vec` for its tool call list.
#[test]
fn an_escaped_tool_argument_delta_allocates_at_most_its_floor() {
    let _guard = lock();
    for (name, wire, body) in tool_bodies() {
        let (blocks, items) = run(wire, chunks(&body));
        let mut deltas = 0;
        for item in &items {
            match item {
                Ok(StreamEvent::ToolCallDelta { partial_json, .. }) => {
                    assert_eq!(&**partial_json, r#"{"path":"a"}"#, "{name}: the delta");
                    deltas += 1;
                }
                Ok(_) => {}
                Err(error) => panic!("{name}: the body parses: {error}"),
            }
        }
        assert_eq!(deltas, DELTAS, "{name}: one argument delta per line");
        let limit = match wire {
            Wire::OpenAiChat => CHAT_BLOCKS_PER_DELTA,
            _ => TOOL_BLOCKS_PER_DELTA,
        };
        let per_delta = blocks as f64 / DELTAS as f64;
        eprintln!("{name} tool: {blocks} blocks for {deltas} deltas = {per_delta:.3}");
        assert!(
            per_delta <= limit,
            "{name}: {blocks} blocks for {deltas} argument deltas = {per_delta:.3} per delta, over {limit}"
        );
    }
}

/// A line split across two reads is joined in buffers that keep their
/// capacity, so it costs no more than a line that arrived whole.
#[test]
fn a_line_split_across_reads_adds_no_blocks() {
    let _guard = lock();
    let bodies = [
        ("anthropic", Wire::Anthropic, anthropic_body()),
        ("openai-responses", Wire::OpenAiResponses, responses_body()),
        ("openai-chat", Wire::OpenAiChat, chat_body()),
    ];
    for (name, wire, body) in bodies {
        let (whole, _) = run(wire, chunks(&body));
        let (split, items) = run(wire, split_chunks(&body));
        let mut deltas = 0;
        for item in &items {
            match item {
                Ok(StreamEvent::TextDelta(_)) => deltas += 1,
                Ok(_) => {}
                Err(error) => panic!("{name}: the split body parses: {error}"),
            }
        }
        assert_eq!(deltas, DELTAS, "{name}: one text delta per line");
        let limit = whole as f64 + 0.2 * DELTAS as f64;
        eprintln!("{name} split: {split} blocks against {whole} whole");
        assert!(
            split as f64 <= limit,
            "{name}: {split} blocks for split lines against {whole} whole, over {limit:.1}"
        );
    }
}

/// Every reasoning detail piece is parsed once, so it costs the one block of
/// its raw value and the growth of the text it folds into.
#[test]
fn a_reasoning_detail_piece_allocates_at_most_two_and_a_half_blocks() {
    let _guard = lock();
    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"x\",\"index\":0}]},\"finish_reason\":null}]}\n\n"
        .repeat(DELTAS)
        + "data: [DONE]\n\n";
    let (blocks, items) = run(Wire::OpenAiChat, chunks(&body));
    let mut signature = None;
    for item in &items {
        match item {
            Ok(StreamEvent::ThinkingSignature(text)) => signature = Some(text.clone()),
            Ok(_) => {}
            Err(error) => panic!("the body parses: {error}"),
        }
    }
    let signature = signature.expect("the merged details leave as one signature");
    let text = "x".repeat(DELTAS);
    assert_eq!(
        &*signature,
        format!(r#"[{{"index":0,"text":"{text}","type":"reasoning.text"}}]"#),
        "the pieces fold into one item"
    );
    let per_piece = blocks as f64 / DELTAS as f64;
    eprintln!("reasoning details: {blocks} blocks for {DELTAS} pieces = {per_piece:.3}");
    assert!(
        per_piece <= 2.5,
        "{blocks} blocks for {DELTAS} reasoning detail pieces = {per_piece:.3} per piece, over 2.5"
    );
}

/// One wire's builder and the cached bodies of a 1 MB history, so the two
/// body tests measure the same thing.
enum History {
    Anthropic(AnthropicBuilder, Vec<AnthropicBody>),
    Chat(ChatBuilder, Vec<ChatBody>),
    Responses(ResponsesBuilder, Vec<ResponsesBody>),
}

impl History {
    fn name(&self) -> &'static str {
        match self {
            History::Anthropic(..) => "anthropic",
            History::Chat(..) => "openai-chat",
            History::Responses(..) => "openai-responses",
        }
    }

    /// The blocks the body as it leaves costs: the list of its pieces, with no
    /// message copied.
    fn blocks(&self) -> u64 {
        match self {
            History::Anthropic(builder, bodies) => blocks_of(|| builder.body_parts(bodies)),
            History::Chat(builder, bodies) => blocks_of(|| builder.body_parts(bodies)),
            History::Responses(builder, bodies) => blocks_of(|| builder.body_parts(bodies)),
        }
    }

    /// The blocks and the bytes the body as it leaves costs.
    fn parts(&self) -> (u64, u64) {
        match self {
            History::Anthropic(builder, bodies) => allocated_of(|| builder.body_parts(bodies)),
            History::Chat(builder, bodies) => allocated_of(|| builder.body_parts(bodies)),
            History::Responses(builder, bodies) => allocated_of(|| builder.body_parts(bodies)),
        }
    }

    /// The length of the body, measured outside the counted work.
    fn body_len(&self) -> usize {
        match self {
            History::Anthropic(builder, bodies) => builder.body_parts(bodies).len(),
            History::Chat(builder, bodies) => builder.body_parts(bodies).len(),
            History::Responses(builder, bodies) => builder.body_parts(bodies).len(),
        }
    }

    /// The most blocks the list of pieces may cost: the pieces themselves, and
    /// on the Anthropic wire the list of the messages that stay visible.
    fn limit(&self) -> u64 {
        match self {
            History::Anthropic(..) => 2,
            History::Chat(..) | History::Responses(..) => 1,
        }
    }
}

/// The cached bodies of `history` for one wire.
fn bodies<T>(history: &[Message], build: impl Fn(&Message) -> Result<T, Error>) -> Vec<T> {
    history
        .iter()
        .map(|message| build(message).expect("a tool result serializes"))
        .collect()
}

/// The three wires over one history of 50 tool results of 20 KB.
fn histories() -> Vec<History> {
    let provider = "test";
    let history: Vec<Message> = (0..50)
        .map(|index| Message::tool_result(format!("call_{index}"), "x".repeat(20_000), false))
        .collect();
    let tools = [];
    vec![
        History::Anthropic(
            AnthropicBuilder::new("m", 64, None, None, &tools),
            bodies(&history, |message| AnthropicBody::new(message, provider)),
        ),
        History::Chat(
            ChatBuilder::new(
                "m",
                64,
                None,
                None,
                &tools,
                MaxTokensField::MaxTokens,
                ReasoningField::Effort,
            ),
            bodies(&history, |message| ChatBody::new(message, provider)),
        ),
        History::Responses(
            ResponsesBuilder::new("m", 64, None, None, None, &tools),
            bodies(&history, |message| ResponsesBody::new(message, provider)),
        ),
    ]
}

/// The body of a request is the list of the pieces it sends, so a history of
/// large tool results costs the list of them and no buffer of the history.
///
/// The old body wrote every message into one buffer sized from the history: 2
/// blocks, the buffer and the list of visible messages. The pieces are that
/// list and the pieces themselves: 1 block, and 2 on the Anthropic wire.
#[test]
fn a_large_history_body_is_one_allocation() {
    let _guard = lock();
    for history in histories() {
        let blocks = history.blocks();
        let limit = history.limit();
        eprintln!(
            "{} pieces: {blocks} blocks for a {} byte body",
            history.name(),
            history.body_len()
        );
        assert!(
            blocks <= limit,
            "{}: the pieces of a 1 MB history took {blocks} allocations, over {limit}",
            history.name()
        );
    }
}

/// The pieces of a large body share the bytes of the cached messages, so a 1 MB
/// history allocates a few kilobytes: the list of its pieces and nothing per
/// message. The old body copied every message into one 1 MB buffer.
#[test]
fn body_pieces_of_a_large_history_hold_no_copy() {
    let _guard = lock();
    for history in histories() {
        let body = history.body_len() as u64;
        let (blocks, bytes) = history.parts();
        eprintln!(
            "{} pieces: {blocks} blocks, {bytes} bytes for a {body} byte body",
            history.name()
        );
        assert!(
            bytes < body / 100,
            "{}: {bytes} bytes for a {body} byte body is a copy of the messages",
            history.name()
        );
    }
}

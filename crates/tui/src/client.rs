//! The connection to a session worker over its socket.
//!
//! A client sends one line per request with one `write_all` from a buffer it
//! reuses, and reads the worker's frames with [`Lines`], decoding each one
//! borrowed from the line: an event carries `Cow::Borrowed` text, so a streamed
//! delta costs no copy and no allocation here. A method or an event type this
//! build does not know is skipped, never an error, so a newer worker still
//! works.

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use reagent_protocol::lines::Lines;
use reagent_protocol::message::{AnswerKind, MAJOR, Mode};
use serde::Deserialize;
use serde::de::{self, Visitor};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// The worker's answer to `initialize`: who this client is and where the
/// session stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Initialized {
    /// The id of the session.
    pub session: String,
    /// The id the worker gave this connection.
    pub client: u64,
    /// The newest journal record the worker committed.
    pub committed: u64,
    /// The provider and model text the run uses. A worker older than the field
    /// names none.
    pub model: Option<Arc<str>>,
    /// The context window of the model, when the run knows it.
    pub context_window: Option<u64>,
    /// The reasoning level the session sends, when it sends one.
    pub level: Option<Arc<str>>,
}

/// The worker's answer to `attach`: where the session stands, and whether a run
/// is open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attached {
    /// The newest journal record the worker committed when it attached.
    pub committed: u64,
    /// Whether a run is open. The `run_end` of one that ended before
    /// `committed` is in no frame the worker replays, so this is what tells a
    /// client that reattached that the run is over.
    pub running: bool,
}

/// One thing the worker said, in the shape the app draws.
///
/// Text borrows from the line the frame came in, so it lives as long as the
/// borrow of the [`Client`] that read it.
#[derive(Debug)]
pub enum Frame<'a> {
    /// A provider call started.
    TurnStart,
    /// A piece of the reply's visible text.
    TextDelta(Cow<'a, str>),
    /// A piece of the model's private reasoning.
    ThinkingDelta(Cow<'a, str>),
    /// A tool call started.
    ToolStart {
        /// The call id.
        id: Cow<'a, str>,
        /// The tool name.
        name: Cow<'a, str>,
    },
    /// A tool call's arguments are complete.
    ToolCallReady {
        /// The call id.
        id: Cow<'a, str>,
        /// The tool name.
        name: Cow<'a, str>,
        /// The raw JSON arguments.
        arguments: Cow<'a, str>,
    },
    /// A tool call produced its result.
    ToolResult {
        /// The call id.
        id: Cow<'a, str>,
        /// Whether the tool reported a failure.
        failed: bool,
        /// The UTF-8 length of the output the model receives.
        bytes: usize,
        /// The output text of a failed call.
        error: Option<Cow<'a, str>>,
        /// The last bytes of the output, from the start of a line, so the call
        /// shows it without reading the journal. `None` from a worker that
        /// sends none.
        output: Option<Cow<'a, str>>,
        /// The count of lines before the tail, which the frame leaves out.
        earlier: u64,
        /// How many images the result produced, whose bytes the journal holds.
        images: usize,
    },
    /// A provider call ended.
    TurnEnd,
    /// The whole run ended.
    RunEnd {
        /// Whether the turn limit ended it, so the task may not be done.
        at_limit: bool,
    },
    /// A client sent a message; every attached client hears it.
    UserMessage {
        /// The text of the message.
        text: Cow<'a, str>,
        /// How many images follow it, such as a pasted one.
        images: usize,
    },
    /// A provider or tool error.
    Error(Cow<'a, str>),
    /// A failed provider call is being retried.
    Retry {
        /// The retry number, from 1.
        attempt: u32,
        /// The error that caused it.
        error: Cow<'a, str>,
    },
    /// A journal record was committed.
    Committed(u64),
    /// A compaction of the history: the prompt tokens before and after it, and
    /// how many messages the checkpoint replaced.
    Compacted {
        /// The prompt size in tokens before it.
        before: u64,
        /// The prompt size in tokens after it.
        after: u64,
        /// The messages the checkpoint replaced.
        replaced: u32,
    },
    /// The settings of the session: the model it took after a change, the
    /// window of that model, the reasoning level and the permission mode.
    Settings {
        /// The provider and model text, as `initialize` names it.
        model: Cow<'a, str>,
        /// The context window of the model, when it is known.
        context_window: Option<u64>,
        /// The reasoning level the session sends, when it sends one.
        level: Option<Cow<'a, str>>,
        /// The permission mode the session decides with.
        mode: Option<Cow<'a, str>>,
    },
    /// The tokens the last request used.
    Usage {
        /// The input tokens the request sent.
        input: u64,
        /// The tokens the request read from the provider's cache and wrote to
        /// it, added together.
        cached: u64,
        /// The output tokens the reply made.
        output: u64,
        /// What the provider says the request cost in USD, when it says.
        cost: Option<f64>,
    },
    /// A tool call waits for an answer: the card, and the rules it would save.
    ApprovalRequest {
        /// What the card shows.
        request: Approval<'a>,
        /// The rules an `always` answer saves: an empty list offers no save.
        saves: Vec<Cow<'a, str>>,
    },
    /// An approval request got its answer, from this client or another.
    ApprovalResolved {
        /// The id of the request.
        id: u64,
    },
    /// The `ask` tool waits for answers: what the card shows.
    QuestionRequest {
        /// The id the answer names.
        id: u64,
        /// The questions, in the order the model asked them.
        questions: Vec<Question<'a>>,
    },
    /// A question request got its answer, from this client or another.
    QuestionResolved {
        /// The id of the request.
        id: u64,
    },
    /// Another client joined the session.
    ClientJoined {
        /// The id the worker gave the client.
        id: u64,
        /// The name the client chose, which the Clients panel shows.
        name: Cow<'a, str>,
    },
    /// A client left the session.
    ClientLeft {
        /// The id of the client that left: the notice names no name.
        id: u64,
    },
    /// The worker cut this client off: the reason it named.
    Closing(Cow<'a, str>),
    /// A method or an event type this build does not know, or an answer to a
    /// request: nothing to draw.
    Unknown,
}

/// One question an `ask` call puts to the person.
#[derive(Debug, Deserialize)]
pub struct Question<'a> {
    /// The question text.
    #[serde(borrow)]
    pub q: Cow<'a, str>,
    /// The answers offered, in the order the card numbers them.
    #[serde(borrow, default)]
    pub options: Vec<QuestionOption<'a>>,
    /// Whether several options may be checked.
    #[serde(default)]
    pub multi: bool,
}

/// One answer a question offers.
#[derive(Debug, Deserialize)]
pub struct QuestionOption<'a> {
    /// The label an answer reads back.
    #[serde(borrow)]
    pub label: Cow<'a, str>,
    /// The short note the card draws after the label, when it has one.
    #[serde(default, borrow, deserialize_with = "cow")]
    pub note: Option<Cow<'a, str>>,
}

/// What an approval request shows the user.
#[derive(Debug)]
pub struct Approval<'a> {
    /// The id the answer names.
    pub id: u64,
    /// The tool the call names.
    pub tool: Cow<'a, str>,
    /// One line that shows the call.
    pub call: Cow<'a, str>,
    /// The rule that asked.
    pub rule: Cow<'a, str>,
    /// Whether an `always` answer is offered: the rules it saves ride with the
    /// frame, so the card can name them.
    pub save: bool,
}

/// The connection to one worker, and the buffer its frames are read into.
#[derive(Debug)]
pub struct Client {
    write: OwnedWriteHalf,
    reader: Reader,
    /// The line of the request being written, reused.
    out: Vec<u8>,
    /// The id of the next request.
    next: u64,
    /// The lines that arrived before the answer they followed, oldest first.
    ///
    /// The worker queues a client's catch-up frames before the answer to its
    /// `attach`, in one frame: they are held here and drawn after the handshake,
    /// in order, instead of being dropped with the lines the handshake skips.
    replay: Vec<Vec<u8>>,
    /// How many of them [`Client::next_frame`] served.
    at: usize,
}

impl Client {
    /// Connects to `socket` and initializes with `token` and `name`.
    ///
    /// The answer names the session, this client and the newest committed
    /// record; the caller reads its own copy of the journal up to that record
    /// and then sends [`Client::attach`].
    pub async fn connect(
        socket: impl AsRef<Path>,
        token: &str,
        name: &str,
    ) -> io::Result<(Client, Initialized)> {
        let stream = UnixStream::connect(socket).await?;
        let (read, write) = stream.into_split();
        let mut client = Client {
            write,
            reader: Reader::new(read),
            out: Vec::with_capacity(256),
            next: 0,
            replay: Vec::new(),
            at: 0,
        };
        let init = client.initialize(token, name).await?;
        Ok((client, init))
    }

    /// Sends `initialize` and reads its answer.
    async fn initialize(&mut self, token: &str, name: &str) -> io::Result<Initialized> {
        let id = self.id();
        begin(&mut self.out, id, "initialize");
        self.out.extend_from_slice(b"{\"major\":");
        push_u64(&mut self.out, u64::from(MAJOR));
        self.out.extend_from_slice(b",\"client\":{\"name\":");
        push_str(&mut self.out, name);
        self.out.extend_from_slice(b",\"version\":");
        push_str(&mut self.out, env!("CARGO_PKG_VERSION"));
        self.out
            .extend_from_slice(b"},\"capabilities\":[],\"token\":");
        push_str(&mut self.out, token);
        self.out.extend_from_slice(b"}}\n");
        self.flush().await?;
        loop {
            if let Some(line) = self.reader.line().await? {
                if let Some(init) = read_init(line, id)? {
                    return Ok(init);
                }
                self.replay.push(line.to_vec());
            } else if self.reader.closed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the worker closed before its answer",
                ));
            }
        }
    }

    /// Attaches at `from`, the record the journal was read up to, and answers
    /// the record the worker had committed when it attached and whether a run
    /// is open.
    ///
    /// The worker replays the frames of the records it had committed since its
    /// last commit, so the caller reads the journal up to the record in the
    /// answer and loses nothing between the handshake and this call. Those
    /// frames arrive before the answer and are held for [`Client::next_frame`].
    pub async fn attach(&mut self, from: u64) -> io::Result<Attached> {
        let id = self.id();
        begin(&mut self.out, id, "attach");
        self.out.extend_from_slice(b"{\"from\":");
        push_u64(&mut self.out, from);
        self.out.extend_from_slice(b"}}\n");
        self.flush().await?;
        loop {
            if let Some(line) = self.reader.line().await? {
                if let Some(attached) = read_attached(line, id)? {
                    return Ok(attached);
                }
                self.replay.push(line.to_vec());
            } else if self.reader.closed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the worker closed before its answer",
                ));
            }
        }
    }

    /// Sends a message: it steers the run, or waits to start the next one.
    pub async fn send(&mut self, text: &str, mode: Mode) -> io::Result<()> {
        let id = self.id();
        begin(&mut self.out, id, "send");
        self.out.extend_from_slice(b"{\"text\":");
        push_str(&mut self.out, text);
        let mode: &[u8] = match mode {
            Mode::Steer => b",\"mode\":\"steer\"}}",
            Mode::Queue => b",\"mode\":\"queue\"}}",
        };
        self.out.extend_from_slice(mode);
        self.out.push(b'\n');
        self.flush().await
    }

    /// Asks the session to compact its history into a checkpoint, with `note`
    /// kept in the checkpoint when one is given.
    pub async fn compact(&mut self, note: Option<&str>) -> io::Result<()> {
        let id = self.id();
        begin(&mut self.out, id, "compact");
        match note {
            Some(note) => {
                self.out.extend_from_slice(b"{\"note\":");
                push_str(&mut self.out, note);
                self.out.push(b'}');
            }
            None => self.out.extend_from_slice(b"{}"),
        }
        self.out.push(b'\n');
        self.flush().await
    }

    /// Sets the model, the reasoning level or the permission mode of the
    /// session; `None` leaves that field as it is.
    pub async fn set(
        &mut self,
        model: Option<&str>,
        level: Option<&str>,
        mode: Option<&str>,
    ) -> io::Result<()> {
        let id = self.id();
        begin(&mut self.out, id, "set");
        self.out.push(b'{');
        let mut first = true;
        for (name, value) in [("model", model), ("level", level), ("mode", mode)] {
            let Some(value) = value else {
                continue;
            };
            if !first {
                self.out.push(b',');
            }
            first = false;
            self.out.push(b'"');
            self.out.extend_from_slice(name.as_bytes());
            self.out.extend_from_slice(b"\":");
            push_str(&mut self.out, value);
        }
        self.out.extend_from_slice(b"}}\n");
        self.flush().await
    }

    /// Asks the session to stop the current turn.
    pub async fn cancel(&mut self) -> io::Result<()> {
        let id = self.id();
        begin(&mut self.out, id, "cancel");
        self.out.extend_from_slice(b"{}}\n");
        self.flush().await
    }

    /// Answers the approval request `id`.
    pub async fn approve(
        &mut self,
        id: u64,
        answer: AnswerKind,
        reason: Option<&str>,
    ) -> io::Result<()> {
        let request = self.id();
        begin(&mut self.out, request, "approve");
        self.out.extend_from_slice(b"{\"id\":");
        push_u64(&mut self.out, id);
        self.out.extend_from_slice(b",\"answer\":\"");
        self.out.extend_from_slice(answer_name(answer).as_bytes());
        self.out.push(b'"');
        if let Some(reason) = reason {
            self.out.extend_from_slice(b",\"reason\":");
            push_str(&mut self.out, reason);
        }
        self.out.extend_from_slice(b"}}\n");
        self.flush().await
    }

    /// Answers the question request `id`: one list of picks or own text per
    /// question, in the order they were asked, or `None` to skip them all.
    pub async fn answer(&mut self, id: u64, answers: Option<&[Vec<String>]>) -> io::Result<()> {
        let request = self.id();
        begin(&mut self.out, request, "answer");
        self.out.extend_from_slice(b"{\"id\":");
        push_u64(&mut self.out, id);
        self.out.extend_from_slice(b",\"answers\":");
        match answers {
            None => self.out.extend_from_slice(b"null"),
            Some(lists) => {
                self.out.push(b'[');
                for (at, list) in lists.iter().enumerate() {
                    if at > 0 {
                        self.out.push(b',');
                    }
                    self.out.push(b'[');
                    for (index, label) in list.iter().enumerate() {
                        if index > 0 {
                            self.out.push(b',');
                        }
                        push_str(&mut self.out, label);
                    }
                    self.out.push(b']');
                }
                self.out.push(b']');
            }
        }
        self.out.extend_from_slice(b"}}\n");
        self.flush().await
    }

    /// Leaves the session; the worker keeps running for its other clients.
    pub async fn detach(&mut self) -> io::Result<()> {
        let id = self.id();
        begin(&mut self.out, id, "detach");
        self.out.extend_from_slice(b"{}}\n");
        self.flush().await
    }

    /// The next frame of the worker: the lines that arrived before an answer
    /// first, then the lines still to read.
    pub async fn next_frame(&mut self) -> io::Result<Next<'_>> {
        let at = self.at;
        if at < self.replay.len() {
            self.at = at + 1;
            let line: &[u8] = &self.replay[at];
            return Ok(Next::Frame(decode(line).unwrap_or(Frame::Unknown)));
        }
        if at > 0 {
            self.replay.clear();
            self.at = 0;
        }
        self.reader.next().await
    }

    /// The id of the next request.
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Writes the line in the buffer as one `write_all`, then clears it.
    async fn flush(&mut self) -> io::Result<()> {
        let written = self.write.write_all(&self.out).await;
        self.out.clear();
        written
    }
}

/// The answer name of an answer kind, as the protocol spells it.
fn answer_name(answer: AnswerKind) -> &'static str {
    match answer {
        AnswerKind::Once => "once",
        AnswerKind::Session => "session",
        AnswerKind::Save => "save",
        AnswerKind::Refuse => "refuse",
        AnswerKind::Skip => "skip",
    }
}

/// The next thing the reader has: a frame, or the state of the buffer.
#[derive(Debug)]
pub enum Next<'a> {
    /// One line of the worker, decoded.
    Frame(Frame<'a>),
    /// The line is not complete yet; the reader is asked again.
    More,
    /// The worker closed the connection.
    End,
}

/// The reading side of one connection: one reused buffer of lines.
#[derive(Debug)]
struct Reader {
    read: OwnedReadHalf,
    lines: Lines,
    /// Whether the buffer holds no complete line yet.
    missing: bool,
    /// Whether the worker closed the connection.
    closed: bool,
}

impl Reader {
    fn new(read: OwnedReadHalf) -> Self {
        Reader {
            read,
            lines: Lines::new(),
            missing: true,
            closed: false,
        }
    }

    /// The next frame, or why there is none.
    ///
    /// A line this build does not know comes back as [`Frame::Unknown`], which
    /// the app drops: skipping it here would fold a loop into the one borrow
    /// that leaves this call, which the borrow checker does not allow.
    async fn next(&mut self) -> io::Result<Next<'_>> {
        // Read before the call, since the call takes the mutable borrow the
        // match holds; a reader that closes during it answers `More` once and
        // `End` on the next call.
        let closed = self.closed;
        match self.line().await? {
            Some(line) => Ok(Next::Frame(decode(line).unwrap_or(Frame::Unknown))),
            None if closed => Ok(Next::End),
            None => Ok(Next::More),
        }
    }

    /// The next line, whatever it carries.
    async fn line(&mut self) -> io::Result<Option<&[u8]>> {
        if self.missing || self.lines.is_empty() {
            if self.lines.fill(&mut self.read).await? == 0 {
                self.closed = true;
                return Ok(None);
            }
            self.missing = false;
        }
        match self.lines.next() {
            Some(Ok(line)) => Ok(Some(line)),
            Some(Err(too_long)) => Err(io::Error::other(too_long.to_string())),
            None => {
                self.missing = true;
                Ok(None)
            }
        }
    }
}

/// The answer to `initialize`, which borrows from its line.
fn read_init(line: &[u8], id: u64) -> io::Result<Option<Initialized>> {
    #[derive(Deserialize)]
    #[serde(bound(deserialize = "'de: 'a"))]
    struct Answer<'a> {
        id: Option<u64>,
        result: Option<Result<'a>>,
    }
    #[derive(Deserialize)]
    #[serde(bound(deserialize = "'de: 'a"))]
    struct Result<'a> {
        #[serde(borrow)]
        session: Cow<'a, str>,
        client: u64,
        committed: u64,
        /// Absent from the answer of a worker older than the field.
        #[serde(borrow, default)]
        model: Option<Cow<'a, str>>,
        #[serde(default)]
        context_window: Option<u64>,
        #[serde(borrow, default)]
        level: Option<Cow<'a, str>>,
    }
    let Ok(answer) = serde_json::from_slice::<Answer<'_>>(line) else {
        return Ok(None);
    };
    if answer.id != Some(id) {
        return Ok(None);
    }
    match answer.result {
        Some(result) => Ok(Some(Initialized {
            session: result.session.into_owned(),
            client: result.client,
            committed: result.committed,
            model: result.model.as_deref().map(Arc::from),
            context_window: result.context_window,
            level: result.level.as_deref().map(Arc::from),
        })),
        None => Err(io::Error::other("the worker refused the handshake")),
    }
}

/// The answer to `attach`, which borrows from its line.
fn read_attached(line: &[u8], id: u64) -> io::Result<Option<Attached>> {
    #[derive(Deserialize)]
    struct Answer {
        id: Option<u64>,
        result: Option<Result>,
    }
    #[derive(Deserialize)]
    struct Result {
        committed: u64,
        /// A worker that does not say is one with no run open.
        #[serde(default)]
        running: bool,
    }
    let Ok(answer) = serde_json::from_slice::<Answer>(line) else {
        return Ok(None);
    };
    if answer.id != Some(id) {
        return Ok(None);
    }
    match answer.result {
        Some(result) => Ok(Some(Attached {
            committed: result.committed,
            running: result.running,
        })),
        None => Err(io::Error::other("the worker refused the attach")),
    }
}

/// A frame this build knows, or `None` for one it skips.
pub fn decode(line: &[u8]) -> Option<Frame<'_>> {
    let Ok(line) = serde_json::from_slice::<Line<'_>>(line) else {
        return None;
    };
    let Line { method, params } = line;
    let Params {
        kind,
        text,
        id,
        name,
        arguments,
        is_error,
        bytes,
        error,
        message,
        attempt,
        record,
        before,
        after,
        replaced,
        input,
        cache_read,
        cache_write,
        output,
        earlier,
        images,
        cost,
        tool,
        call,
        rule,
        save,
        reason,
        client,
        model,
        context_window,
        level,
        mode,
        questions,
    } = params?;
    Some(match (&*method, kind.as_deref().unwrap_or_default()) {
        ("event", "turn_start") => Frame::TurnStart,
        ("event", "text_delta") => Frame::TextDelta(text?),
        ("event", "thinking_delta") => Frame::ThinkingDelta(text?),
        ("event", "tool_start") => Frame::ToolStart {
            id: id?.text()?,
            name: name?,
        },
        ("event", "tool_call_ready") => Frame::ToolCallReady {
            id: id?.text()?,
            name: name?,
            arguments: arguments?,
        },
        ("event", "tool_result") => Frame::ToolResult {
            id: id?.text()?,
            failed: is_error,
            bytes,
            error,
            output: output.and_then(Output::text),
            earlier,
            images: images.len(),
        },
        ("event", "turn_end") => Frame::TurnEnd,
        ("event", "run_end") => Frame::RunEnd {
            at_limit: reason.as_deref() == Some("max_turns"),
        },
        ("event", "user_message") => Frame::UserMessage {
            text: text?,
            images: images.len(),
        },
        ("event", "error") => Frame::Error(message?),
        ("event", "retry") => Frame::Retry {
            attempt,
            error: error?,
        },
        ("event", "committed") => Frame::Committed(record),
        ("event", "compacted") => Frame::Compacted {
            before,
            after,
            replaced,
        },
        ("event", "usage") => Frame::Usage {
            input,
            cached: cache_read.saturating_add(cache_write),
            output: output.map(Output::count).unwrap_or(0),
            cost,
        },
        ("approval_request", _) => Frame::ApprovalRequest {
            request: Approval {
                id: id?.num()?,
                tool: tool?,
                call: call?,
                rule: rule?,
                save: !save.is_empty(),
            },
            saves: save,
        },
        ("approval_resolved", _) => Frame::ApprovalResolved { id: id?.num()? },
        ("question_request", _) => Frame::QuestionRequest {
            id: id?.num()?,
            questions,
        },
        ("question_resolved", _) => Frame::QuestionResolved { id: id?.num()? },
        ("closing", _) => Frame::Closing(reason?),
        ("settings", _) => Frame::Settings {
            model: model.unwrap_or_default(),
            context_window,
            level,
            mode,
        },
        ("client_joined", _) => Frame::ClientJoined {
            id: client?,
            name: name.unwrap_or_default(),
        },
        ("client_left", _) => Frame::ClientLeft { id: client? },
        _ => return None,
    })
}

/// One worker line: the method and the params of every method, flat so no tag
/// is buffered.
#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow, default)]
    method: Cow<'a, str>,
    #[serde(borrow, default)]
    params: Option<Params<'a>>,
}

/// The params of every method and event type, each optional so a line without
/// it reads as absent.
#[derive(Deserialize)]
struct Params<'a> {
    #[serde(rename = "type", default, borrow, deserialize_with = "cow")]
    kind: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    id: Option<Id<'a>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    arguments: Option<Cow<'a, str>>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    bytes: usize,
    /// A string in `retry`, and an optional string in `tool_result`.
    #[serde(default, borrow, deserialize_with = "cow")]
    error: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    message: Option<Cow<'a, str>>,
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    record: u64,
    #[serde(default)]
    before: u64,
    #[serde(default)]
    after: u64,
    #[serde(default)]
    replaced: u32,
    #[serde(default)]
    input: u64,
    /// The tokens a `usage` frame read from the provider's cache.
    #[serde(default)]
    cache_read: u64,
    /// The tokens a `usage` frame wrote to the provider's cache.
    #[serde(default)]
    cache_write: u64,
    /// A token count in `usage`, and the tail of the text in `tool_result`.
    #[serde(default, borrow)]
    output: Option<Output<'a>>,
    /// The lines a `tool_result` frame left out before its tail.
    #[serde(default)]
    earlier: u64,
    /// The images a `tool_result` frame names, whose bytes it leaves out: the
    /// count is all the app needs, so each one is skipped as it is read.
    #[serde(default)]
    images: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default, borrow, deserialize_with = "cow")]
    tool: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    call: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    rule: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    save: Vec<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    reason: Option<Cow<'a, str>>,
    #[serde(default)]
    client: Option<u64>,
    #[serde(default, borrow, deserialize_with = "cow")]
    model: Option<Cow<'a, str>>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default, borrow, deserialize_with = "cow")]
    level: Option<Cow<'a, str>>,
    #[serde(default, borrow, deserialize_with = "cow")]
    mode: Option<Cow<'a, str>>,
    /// The questions a `question_request` names.
    #[serde(borrow, default)]
    questions: Vec<Question<'a>>,
}

/// One string field of a line: borrowed when the line wrote it verbatim, and
/// absent when the line leaves it out or sends null.
fn cow<'de: 'a, 'a, D>(deserializer: D) -> Result<Option<Cow<'a, str>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CowVisitor;

    impl<'de> Visitor<'de> for CowVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string")
        }

        fn visit_none<E: de::Error>(self) -> Result<Option<Cow<'de, str>>, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Option<Cow<'de, str>>, E> {
            Ok(None)
        }

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Option<Cow<'de, str>>, D::Error> {
            deserializer.deserialize_str(CowVisitor)
        }

        fn visit_borrowed_str<E: de::Error>(
            self,
            value: &'de str,
        ) -> Result<Option<Cow<'de, str>>, E> {
            Ok(Some(Cow::Borrowed(value)))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Option<Cow<'de, str>>, E> {
            Ok(Some(Cow::Owned(value.to_owned())))
        }
    }

    deserializer.deserialize_option(CowVisitor)
}

/// An `output` field as the line wrote it: a token count in `usage`, and the
/// tail of the text in `tool_result`.
enum Output<'a> {
    Count(u64),
    Text(Cow<'a, str>),
}

impl<'a> Output<'a> {
    /// The field as a count, when the line wrote a number.
    fn count(self) -> u64 {
        match self {
            Output::Count(count) => count,
            Output::Text(_) => 0,
        }
    }

    /// The field as text, when the line wrote a string.
    fn text(self) -> Option<Cow<'a, str>> {
        match self {
            Output::Text(text) => Some(text),
            Output::Count(_) => None,
        }
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Output<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OutputVisitor;

        impl<'de> Visitor<'de> for OutputVisitor {
            type Value = Output<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a count or the tail of an output")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Output<'de>, E> {
                Ok(Output::Count(value))
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Output<'de>, E> {
                Ok(Output::Text(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Output<'de>, E> {
                Ok(Output::Text(Cow::Owned(value.to_owned())))
            }
        }

        deserializer.deserialize_any(OutputVisitor)
    }
}

/// An id as the line wrote it: a call id in an event, a number in an approval.
enum Id<'a> {
    Num(u64),
    Text(Cow<'a, str>),
}

impl<'a> Id<'a> {
    /// The id as text, when the line wrote it as a string.
    fn text(self) -> Option<Cow<'a, str>> {
        match self {
            Id::Text(text) => Some(text),
            Id::Num(_) => None,
        }
    }

    /// The id as a number, when the line wrote it as one.
    fn num(self) -> Option<u64> {
        match self {
            Id::Num(num) => Some(num),
            Id::Text(_) => None,
        }
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Id<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct IdVisitor;

        impl<'de> Visitor<'de> for IdVisitor {
            type Value = Id<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a call id or a number")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Id<'de>, E> {
                Ok(Id::Num(value))
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Id<'de>, E> {
                Ok(Id::Text(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Id<'de>, E> {
                Ok(Id::Text(Cow::Owned(value.to_owned())))
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

/// Writes the head of the request `id`, clearing what the buffer held.
fn begin(out: &mut Vec<u8>, id: u64, method: &str) {
    out.clear();
    out.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
    push_u64(out, id);
    out.extend_from_slice(b",\"method\":\"");
    out.extend_from_slice(method.as_bytes());
    out.extend_from_slice(b"\",\"params\":");
}

/// Writes `value` as one JSON number.
fn push_u64(out: &mut Vec<u8>, value: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut rest = value;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

/// Writes `text` as one JSON string, quotes included.
fn push_str(out: &mut Vec<u8>, text: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for c in text.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                let code = c as u32;
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(code >> 4) as usize]);
                out.push(HEX[(code & 0xf) as usize]);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use reagent_protocol::lines::Lines;
    use reagent_protocol::message::{self, MAJOR, Mode, Request};
    use serde_json::json;
    use tokio::net::UnixListener;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use std::sync::Arc;

    use super::{Attached, Client, Frame, Initialized, Next, decode};

    /// How long a test waits for a frame before it fails.
    const WAIT: Duration = Duration::from_secs(10);

    /// The model the fake worker names in its `initialize` answer.
    const MODEL: &str = "fake/m";

    /// The context window the fake worker names there.
    const WINDOW: u64 = 200_000;

    /// A worker that answers `initialize` and `attach` and sends what the test
    /// scripts, on a socket in `/tmp` so its path stays short.
    struct Fake {
        // Held for the test, to remove the socket's folder at the end.
        _dir: tempfile::TempDir,
        socket: PathBuf,
        /// The raw line of every request the client sent.
        sent: mpsc::UnboundedReceiver<String>,
        /// The lines the client has to read.
        out: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl Fake {
        fn start(session: &str, committed: u64) -> Fake {
            Fake::start_replaying(session, committed, Vec::new())
        }

        /// Starts a worker whose `initialize` answer names `model` and `window`,
        /// and no model at all when they are `None`, as an older worker did.
        fn naming(
            session: &str,
            committed: u64,
            model: Option<&'static str>,
            window: Option<u64>,
        ) -> Fake {
            Fake::start_with(session, committed, Vec::new(), model, window)
        }

        /// Starts a worker that sends `catch_up` before the answer to `attach`,
        /// as it does the frames a client missed in one frame.
        fn start_replaying(session: &str, committed: u64, catch_up: Vec<u8>) -> Fake {
            Fake::start_with(session, committed, catch_up, Some(MODEL), Some(WINDOW))
        }

        fn start_with(
            session: &str,
            committed: u64,
            catch_up: Vec<u8>,
            model: Option<&'static str>,
            window: Option<u64>,
        ) -> Fake {
            let dir = tempfile::Builder::new()
                .prefix("tui")
                .tempdir_in("/tmp")
                .expect("a temp folder in /tmp");
            let socket = dir.path().join("s.sock");
            let listener = UnixListener::bind(&socket).expect("bind the socket");
            let (sent, sent_rx) = mpsc::unbounded_channel();
            let (out, queue) = mpsc::unbounded_channel();
            let answers = out.clone();
            let session = session.to_owned();
            tokio::spawn(async move {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (read, out) = stream.into_split();
                tokio::spawn(write_lines(out, queue));
                let mut answers = Answers {
                    out: answers,
                    sent,
                    session,
                    committed,
                    catch_up,
                    model,
                    window,
                };
                answers.read(read).await;
            });
            Fake {
                _dir: dir,
                socket,
                sent: sent_rx,
                out,
            }
        }

        /// Sends one notification line to the client.
        fn notify(&self, method: &str, params: &[u8]) {
            let mut line = Vec::new();
            message::notify(&mut line, method, params);
            let _ = self.out.send(line);
        }

        /// Sends one event line to the client.
        fn event(&self, body: serde_json::Value) {
            self.notify("event", &serde_json::to_vec(&body).expect("a body"));
        }
    }

    /// Writes every line the fake hands it, in order.
    async fn write_lines(mut write: OwnedWriteHalf, mut queue: mpsc::UnboundedReceiver<Vec<u8>>) {
        while let Some(line) = queue.recv().await {
            if write.write_all(&line).await.is_err() {
                return;
            }
        }
    }

    /// The reading side of the fake: it answers the handshake and records the
    /// lines the client sent.
    struct Answers {
        out: mpsc::UnboundedSender<Vec<u8>>,
        sent: mpsc::UnboundedSender<String>,
        session: String,
        committed: u64,
        /// The frames written before the answer to `attach`.
        catch_up: Vec<u8>,
        /// The model the `initialize` answer names, when it names one.
        model: Option<&'static str>,
        /// The context window it names, when it names one.
        window: Option<u64>,
    }

    impl Answers {
        async fn read(&mut self, mut read: OwnedReadHalf) {
            let mut lines = Lines::new();
            loop {
                match lines.fill(&mut read).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                while let Some(Ok(line)) = lines.next() {
                    let raw = String::from_utf8_lossy(line).into_owned();
                    let Ok((id, request)) = message::decode(line) else {
                        continue;
                    };
                    let attaches = matches!(&request, Request::Attach { .. });
                    let body = match request {
                        Request::Initialize { .. } => {
                            let mut body = json!({
                                "major": MAJOR,
                                "version": "1",
                                "session": self.session,
                                "client": 7,
                                "capabilities": [],
                                "committed": self.committed,
                            });
                            if let Some(model) = self.model {
                                body["model"] = json!(model);
                            }
                            if let Some(window) = self.window {
                                body["context_window"] = json!(window);
                            }
                            body
                        }
                        _ => json!({ "committed": self.committed, "running": true }),
                    };
                    if attaches && !self.catch_up.is_empty() {
                        let _ = self.out.send(std::mem::take(&mut self.catch_up));
                    }
                    let mut answer = Vec::new();
                    message::result(&mut answer, id.unwrap_or(0), &body);
                    let _ = self.out.send(answer);
                    let _ = self.sent.send(raw);
                }
            }
        }
    }

    #[tokio::test]
    async fn the_catch_up_frame_before_the_attach_answer_is_drawn() {
        let mut catch_up = Vec::new();
        message::notify(
            &mut catch_up,
            "event",
            br#"{"type":"text_delta","text":"caught up"}"#,
        );
        let fake = Fake::start_replaying("sess", 0, catch_up);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        client.attach(0).await.expect("attach");
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::TextDelta(text)) => assert_eq!(text, "caught up"),
            other => panic!("the frame that came before the answer is kept, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_initializes_and_attaches() {
        let mut fake = Fake::start("sess-abcdef12", 42);
        let (mut client, init) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        assert_eq!(
            init,
            Initialized {
                session: "sess-abcdef12".to_owned(),
                client: 7,
                committed: 42,
                model: Some(Arc::from(MODEL)),
                context_window: Some(WINDOW),
                level: None,
            }
        );
        let attached = client.attach(42).await.expect("attach");
        assert_eq!(
            attached,
            Attached {
                committed: 42,
                running: true,
            },
            "the answer names the record and the run"
        );
        let first = timeout(WAIT, fake.sent.recv())
            .await
            .expect("the initialize line")
            .expect("the fake worker");
        let (_, request) = message::decode(first.as_bytes()).expect("decode");
        match request {
            Request::Initialize {
                major,
                client,
                token,
                ..
            } => {
                assert_eq!(major, MAJOR);
                assert_eq!(client.name, "probe");
                assert_eq!(token, "tok");
            }
            other => panic!("expected initialize, got {other:?}"),
        }
        let second = timeout(WAIT, fake.sent.recv())
            .await
            .expect("the attach line")
            .expect("the fake worker");
        let (_, request) = message::decode(second.as_bytes()).expect("decode");
        assert_eq!(request, Request::Attach { from: 42 });
    }

    /// The `initialize` answer names the model of the session and the window of
    /// that model, which the context panel shows.
    #[tokio::test]
    async fn initialize_names_the_model_and_its_context_window() {
        let fake = Fake::start("sess", 0);
        let (_, init) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        assert_eq!(init.model.as_deref(), Some(MODEL));
        assert_eq!(init.context_window, Some(WINDOW));
    }

    /// A worker from before the answer named the model still works, and says no
    /// model rather than a wrong one.
    #[tokio::test]
    async fn an_old_worker_answers_without_a_model() {
        let fake = Fake::naming("sess", 0, None, None);
        let (_, init) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        assert_eq!(init.model, None);
        assert_eq!(init.context_window, None);
    }

    /// A settings notice, which the worker sends every client after a model
    /// change, carries the new model, its window, the level and the mode.
    #[tokio::test]
    async fn a_settings_notice_names_the_model_and_its_context_window() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        let params = json!({
            "model": "fake/other",
            "context_window": 128_000,
            "level": "high",
            "mode": "ask",
        });
        fake.notify("settings", &serde_json::to_vec(&params).expect("a body"));
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::Settings {
                model,
                context_window,
                level,
                mode,
            }) => {
                assert_eq!(model, "fake/other");
                assert_eq!(context_window, Some(128_000));
                assert_eq!(level.as_deref(), Some("high"));
                assert_eq!(mode.as_deref(), Some("ask"));
                assert!(
                    matches!(model, Cow::Borrowed(_)),
                    "the model points into the line: {model:?}"
                );
            }
            other => panic!("expected the settings notice, got {other:?}"),
        }
    }

    /// A client that joins the session names its id and the name it chose, and
    /// one that leaves names only its id: the worker keeps no name for it.
    #[tokio::test]
    async fn client_notices_carry_the_id_and_the_name() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        fake.notify("client_joined", br#"{"client":7,"name":"tmux:2"}"#);
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::ClientJoined { id, name }) => {
                assert_eq!(id, 7);
                assert_eq!(name, "tmux:2");
                assert!(
                    matches!(name, Cow::Borrowed(_)),
                    "the name points into the line: {name:?}"
                );
            }
            other => panic!("expected the join notice, got {other:?}"),
        }
        fake.notify("client_left", br#"{"client":7}"#);
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::ClientLeft { id }) => assert_eq!(id, 7),
            other => panic!("expected the leave notice, got {other:?}"),
        }
    }

    /// A usage event, which the worker sends after every call, carries the
    /// tokens of the request and the reply.
    #[tokio::test]
    async fn a_usage_event_carries_the_tokens_of_the_request() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        fake.event(json!({
            "type": "usage",
            "input": 50_000,
            "output": 1_200,
            "cache_read": 0,
            "cache_write": 0,
            "cache_hit_rate": 0.0,
            "cost": null,
        }));
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::Usage {
                input,
                cached,
                output,
                cost,
            }) => {
                assert_eq!((input, output), (50_000, 1_200));
                assert_eq!(cached, 0, "a line without cache tokens reads none");
                assert_eq!(cost, None);
            }
            other => panic!("expected the usage, got {other:?}"),
        }
    }

    /// A usage event carries the cache tokens of the request: what it read from
    /// the provider's cache and what it wrote to it, added together.
    #[tokio::test]
    async fn stream_tokens_carry_the_cache_of_the_request() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        fake.event(json!({
            "type": "usage",
            "input": 1_000,
            "output": 10,
            "cache_read": 40,
            "cache_write": 2,
            "cost": null,
        }));
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::Usage { cached, .. }) => assert_eq!(cached, 42),
            other => panic!("expected the usage, got {other:?}"),
        }
    }

    /// An approval request carries the rule that asked and the rules an always
    /// answer saves, the rule borrowed from the line and no `Value` in between.
    ///
    /// A `Cow` inside a sequence comes back owned: serde copies a string it
    /// cannot lend out of a `Vec`. That is one small string per rule on a frame
    /// that arrives once per card, and the card keeps it as its own string, so
    /// nothing is copied twice.
    #[tokio::test]
    async fn an_approval_request_carries_the_rules_it_saves() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        let params = json!({
            "id": 7,
            "tool": "bash",
            "call": "rm -rf target",
            "rule": "Bash(rm:*)",
            "save": ["Bash(rm -rf target:*)"],
        });
        fake.notify(
            "approval_request",
            &serde_json::to_vec(&params).expect("a body"),
        );
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        let Next::Frame(Frame::ApprovalRequest { request, saves }) = next else {
            panic!("expected the approval request, got {next:?}");
        };
        assert_eq!(request.id, 7);
        assert_eq!(request.rule, "Bash(rm:*)");
        assert!(request.save, "the request offers the save answer");
        assert_eq!(saves, ["Bash(rm -rf target:*)"]);
        assert!(
            matches!(request.rule, Cow::Borrowed(_)),
            "the rule points into the line: {:?}",
            request.rule
        );
    }

    #[tokio::test]
    async fn a_line_the_build_does_not_know_is_skipped() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        fake.notify("nonsense", br#"{"anything":1}"#);
        fake.event(json!({ "type": "no_such_event" }));
        fake.event(json!({ "type": "text_delta", "text": "hello" }));
        for _ in 0..2 {
            let next = timeout(WAIT, client.next_frame())
                .await
                .expect("a frame")
                .expect("the worker stays");
            assert!(
                matches!(next, Next::Frame(Frame::Unknown)),
                "a line this build does not know reads as unknown: {next:?}"
            );
        }
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        match next {
            Next::Frame(Frame::TextDelta(text)) => assert_eq!(text, "hello"),
            other => panic!("expected the delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_borrows_from_the_line() {
        let fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        fake.event(json!({ "type": "text_delta", "text": "borrowed" }));
        let next = timeout(WAIT, client.next_frame())
            .await
            .expect("a frame")
            .expect("the worker stays");
        let Next::Frame(Frame::TextDelta(text)) = next else {
            panic!("expected the delta")
        };
        assert!(
            matches!(text, Cow::Borrowed(_)),
            "the delta points into the line: {text:?}"
        );
    }

    #[tokio::test]
    async fn requests_carry_the_answer_the_text_and_the_mode() {
        let mut fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        client
            .send("say \"hi\"\nnow", Mode::Steer)
            .await
            .expect("send");
        client
            .approve(9, message::AnswerKind::Refuse, Some("no"))
            .await
            .expect("approve");
        client
            .answer(9, Some(&[vec!["sqlite".to_owned()], vec![]]))
            .await
            .expect("answer");
        client.answer(9, None).await.expect("skip");
        client.cancel().await.expect("cancel");
        client.detach().await.expect("detach");
        // The handshake's own line comes first.
        let _ = timeout(WAIT, fake.sent.recv())
            .await
            .expect("the handshake")
            .expect("the fake worker");
        let mut got = Vec::new();
        for _ in 0..6 {
            let line = timeout(WAIT, fake.sent.recv())
                .await
                .expect("a request")
                .expect("the fake worker");
            let (_, request) = message::decode(line.as_bytes()).expect("decode");
            got.push(format!("{request:?}"));
        }
        assert_eq!(
            got,
            vec![
                "Send { text: \"say \\\"hi\\\"\\nnow\", mode: Steer }".to_owned(),
                "Approve { id: 9, answer: Refuse, reason: Some(\"no\") }".to_owned(),
                "Answer { id: 9, answers: Some([[\"sqlite\"], []]) }".to_owned(),
                "Answer { id: 9, answers: None }".to_owned(),
                "Cancel".to_owned(),
                "Detach".to_owned(),
            ],
            "every request decodes as the protocol says"
        );
    }

    /// A set request names only the fields it sets, so a key that changes the
    /// level keeps the model and the mode as they are.
    #[tokio::test]
    async fn a_set_request_names_only_the_fields_it_sets() {
        let mut fake = Fake::start("sess", 0);
        let (mut client, _) = Client::connect(&fake.socket, "tok", "probe")
            .await
            .expect("connect");
        client
            .set(None, Some("low"), None)
            .await
            .expect("set the level");
        // The handshake's own line comes first.
        let _ = timeout(WAIT, fake.sent.recv())
            .await
            .expect("the handshake")
            .expect("the fake worker");
        let line = timeout(WAIT, fake.sent.recv())
            .await
            .expect("the set line")
            .expect("the fake worker");
        let (_, request) = message::decode(line.as_bytes()).expect("decode");
        assert_eq!(
            request,
            Request::Set {
                model: None,
                level: Some(Cow::Borrowed("low")),
                mode: None,
            }
        );
    }

    /// One worker line, as the worker writes it, decoded.
    fn frame(line: &str) -> Option<Frame<'_>> {
        decode(line.as_bytes())
    }

    /// Every event type decodes to the frame the app draws, with its fields.
    #[test]
    fn decode_events_give_the_frame_of_their_type() {
        assert!(matches!(
            frame(r#"{"jsonrpc":"2.0","method":"event","params":{"type":"turn_start"}}"#),
            Some(Frame::TurnStart)
        ));
        assert!(matches!(
            frame(
                r#"{"jsonrpc":"2.0","method":"event","params":{"type":"turn_end","reason":"done"}}"#
            ),
            Some(Frame::TurnEnd)
        ));
        assert!(matches!(
            frame(
                r#"{"jsonrpc":"2.0","method":"event","params":{"type":"run_end","reason":"cancelled"}}"#
            ),
            Some(Frame::RunEnd { at_limit: false })
        ));
        assert!(matches!(
            frame(
                r#"{"jsonrpc":"2.0","method":"event","params":{"type":"run_end","reason":"max_turns"}}"#
            ),
            Some(Frame::RunEnd { at_limit: true })
        ));
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"hello"}}"#,
        ) {
            Some(Frame::TextDelta(text)) => assert_eq!(text, "hello"),
            other => panic!("expected the text delta, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"thinking_delta","text":"hm"}}"#,
        ) {
            Some(Frame::ThinkingDelta(text)) => assert_eq!(text, "hm"),
            other => panic!("expected the thinking delta, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_start","id":"call_1","name":"bash"}}"#,
        ) {
            Some(Frame::ToolStart { id, name }) => assert_eq!((&*id, &*name), ("call_1", "bash")),
            other => panic!("expected the tool start, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_call_ready","id":"call_1","name":"bash","arguments":"{}"}}"#,
        ) {
            Some(Frame::ToolCallReady {
                id,
                name,
                arguments,
            }) => {
                assert_eq!((&*id, &*name), ("call_1", "bash"));
                assert_eq!(arguments, "{}");
            }
            other => panic!("expected the call ready, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_1","is_error":true,"bytes":12,"error":"boom"}}"#,
        ) {
            Some(Frame::ToolResult {
                id,
                failed,
                bytes,
                error,
                output,
                ..
            }) => {
                assert_eq!(output, None, "a worker that sends no tail");
                assert_eq!(id, "call_1");
                assert!(failed, "the tool reported a failure");
                assert_eq!(bytes, 12);
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("expected the tool result, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_2","is_error":false,"bytes":10}}"#,
        ) {
            Some(Frame::ToolResult { failed, error, .. }) => {
                assert!(!failed);
                assert_eq!(error, None);
            }
            other => panic!("expected the tool result, got {other:?}"),
        }
        // A frame with the tail of the output of a failed call carries both.
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_3","is_error":true,"bytes":9000,"error":"boom","output":"the last line","earlier":298}}"#,
        ) {
            Some(Frame::ToolResult {
                id,
                failed,
                bytes,
                error,
                output,
                earlier,
                images,
            }) => {
                assert_eq!(id, "call_3");
                assert!(failed, "the tool reported a failure");
                assert_eq!(bytes, 9000);
                assert_eq!(error.as_deref(), Some("boom"));
                assert_eq!(output.as_deref(), Some("the last line"));
                assert_eq!(earlier, 298);
                assert_eq!(images, 0, "a frame that names no image");
            }
            other => panic!("expected the tool output, got {other:?}"),
        }
        // A result that produced images names their number, and no bytes come
        // over the line.
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","id":"call_4","bytes":80,"images":[{"media_type":"image/png","width":64,"height":48,"bytes":700},{"media_type":"image/png","width":8,"height":8,"bytes":90}]}}"#,
        ) {
            Some(Frame::ToolResult { id, images, .. }) => {
                assert_eq!(id, "call_4");
                assert_eq!(images, 2);
            }
            other => panic!("expected the tool result with its images, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"user_message","client":7,"mode":"steer","text":"hi"}}"#,
        ) {
            Some(Frame::UserMessage { text, images }) => {
                assert_eq!(text, "hi");
                assert_eq!(images, 0);
            }
            other => panic!("expected the message, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"user_message","client":7,"mode":"steer","text":"[Image #1]","images":[{"media_type":"image/png","width":64,"height":48,"bytes":700}]}}"#,
        ) {
            Some(Frame::UserMessage { images, .. }) => assert_eq!(images, 1),
            other => panic!("expected the message, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"error","message":"boom"}}"#,
        ) {
            Some(Frame::Error(message)) => assert_eq!(message, "boom"),
            other => panic!("expected the error, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"retry","attempt":2,"delay_ms":500,"error":"429"}}"#,
        ) {
            Some(Frame::Retry { attempt, error }) => {
                assert_eq!(attempt, 2);
                assert_eq!(error, "429");
            }
            other => panic!("expected the retry, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"committed","record":7}}"#,
        ) {
            Some(Frame::Committed(record)) => assert_eq!(record, 7),
            other => panic!("expected the committed record, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"usage","input":10,"output":4,"cost":0.5}}"#,
        ) {
            Some(Frame::Usage {
                input,
                cached,
                output,
                cost,
            }) => {
                assert_eq!((input, output, cost), (10, 4, Some(0.5)));
                assert_eq!(cached, 0);
            }
            other => panic!("expected the usage, got {other:?}"),
        }
    }

    /// Every notice the worker sends decodes to the frame the app draws.
    #[test]
    fn decode_methods_give_the_frame_of_each_notice() {
        match frame(r#"{"jsonrpc":"2.0","method":"closing","params":{"reason":"behind"}}"#) {
            Some(Frame::Closing(reason)) => assert_eq!(reason, "behind"),
            other => panic!("expected the closing, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"approval_resolved","params":{"id":4,"client":7,"answer":"once"}}"#,
        ) {
            Some(Frame::ApprovalResolved { id }) => assert_eq!(id, 4),
            other => panic!("expected the resolved request, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"client_joined","params":{"client":2,"name":"web"}}"#,
        ) {
            Some(Frame::ClientJoined { id, name }) => {
                assert_eq!(id, 2);
                assert_eq!(name, "web");
            }
            other => panic!("expected the join notice, got {other:?}"),
        }
        match frame(r#"{"jsonrpc":"2.0","method":"client_joined","params":{"client":2}}"#) {
            Some(Frame::ClientJoined { id, name }) => {
                assert_eq!(id, 2);
                assert_eq!(name, "", "a notice that names no name reads as empty");
            }
            other => panic!("expected the join notice, got {other:?}"),
        }
        match frame(r#"{"jsonrpc":"2.0","method":"client_left","params":{"client":2}}"#) {
            Some(Frame::ClientLeft { id }) => assert_eq!(id, 2),
            other => panic!("expected the leave notice, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"settings","params":{"model":"p/m","context_window":1000,"level":"high","mode":"ask"}}"#,
        ) {
            Some(Frame::Settings {
                model,
                context_window,
                level,
                mode,
            }) => {
                assert_eq!(model, "p/m");
                assert_eq!(context_window, Some(1000));
                assert_eq!(level.as_deref(), Some("high"));
                assert_eq!(mode.as_deref(), Some("ask"));
            }
            other => panic!("expected the settings, got {other:?}"),
        }
        match frame(r#"{"jsonrpc":"2.0","method":"settings","params":{}}"#) {
            Some(Frame::Settings {
                model,
                context_window,
                level,
                mode,
            }) => {
                assert_eq!(model, "", "a notice that names no model reads as empty");
                assert_eq!((context_window, level, mode), (None, None, None));
            }
            other => panic!("expected the settings, got {other:?}"),
        }
    }

    /// An approval request offers the save answer only when it names rules.
    #[test]
    fn decode_an_approval_request_offers_a_save_only_with_rules() {
        match frame(
            r#"{"jsonrpc":"2.0","method":"approval_request","params":{"id":7,"tool":"bash","call":"rm -rf target","rule":"Bash(rm:*)","save":["Bash(rm -rf target:*)"]}}"#,
        ) {
            Some(Frame::ApprovalRequest { request, saves }) => {
                assert_eq!(request.id, 7);
                assert_eq!(request.tool, "bash");
                assert_eq!(request.call, "rm -rf target");
                assert_eq!(request.rule, "Bash(rm:*)");
                assert!(request.save, "the request offers the save answer");
                assert_eq!(saves, ["Bash(rm -rf target:*)"]);
            }
            other => panic!("expected the approval request, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"approval_request","params":{"id":7,"tool":"bash","call":"rm -rf target","rule":"Bash(rm:*)"}}"#,
        ) {
            Some(Frame::ApprovalRequest { request, saves }) => {
                assert!(!request.save, "no rules arrive, so no save is offered");
                assert!(saves.is_empty());
            }
            other => panic!("expected the approval request, got {other:?}"),
        }
    }

    /// A question request carries the questions, their options and their notes,
    /// and a resolved notice names the request it closes.
    #[test]
    fn question_request_carries_the_questions_and_their_notes() {
        match frame(
            r#"{"jsonrpc":"2.0","method":"question_request","params":{"id":3,"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres","note":"on the server"}],"multi":false},{"q":"Which stores?","options":[{"label":"redis"}],"multi":true}]}}"#,
        ) {
            Some(Frame::QuestionRequest { id, questions }) => {
                assert_eq!(id, 3);
                assert_eq!(questions.len(), 2, "one entry per question");
                let first = &questions[0];
                assert_eq!(first.q, "Which database?");
                assert!(!first.multi);
                assert_eq!(first.options.len(), 2);
                assert_eq!(first.options[0].label, "sqlite");
                assert_eq!(first.options[0].note, None, "an option may have no note");
                assert_eq!(first.options[1].note.as_deref(), Some("on the server"));
                assert!(questions[1].multi, "a question may take several picks");
            }
            other => panic!("expected the question request, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"question_resolved","params":{"id":3,"client":7}}"#,
        ) {
            Some(Frame::QuestionResolved { id }) => assert_eq!(id, 3),
            other => panic!("expected the resolved question, got {other:?}"),
        }
    }

    /// A worker may write the keys in any order: only the names matter.
    #[test]
    fn decode_reads_the_keys_in_any_order() {
        match frame(
            r#"{"params":{"type":"text_delta","text":"x"},"method":"event","jsonrpc":"2.0"}"#,
        ) {
            Some(Frame::TextDelta(text)) => assert_eq!(text, "x"),
            other => panic!("expected the delta, got {other:?}"),
        }
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"text":"y","type":"text_delta"}}"#,
        ) {
            Some(Frame::TextDelta(text)) => assert_eq!(text, "y"),
            other => panic!("expected the delta, got {other:?}"),
        }
        match frame(
            r#"{"params":{"mode":"ask","model":"p/m","level":"low","context_window":9,"type":"settings"},"jsonrpc":"2.0","method":"settings"}"#,
        ) {
            Some(Frame::Settings {
                model,
                context_window,
                level,
                mode,
            }) => {
                assert_eq!(model, "p/m");
                assert_eq!(context_window, Some(9));
                assert_eq!(level.as_deref(), Some("low"));
                assert_eq!(mode.as_deref(), Some("ask"));
            }
            other => panic!("expected the settings, got {other:?}"),
        }
    }

    /// A method or a type this build does not know, and an answer to a request,
    /// have nothing to draw.
    #[test]
    fn decode_an_unknown_line_gives_none() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"nonsense","params":{"anything":1}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"no_such_event"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"anything":1}}"#,
            r#"{"jsonrpc":"2.0","method":"event"}"#,
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{}}",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"no"}}"#,
        ] {
            assert!(frame(line).is_none(), "{line} decoded to a frame");
        }
    }

    /// A notice without a field it cannot draw without is skipped, as it was
    /// when every notice had a struct of its own.
    #[test]
    fn decode_a_missing_required_field_gives_none() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"thinking_delta"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_start","name":"bash"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_call_ready","id":"c","name":"bash"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"tool_result","bytes":1}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"user_message"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"error"}}"#,
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"retry","attempt":1}}"#,
            r#"{"jsonrpc":"2.0","method":"closing","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"client_joined","params":{"name":"web"}}"#,
            r#"{"jsonrpc":"2.0","method":"client_left","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"approval_resolved","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"approval_request","params":{"tool":"bash","call":"c","rule":"r"}}"#,
        ] {
            assert!(frame(line).is_none(), "{line} decoded with a field missing");
        }
    }

    /// Text with an escape is unescaped once, and the frame still draws.
    #[test]
    fn decode_unescapes_the_text_of_a_line() {
        match frame(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"a\"b"}}"#,
        ) {
            Some(Frame::TextDelta(text)) => assert_eq!(text, "a\"b"),
            other => panic!("expected the delta, got {other:?}"),
        }
    }
}

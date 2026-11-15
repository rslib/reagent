//! One ACP session: its worker, the frames it sends, and the turn that is open.
//!
//! A session task owns its connection to the worker: it reads the frames, turns
//! each into the updates of the protocol, and carries out what the agent asks it
//! for. Nothing of it polls.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use reagent_ai::Content;
use reagent_ai::Message;
use reagent_protocol::message::AnswerKind;
use reagent_protocol::message::Mode;
use reagent_tui::client::Approval;
use reagent_tui::client::Client;
use reagent_tui::client::Frame;
use reagent_tui::client::Next;
use reagent_tui::client::Question;
use reagent_tui::client::RunEnd;
use reagent_tui::history;
use reagent_tui::picture;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

use super::editor::Editor;
use super::editor::Reply;
use super::message::CallRef;
use super::message::Change;
use super::message::Chunk;
use super::message::Kind;
use super::message::Location;
use super::message::OptionKind;
use super::message::Permission;
use super::message::PermissionOption;
use super::message::SessionUpdate;
use super::message::Status;
use super::message::ToolContent;

/// The longest text of one tool result this agent sends, in bytes.
const CAP: usize = 8 * 1024;

/// What the agent asks a session to do.
pub(super) enum Command {
    /// Runs the prompt of the request `id`.
    Prompt { id: Value, text: String },
    /// Stops the open turn.
    Cancel,
    /// Answers the worker's question `id` with no answer, which skips it.
    Skip { id: u64 },
    /// The editor answered the request this session sent as `request`.
    Answer { request: u64, reply: Reply },
}

/// What one step of a session's task came to.
enum Step {
    /// The worker gave a frame, or a line that is not complete yet.
    Frame,
    /// The agent asked for something.
    Command(Command),
}

/// One tool call of the run, from the id the provider gave it.
struct Tool {
    /// The call's id.
    id: String,
    /// The tool's name.
    name: String,
    /// The arguments, once the model wrote them all.
    arguments: Option<Value>,
}

/// A request this session sent to the editor, and the worker's request it
/// answers.
enum Waiting {
    /// An approval of the worker's approval `id`.
    Approval { id: u64, saves: bool },
    /// A question set of the worker's question `id`, with the label of each
    /// answer it offered.
    Question { id: u64, labels: Vec<String> },
}

/// One ACP session: its state, and the editor it speaks to.
pub(super) struct Session {
    /// The session, as the worker names it.
    id: String,
    /// The folder the session runs in.
    cwd: PathBuf,
    /// The editor on the other end.
    editor: Editor,
    /// The channel this task listens on, which a task it starts answers on.
    commands: Sender<Command>,
    /// The tool calls of the open turn, oldest first, each with no result yet.
    tools: Vec<Tool>,
    /// The requests this session sent to the editor, by its own id.
    open: HashMap<u64, Waiting>,
    /// The id of the request of the open turn, when a turn is open.
    turn: Option<Value>,
    /// The error text of the run, when one failed.
    failure: Option<String>,
}

impl Session {
    /// Makes the session of a worker, which the caller starts.
    pub(super) fn new(id: String, cwd: PathBuf, editor: Editor, commands: Sender<Command>) -> Self {
        Session {
            id,
            cwd,
            editor,
            commands,
            tools: Vec::new(),
            open: HashMap::new(),
            turn: None,
            failure: None,
        }
    }

    /// Reads the frames of the worker and carries out what the agent asks,
    /// until the worker closes the connection.
    pub(super) async fn run(mut self, mut client: Client, mut commands: Receiver<Command>) {
        loop {
            let step = tokio::select! {
                frame = client.next_frame() => match frame {
                    Ok(Next::Frame(frame)) => {
                        self.take(frame).await;
                        Step::Frame
                    }
                    Ok(Next::More) => Step::Frame,
                    Ok(Next::End) | Err(_) => break,
                },
                command = commands.recv() => match command {
                    Some(command) => Step::Command(command),
                    None => break,
                },
            };
            let going = match step {
                Step::Frame => true,
                Step::Command(command) => self.command(command, &mut client).await,
            };
            if !going {
                break;
            }
        }
        // The worker went: a turn that waited for it has no answer of its own.
        if let Some(id) = self.turn.take() {
            self.editor
                .fail(id, -32603, "the session's worker closed", None)
                .await;
        }
        let _ = client.detach().await;
    }

    /// Carries out one request of the agent, and answers whether the task goes
    /// on.
    async fn command(&mut self, command: Command, client: &mut Client) -> bool {
        match command {
            Command::Prompt { id, text } => {
                if self.turn.is_some() {
                    self.editor
                        .fail(id, -32000, "a turn is running", None)
                        .await;
                    return true;
                }
                match client.send(&text, Mode::Queue).await {
                    Ok(()) => {
                        self.turn = Some(id);
                        self.failure = None;
                    }
                    Err(error) => {
                        self.editor
                            .fail(
                                id,
                                -32603,
                                &format!("the session took no prompt: {error}"),
                                None,
                            )
                            .await;
                    }
                }
                true
            }
            Command::Cancel => {
                let _ = client.cancel().await;
                true
            }
            Command::Skip { id } => {
                let _ = client.answer(id, None).await;
                true
            }
            Command::Answer { request, reply } => {
                self.answer(request, reply, client).await;
                true
            }
        }
    }

    /// Passes the editor's answer to the worker, for a request that still waits
    /// for one.
    async fn answer(&mut self, request: u64, reply: Reply, client: &mut Client) {
        let Some(waiting) = self.open.remove(&request) else {
            return;
        };
        match waiting {
            Waiting::Approval { id, saves } => {
                let answer = match reply {
                    Ok(result) => match result["outcome"]["optionId"].as_str() {
                        Some("allow_once") => AnswerKind::Once,
                        Some("allow_always") if saves => AnswerKind::Save,
                        Some("allow_always") => AnswerKind::Session,
                        Some("reject_once") => AnswerKind::Refuse,
                        _ => AnswerKind::Skip,
                    },
                    Err(_) => AnswerKind::Skip,
                };
                let _ = client.approve(id, answer, None).await;
            }
            Waiting::Question { id, labels } => {
                let pick = match reply {
                    Ok(result) => result["outcome"]["optionId"]
                        .as_str()
                        .and_then(|at| at.parse::<usize>().ok())
                        .and_then(|at| labels.get(at).cloned()),
                    Err(_) => None,
                };
                let answers = pick.map(|label| vec![vec![label]]);
                let _ = client.answer(id, answers.as_deref()).await;
            }
        }
    }

    /// Turns one frame of the worker into what the editor shows, or into the
    /// answer of the open turn.
    async fn take(&mut self, frame: Frame<'_>) {
        match frame {
            Frame::TextDelta(text) => {
                self.send(Change::AgentMessageChunk {
                    content: Chunk::Text { text },
                })
                .await;
            }
            Frame::ThinkingDelta(text) => {
                self.send(Change::AgentThoughtChunk {
                    content: Chunk::Text { text },
                })
                .await;
            }
            Frame::UserMessage { text, .. } => {
                self.send(Change::UserMessageChunk {
                    content: Chunk::Text { text },
                })
                .await;
            }
            Frame::ToolStart { id, name } => {
                let kind = kind_of(&name);
                self.tools.push(Tool {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: None,
                });
                self.send(Change::ToolCall {
                    tool_call_id: &id,
                    title: name.to_string(),
                    kind,
                    status: Status::Pending,
                    raw_input: None,
                })
                .await;
            }
            Frame::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                let parsed = serde_json::from_str::<Value>(&arguments).ok();
                let title = title_of(&name, parsed.as_ref());
                let locations = locations_of(parsed.as_ref(), &self.cwd);
                let call = Tool {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: parsed.clone(),
                };
                match self.tools.iter_mut().find(|tool| tool.id == call.id) {
                    Some(known) => *known = call,
                    None => self.tools.push(call),
                }
                self.send(Change::ToolCallUpdate {
                    tool_call_id: &id,
                    status: Some(Status::InProgress),
                    title: Some(title),
                    raw_input: parsed,
                    locations,
                    content: Vec::new(),
                })
                .await;
            }
            Frame::ToolResult {
                id,
                failed,
                bytes,
                error,
                output,
                ..
            } => {
                let at = self.tools.iter().position(|tool| tool.id == *id);
                let tool = at.map(|at| self.tools.remove(at));
                let text = match (error, output) {
                    (Some(error), _) => error.into_owned(),
                    (None, Some(output)) => output.into_owned(),
                    (None, None) => format!("{bytes} bytes of output"),
                };
                let mut content = vec![ToolContent::Content {
                    content: Chunk::Text {
                        text: Cow::Owned(text),
                    },
                }];
                if let Some(tool) = &tool
                    && let Some(diff) = diff_of(&tool.name, tool.arguments.as_ref(), &self.cwd)
                {
                    content.push(diff);
                }
                self.send(Change::ToolCallUpdate {
                    tool_call_id: &id,
                    status: Some(if failed {
                        Status::Failed
                    } else {
                        Status::Completed
                    }),
                    title: None,
                    raw_input: None,
                    locations: Vec::new(),
                    content,
                })
                .await;
            }
            Frame::Error(text) => self.failure = Some(text.into_owned()),
            Frame::ApprovalRequest { request, saves } => {
                self.approval(request, !saves.is_empty()).await;
            }
            Frame::ApprovalResolved { id } => {
                self.open.retain(|_, waiting| {
                    !matches!(waiting, Waiting::Approval { id: other, .. } if *other == id)
                });
            }
            Frame::QuestionRequest { id, questions } => self.questions(id, questions).await,
            Frame::QuestionResolved { id } => {
                self.open.retain(|_, waiting| {
                    !matches!(waiting, Waiting::Question { id: other, .. } if *other == id)
                });
            }
            Frame::RunEnd { end } => self.end(end).await,
            _ => {}
        }
    }

    /// Sends one update of this session.
    async fn send(&self, update: Change<'_>) {
        self.editor
            .note(
                "session/update",
                SessionUpdate {
                    session_id: &self.id,
                    update,
                },
            )
            .await;
    }

    /// Asks the editor whether a tool call may run.
    async fn approval(&mut self, request: Approval<'_>, saves: bool) {
        let call = self
            .tools
            .last()
            .map(|tool| tool.id.clone())
            .unwrap_or_default();
        let request_id = self.editor.next_id();
        self.open.insert(
            request_id,
            Waiting::Approval {
                id: request.id,
                saves,
            },
        );
        let params = Permission {
            session_id: self.id.clone(),
            tool_call: CallRef { tool_call_id: call },
            options: vec![
                PermissionOption {
                    option_id: String::from("allow_once"),
                    name: String::from("Allow once"),
                    kind: OptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: String::from("allow_always"),
                    name: String::from("Allow always"),
                    kind: OptionKind::AllowAlways,
                },
                PermissionOption {
                    option_id: String::from("reject_once"),
                    name: String::from("Reject"),
                    kind: OptionKind::RejectOnce,
                },
            ],
        };
        let editor = self.editor.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let reply = editor.request("session/request_permission", params).await;
            let _ = commands
                .send(Command::Answer {
                    request: request_id,
                    reply,
                })
                .await;
        });
    }

    /// Asks the editor a question set the `ask` tool put, or says the editor
    /// cannot show it.
    ///
    /// One single-choice question is what a permission request can carry: an
    /// option per label. Everything else is answered with no answer, which the
    /// worker reads as a skip.
    async fn questions(&mut self, id: u64, questions: Vec<Question<'_>>) {
        let one = questions.len() == 1 && !questions[0].multi && !questions[0].options.is_empty();
        if !one {
            let asked = questions
                .first()
                .map(|question| question.q.to_string())
                .unwrap_or_default();
            let text = format!("reagent asked a question this editor cannot show: {asked}");
            self.send(Change::AgentMessageChunk {
                content: Chunk::Text {
                    text: Cow::Owned(text),
                },
            })
            .await;
            let _ = self.commands.send(Command::Skip { id }).await;
            return;
        }
        let labels: Vec<String> = questions[0]
            .options
            .iter()
            .map(|option| option.label.to_string())
            .collect();
        let options: Vec<PermissionOption> = labels
            .iter()
            .enumerate()
            .map(|(at, label)| PermissionOption {
                option_id: at.to_string(),
                name: label.clone(),
                kind: OptionKind::AllowOnce,
            })
            .collect();
        let request_id = self.editor.next_id();
        self.open
            .insert(request_id, Waiting::Question { id, labels });
        let params = Permission {
            session_id: self.id.clone(),
            tool_call: CallRef {
                tool_call_id: format!("question-{id}"),
            },
            options,
        };
        let editor = self.editor.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let reply = editor.request("session/request_permission", params).await;
            let _ = commands
                .send(Command::Answer {
                    request: request_id,
                    reply,
                })
                .await;
        });
    }

    /// Answers the open turn with why the run ended.
    async fn end(&mut self, end: RunEnd) {
        self.open.clear();
        self.tools.clear();
        let Some(id) = self.turn.take() else {
            return;
        };
        match end {
            RunEnd::Cancelled => {
                self.editor
                    .respond(id, json!({"stopReason": "cancelled"}))
                    .await
            }
            RunEnd::MaxTurns => {
                self.editor
                    .respond(id, json!({"stopReason": "max_turn_requests"}))
                    .await;
            }
            RunEnd::Error => {
                let text = self
                    .failure
                    .take()
                    .unwrap_or_else(|| String::from("the run failed"));
                self.editor.fail(id, -32603, &text, None).await;
            }
            RunEnd::EndTurn | RunEnd::Stopped | RunEnd::Other => {
                self.editor
                    .respond(id, json!({"stopReason": "end_turn"}))
                    .await;
            }
        }
    }
}

/// Replays the conversation of the session in `dir` up to the record `upto`, as
/// the updates a load sends, oldest first.
pub(super) async fn replay(
    editor: &Editor,
    session: &str,
    cwd: &Path,
    dir: PathBuf,
    upto: u64,
) -> Result<(), String> {
    let tail = history::read(dir, upto, picture::Room::columns(0))
        .await
        .map_err(|error| error.to_string())?;
    let mut calls: Vec<Tool> = Vec::new();
    for message in &tail.messages {
        match message {
            Message::User { content } => {
                for block in content {
                    if let Content::Text { text } = block {
                        note(
                            editor,
                            session,
                            Change::UserMessageChunk {
                                content: Chunk::Text {
                                    text: Cow::Borrowed(&**text),
                                },
                            },
                        )
                        .await;
                    }
                }
            }
            Message::Assistant(reply) => {
                for block in &reply.content {
                    match block {
                        Content::Text { text } => {
                            note(
                                editor,
                                session,
                                Change::AgentMessageChunk {
                                    content: Chunk::Text {
                                        text: Cow::Borrowed(&**text),
                                    },
                                },
                            )
                            .await;
                        }
                        Content::Thinking { thinking, .. } => {
                            note(
                                editor,
                                session,
                                Change::AgentThoughtChunk {
                                    content: Chunk::Text {
                                        text: Cow::Borrowed(&**thinking),
                                    },
                                },
                            )
                            .await;
                        }
                        Content::ToolCall(call) => {
                            let parsed = serde_json::from_str::<Value>(&call.arguments).ok();
                            calls.push(Tool {
                                id: call.id.to_string(),
                                name: call.name.to_string(),
                                arguments: parsed.clone(),
                            });
                            note(
                                editor,
                                session,
                                Change::ToolCall {
                                    tool_call_id: &call.id,
                                    title: title_of(&call.name, parsed.as_ref()),
                                    kind: kind_of(&call.name),
                                    status: Status::InProgress,
                                    raw_input: parsed,
                                },
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let at = calls.iter().position(|tool| tool.id == *result.call_id);
                let tool = at.map(|at| calls.remove(at));
                let mut content = vec![ToolContent::Content {
                    content: Chunk::Text {
                        text: Cow::Borrowed(cut(&result.content)),
                    },
                }];
                if let Some(tool) = &tool
                    && let Some(diff) = diff_of(&tool.name, tool.arguments.as_ref(), cwd)
                {
                    content.push(diff);
                }
                note(
                    editor,
                    session,
                    Change::ToolCallUpdate {
                        tool_call_id: &result.call_id,
                        status: Some(if result.is_error {
                            Status::Failed
                        } else {
                            Status::Completed
                        }),
                        title: None,
                        raw_input: None,
                        locations: Vec::new(),
                        content,
                    },
                )
                .await;
            }
        }
    }
    Ok(())
}

/// Sends one update of a replayed session.
async fn note(editor: &Editor, session: &str, update: Change<'_>) {
    editor
        .note(
            "session/update",
            SessionUpdate {
                session_id: session,
                update,
            },
        )
        .await;
}

/// The last part of `text` that fits in one line of this agent, at a line start.
fn cut(text: &str) -> &str {
    if text.len() <= CAP {
        return text;
    }
    let mut at = text.len() - CAP;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    match text[at..].find('\n') {
        Some(end) => &text[at + end + 1..],
        None => &text[at..],
    }
}

/// The path of a tool argument, absolute against the folder of the session.
fn absolute(cwd: &Path, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.display().to_string()
    } else {
        cwd.join(path).display().to_string()
    }
}

/// The line an editor shows for a call whose arguments are known.
fn title_of(name: &str, arguments: Option<&Value>) -> String {
    let argument = |key: &str| {
        arguments
            .and_then(|arguments| arguments[key].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let verb = match name {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        other => other,
    };
    match name {
        "read" | "write" | "edit" if !argument("path").is_empty() => {
            format!("{verb} {}", argument("path"))
        }
        "bash" if !argument("command").is_empty() => format!("$ {}", argument("command")),
        _ => name.to_string(),
    }
}

/// The kind of work a tool does.
fn kind_of(name: &str) -> Kind {
    match name {
        "read" => Kind::Read,
        "edit" | "write" => Kind::Edit,
        "bash" => Kind::Execute,
        "grep" | "find" => Kind::Search,
        "fetch" | "web" => Kind::Fetch,
        _ => Kind::Other,
    }
}

/// The files a call's arguments name, absolute.
fn locations_of(arguments: Option<&Value>, cwd: &Path) -> Vec<Location> {
    let Some(path) = arguments.and_then(|arguments| arguments["path"].as_str()) else {
        return Vec::new();
    };
    vec![Location {
        path: absolute(cwd, path),
    }]
}

/// The change an `edit` or a `write` made, from the arguments the model wrote.
fn diff_of(name: &str, arguments: Option<&Value>, cwd: &Path) -> Option<ToolContent<'static>> {
    let arguments = arguments?;
    let path = arguments["path"].as_str()?;
    let path = absolute(cwd, path);
    match name {
        "write" => Some(ToolContent::Diff {
            path,
            old_text: None,
            new_text: arguments["content"].as_str()?.to_string(),
        }),
        "edit" => Some(ToolContent::Diff {
            path,
            old_text: arguments["old_string"].as_str().map(str::to_string),
            new_text: arguments["new_string"].as_str()?.to_string(),
        }),
        _ => None,
    }
}

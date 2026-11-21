//! JSON-RPC 2.0 messages, one per line.
//!
//! Client to worker lines decode with borrowed serde: a string without escapes
//! points into the line. Worker to client lines are written into the caller's
//! buffer, so an event's `params` bytes are copied once and never parsed.

use std::borrow::Cow;
use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::error::Category;

/// The protocol major. The discovery folder and the handshake carry it.
pub const MAJOR: u32 = 1;

/// The methods a worker sends to its clients.
pub mod methods {
    pub const EVENT: &str = "event";
    pub const APPROVAL_REQUEST: &str = "approval_request";
    pub const APPROVAL_RESOLVED: &str = "approval_resolved";
    pub const QUESTION_REQUEST: &str = "question_request";
    pub const QUESTION_RESOLVED: &str = "question_resolved";
    pub const CLIENT_JOINED: &str = "client_joined";
    pub const CLIENT_LEFT: &str = "client_left";
    pub const CLOSING: &str = "closing";
    /// The model, its context window, the level and the mode after a `set`
    /// changed the model.
    pub const SETTINGS: &str = "settings";
    /// The JS plugin commands a client may run, sent once after it attaches.
    pub const PLUGIN_COMMANDS: &str = "plugin_commands";
    /// One panel of a JS plugin. A panel with no lines is hidden.
    pub const PLUGIN_PANEL: &str = "plugin_panel";
    /// A note to show in the transcript.
    pub const NOTE: &str = "note";
    /// A JS plugin command a client asked for is done.
    pub const COMMAND_END: &str = "command_end";
}

const JSONRPC: &str = "2.0";

/// How a `send` joins the run: after the current tool calls, or after the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Steer,
    Queue,
}

/// How a client answers an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    Once,
    Session,
    Save,
    Refuse,
    Skip,
}

/// The client's name and version, as it reports them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClientInfo<'a> {
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    #[serde(borrow)]
    pub version: Cow<'a, str>,
}

/// A client to worker message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request<'a> {
    Initialize {
        major: u32,
        client: ClientInfo<'a>,
        capabilities: Vec<Cow<'a, str>>,
        token: Cow<'a, str>,
    },
    Attach {
        from: u64,
    },
    Send {
        text: Cow<'a, str>,
        mode: Mode,
    },
    /// Runs one command a JS plugin offers, with the rest of the line as its
    /// arguments. The command's output comes back as a `note`.
    Command {
        name: Cow<'a, str>,
        args: Cow<'a, str>,
    },
    Approve {
        id: u64,
        answer: AnswerKind,
        reason: Option<Cow<'a, str>>,
    },
    Answer {
        id: u64,
        /// One list of labels per question, in the order asked, or `None` to
        /// skip the whole request.
        answers: Option<Vec<Vec<Cow<'a, str>>>>,
    },
    Cancel,
    Detach,
    Set {
        model: Option<Cow<'a, str>>,
        level: Option<Cow<'a, str>>,
        mode: Option<Cow<'a, str>>,
    },
    Compact {
        note: Option<Cow<'a, str>>,
    },
}

/// A JSON-RPC error code and message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: Cow<'static, str>,
}

impl RpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const WRONG_TOKEN: i32 = -32000;
    pub const WRONG_VERSION: i32 = -32001;
    pub const NOT_INITIALIZED: i32 = -32002;
    pub const BEHIND: i32 = -32003;
    /// The id an `approve` or an `answer` named is not waiting for a person.
    pub const NOT_WAITING: i32 = -32004;

    pub fn new(code: i32, message: &'static str) -> Self {
        Self {
            code,
            message: Cow::Borrowed(message),
        }
    }

    pub fn parse_error() -> Self {
        Self::new(Self::PARSE_ERROR, "parse error")
    }

    pub fn invalid_request() -> Self {
        Self::new(Self::INVALID_REQUEST, "invalid request")
    }

    pub fn method_not_found() -> Self {
        Self::new(Self::METHOD_NOT_FOUND, "method not found")
    }

    pub fn invalid_params(detail: String) -> Self {
        Self {
            code: Self::INVALID_PARAMS,
            message: Cow::Owned(format!("invalid params: {detail}")),
        }
    }

    pub fn wrong_token() -> Self {
        Self::new(Self::WRONG_TOKEN, "wrong token")
    }

    pub fn wrong_version() -> Self {
        Self::new(Self::WRONG_VERSION, "wrong protocol version")
    }

    pub fn not_initialized() -> Self {
        Self::new(Self::NOT_INITIALIZED, "not initialized")
    }

    pub fn behind() -> Self {
        Self::new(Self::BEHIND, "behind")
    }

    /// The error of an `approve` or an `answer` that names a request of `kind`
    /// which does not wait: the run raises nothing of that kind with that id.
    pub fn not_waiting(kind: &str, id: u64) -> Self {
        Self {
            code: Self::NOT_WAITING,
            message: Cow::Owned(format!("{kind} {id} is not waiting")),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// The worker's answer to `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitializeResult<'a> {
    pub major: u32,
    pub version: &'a str,
    pub session: &'a str,
    pub client: u64,
    pub capabilities: &'a [&'a str],
    pub committed: u64,
    /// The provider and model text the run uses.
    pub model: &'a str,
    /// The context window of the model, when the run knows it.
    pub context_window: Option<u64>,
    /// The reasoning level the session sends, when it sends one.
    pub level: Option<&'a str>,
    /// The skills the run found, in name order. A run that found none sends no
    /// key at all, so it sends the bytes it sent before the field.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub skills: &'a [SkillInfo<'a>],
    /// The permission mode in force, so a client that joins a live session knows
    /// it. No key at all when the sender names none, so the bytes are the ones
    /// sent before the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<&'a str>,
}

/// One skill a session found, as its `initialize` answer names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInfo<'a> {
    /// The skill's name.
    pub name: &'a str,
    /// The absolute path of the skill's `SKILL.md`.
    pub path: &'a str,
    /// The folder it was found in: `global` or `project`.
    pub scope: &'a str,
}

/// What the `settings` notice tells every client: the model the session took,
/// its context window, the reasoning level and the permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SettingsNotice<'a> {
    /// The provider and model text the run uses, as `initialize` names it.
    pub model: &'a str,
    /// The context window of the model, when it is known.
    pub context_window: Option<u64>,
    /// The reasoning level the session sends, when it sends one.
    pub level: Option<&'a str>,
    /// The permission mode the session decides with.
    pub mode: &'a str,
}

/// One command a JS plugin offers, with the line that describes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginCommand<'a> {
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    #[serde(borrow)]
    pub description: Cow<'a, str>,
}

/// The commands the JS plugins of a session offer, sent once to a client after
/// it attaches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginCommands<'a> {
    #[serde(borrow)]
    pub commands: Vec<PluginCommand<'a>>,
}

/// One panel of a JS plugin: its id, its title and its lines. A panel with an
/// empty `lines` is hidden.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginPanel<'a> {
    #[serde(borrow)]
    pub id: Cow<'a, str>,
    #[serde(borrow)]
    pub title: Cow<'a, str>,
    #[serde(borrow)]
    pub lines: Vec<PanelLine<'a>>,
}

/// One line of a plugin panel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PanelLine<'a> {
    /// A plain line.
    Plain(#[serde(borrow)] Cow<'a, str>),
    /// A line in a role: text, dim, strong, accent, good, warn or bad. The
    /// client draws an unknown or missing role as text.
    Text {
        #[serde(borrow)]
        text: Cow<'a, str>,
        #[serde(default, borrow)]
        role: Option<Cow<'a, str>>,
    },
    /// A key and its value, such as `tests 12`.
    Row {
        #[serde(borrow)]
        key: Cow<'a, str>,
        #[serde(borrow)]
        value: Cow<'a, str>,
    },
    /// A labelled progress bar, `value` of `max`.
    Progress {
        #[serde(borrow)]
        label: Cow<'a, str>,
        value: u64,
        max: u64,
    },
}

/// A note to show in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Note<'a> {
    #[serde(borrow)]
    pub text: Cow<'a, str>,
}

/// A JS plugin command a client asked for is done: its name, and whether it
/// queued a message for the session, so a run follows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommandEnd<'a> {
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    pub sent: bool,
}

/// The worker's answer to `attach`.
///
/// `committed` is the newest record the journal committed when it attached, and
/// `running` says whether a run is open at that moment. The `run_end` of a run
/// that ended before `committed` is in no frame the client catches up on, so the
/// answer is what tells a client that reattached that the run is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AttachResult {
    pub committed: u64,
    pub running: bool,
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    method: Cow<'a, str>,
    id: Option<u64>,
    #[serde(borrow)]
    jsonrpc: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Params<P> {
    params: P,
}

/// A string that borrows from the line when it has no escape. serde borrows a
/// bare `Cow<str>` field only, not one inside an `Option` or a `Vec`.
#[derive(Deserialize)]
#[serde(transparent)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

fn texts(found: Option<Text<'_>>) -> Option<Cow<'_, str>> {
    found.map(|text| text.0)
}

#[derive(Deserialize)]
struct InitializeParams<'a> {
    major: u32,
    #[serde(borrow)]
    client: ClientInfo<'a>,
    #[serde(borrow, default)]
    capabilities: Vec<Text<'a>>,
    #[serde(borrow)]
    token: Cow<'a, str>,
}

#[derive(Deserialize)]
struct AttachParams {
    from: u64,
}

#[derive(Deserialize)]
struct SendParams<'a> {
    #[serde(borrow)]
    text: Cow<'a, str>,
    #[serde(default)]
    mode: Mode,
}

#[derive(Deserialize)]
struct ApproveParams<'a> {
    id: u64,
    answer: AnswerKind,
    #[serde(borrow, default)]
    reason: Option<Text<'a>>,
}

/// The picks of each question of an `answer` request, one list per question.
type Picks<'a> = Vec<Vec<Text<'a>>>;

#[derive(Deserialize)]
struct AnswerParams<'a> {
    id: u64,
    /// The picks of each question, `Some(None)` for a skip; the outer `None`
    /// stands for the field a client left out.
    #[serde(borrow, default, deserialize_with = "picks")]
    answers: Option<Option<Picks<'a>>>,
}

/// Reads the `answers` of an `answer` request: the field is there, so `null`
/// is a skip and a list is one list of labels per question.
fn picks<'de, D>(deserializer: D) -> Result<Option<Option<Picks<'de>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Picks<'de>>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct SetParams<'a> {
    #[serde(borrow, default)]
    model: Option<Text<'a>>,
    #[serde(borrow, default)]
    level: Option<Text<'a>>,
    #[serde(borrow, default)]
    mode: Option<Text<'a>>,
}

#[derive(Deserialize)]
struct CompactParams<'a> {
    #[serde(borrow, default)]
    note: Option<Text<'a>>,
}

#[derive(Deserialize)]
struct CommandParams<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    #[serde(borrow, default)]
    args: Cow<'a, str>,
}

fn params<'a, P: Deserialize<'a>>(line: &'a [u8]) -> Result<P, RpcError> {
    match serde_json::from_slice::<Params<P>>(line) {
        Ok(found) => Ok(found.params),
        Err(err) => Err(RpcError::invalid_params(err.to_string())),
    }
}

/// Decodes one line from a client. The parameters borrow from `line`.
pub fn decode(line: &[u8]) -> Result<(Option<u64>, Request<'_>), RpcError> {
    let head: Head<'_> = serde_json::from_slice(line).map_err(|err| match err.classify() {
        Category::Syntax | Category::Eof => RpcError::parse_error(),
        Category::Data | Category::Io => RpcError::invalid_request(),
    })?;
    if head.jsonrpc.as_deref().is_some_and(|seen| seen != JSONRPC) {
        return Err(RpcError::invalid_request());
    }
    let request = match head.method.as_ref() {
        "initialize" => {
            let found: InitializeParams<'_> = params(line)?;
            Request::Initialize {
                major: found.major,
                client: found.client,
                capabilities: found.capabilities.into_iter().map(|text| text.0).collect(),
                token: found.token,
            }
        }
        "attach" => {
            let found: AttachParams = params(line)?;
            Request::Attach { from: found.from }
        }
        "send" => {
            let found: SendParams<'_> = params(line)?;
            Request::Send {
                text: found.text,
                mode: found.mode,
            }
        }
        "command" => {
            let found: CommandParams<'_> = params(line)?;
            Request::Command {
                name: found.name,
                args: found.args,
            }
        }
        "approve" => {
            let found: ApproveParams<'_> = params(line)?;
            Request::Approve {
                id: found.id,
                answer: found.answer,
                reason: texts(found.reason),
            }
        }
        "cancel" => Request::Cancel,
        "answer" => {
            let found: AnswerParams<'_> = params(line)?;
            let answers = found
                .answers
                .ok_or_else(|| RpcError::invalid_params("answer without answers".to_string()))?;
            Request::Answer {
                id: found.id,
                answers: answers.map(|per| {
                    per.into_iter()
                        .map(|picks| picks.into_iter().map(|pick| pick.0).collect())
                        .collect()
                }),
            }
        }
        "detach" => Request::Detach,
        "set" => {
            let found: SetParams<'_> = params(line)?;
            Request::Set {
                model: texts(found.model),
                level: texts(found.level),
                mode: texts(found.mode),
            }
        }
        "compact" => {
            let found: CompactParams<'_> = params(line)?;
            Request::Compact {
                note: texts(found.note),
            }
        }
        _ => return Err(RpcError::method_not_found()),
    };
    Ok((head.id, request))
}

#[derive(Serialize)]
struct ResultEnvelope<'a, T> {
    jsonrpc: &'static str,
    id: u64,
    result: &'a T,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    jsonrpc: &'static str,
    id: Option<u64>,
    error: &'a RpcError,
}

/// Writes `value` as JSON. A `Serialize` impl that fails leaves `buf` unchanged.
fn push_json(buf: &mut Vec<u8>, value: &impl Serialize) -> bool {
    let start = buf.len();
    if serde_json::to_writer(&mut *buf, value).is_ok() {
        return true;
    }
    buf.truncate(start);
    false
}

/// Writes a result line for `id` into `buf`.
pub fn result(buf: &mut Vec<u8>, id: u64, body: &impl Serialize) {
    let line = ResultEnvelope {
        jsonrpc: JSONRPC,
        id,
        result: body,
    };
    if push_json(buf, &line) {
        buf.push(b'\n');
    }
}

/// Writes an error line into `buf`.
pub fn error(buf: &mut Vec<u8>, id: Option<u64>, err: &RpcError) {
    let line = ErrorEnvelope {
        jsonrpc: JSONRPC,
        id,
        error: err,
    };
    if push_json(buf, &line) {
        buf.push(b'\n');
    }
}

/// Writes the `settings` notice line into `buf`.
///
/// The notice goes to every client of a session, so it is written once and
/// shared as one frame.
pub fn settings(buf: &mut Vec<u8>, settings: &SettingsNotice<'_>) {
    let start = buf.len();
    buf.extend_from_slice(br#"{"jsonrpc":"2.0","method":""#);
    buf.extend_from_slice(methods::SETTINGS.as_bytes());
    buf.extend_from_slice(br#"","params":"#);
    if !push_json(buf, settings) {
        buf.truncate(start);
        return;
    }
    buf.extend_from_slice(b"}\n");
}

/// Writes a notice line whose `params` are `value`, under `method`.
fn notice(buf: &mut Vec<u8>, method: &str, value: &impl Serialize) {
    let start = buf.len();
    buf.extend_from_slice(br#"{"jsonrpc":"2.0","method":""#);
    buf.extend_from_slice(method.as_bytes());
    buf.extend_from_slice(br#"","params":"#);
    if !push_json(buf, value) {
        buf.truncate(start);
        return;
    }
    buf.extend_from_slice(b"}\n");
}

/// Writes the `plugin_commands` notice line into `buf`.
pub fn plugin_commands(buf: &mut Vec<u8>, commands: &PluginCommands<'_>) {
    notice(buf, methods::PLUGIN_COMMANDS, commands);
}

/// Writes the `plugin_panel` notice line into `buf`.
pub fn plugin_panel(buf: &mut Vec<u8>, panel: &PluginPanel<'_>) {
    notice(buf, methods::PLUGIN_PANEL, panel);
}

/// Writes the `note` notice line into `buf`.
pub fn note(buf: &mut Vec<u8>, text: &str) {
    notice(
        buf,
        methods::NOTE,
        &Note {
            text: Cow::Borrowed(text),
        },
    );
}

/// Writes the `command_end` notice line into `buf`.
pub fn command_end(buf: &mut Vec<u8>, name: &str, sent: bool) {
    notice(
        buf,
        methods::COMMAND_END,
        &CommandEnd {
            name: Cow::Borrowed(name),
            sent,
        },
    );
}

/// Decodes the params of a `plugin_commands` notice. They borrow from `params`.
pub fn decode_plugin_commands(params: &[u8]) -> Option<PluginCommands<'_>> {
    serde_json::from_slice(params).ok()
}

/// Decodes the params of a `plugin_panel` notice. They borrow from `params`.
pub fn decode_plugin_panel(params: &[u8]) -> Option<PluginPanel<'_>> {
    serde_json::from_slice(params).ok()
}

/// Decodes the params of a `note` notice. The text borrows from `params`.
pub fn decode_note(params: &[u8]) -> Option<Note<'_>> {
    serde_json::from_slice(params).ok()
}

/// Writes a notification line into `buf`, copying `params` once as given.
pub fn notify(buf: &mut Vec<u8>, method: &str, params: &[u8]) {
    let start = buf.len();
    buf.extend_from_slice(br#"{"jsonrpc":"2.0","method":"#);
    if serde_json::to_writer(&mut *buf, method).is_err() {
        buf.truncate(start);
        return;
    }
    buf.extend_from_slice(br#","params":"#);
    buf.extend_from_slice(params);
    buf.extend_from_slice(b"}\n");
}

/// Writes a request line into `buf`, copying `params` once as given.
///
/// A client builds its `initialize`, its `attach` and the rest with it, so one
/// line of a client is written here and not by hand.
pub fn request(buf: &mut Vec<u8>, id: u64, method: &str, params: &[u8]) {
    let start = buf.len();
    buf.extend_from_slice(br#"{"jsonrpc":"2.0","id":"#);
    if serde_json::to_writer(&mut *buf, &id).is_err() {
        buf.truncate(start);
        return;
    }
    buf.extend_from_slice(br#","method":"#);
    if serde_json::to_writer(&mut *buf, method).is_err() {
        buf.truncate(start);
        return;
    }
    buf.extend_from_slice(br#","params":"#);
    buf.extend_from_slice(params);
    buf.extend_from_slice(b"}\n");
}

/// The number of questions of the `question_request` line `line`, or `None`
/// when `line` is not one.
///
/// A worker reads it from the frame of the request that waits to tell an
/// `answer` whose lists do not match the questions it asks.
pub fn question_count(line: &[u8]) -> Option<usize> {
    #[derive(Deserialize)]
    struct Questions {
        #[serde(default)]
        questions: Vec<serde::de::IgnoredAny>,
    }
    #[derive(Deserialize)]
    struct Line<'a> {
        #[serde(borrow)]
        method: Cow<'a, str>,
        params: Questions,
    }
    let decoded: Line<'_> = serde_json::from_slice(line).ok()?;
    (decoded.method == methods::QUESTION_REQUEST).then_some(decoded.params.questions.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_ok(line: &[u8]) -> (Option<u64>, Request<'_>) {
        decode(line).expect("decodes")
    }

    fn text(buf: &[u8]) -> &str {
        std::str::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn every_request_decodes() {
        let (_, request) = decode_ok(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"major":1,"client":{"name":"tui","version":"0.1.0"},"capabilities":["event","approve"],"token":"ab12"}}"#,
        );
        assert_eq!(
            request,
            Request::Initialize {
                major: 1,
                client: ClientInfo {
                    name: Cow::Borrowed("tui"),
                    version: Cow::Borrowed("0.1.0"),
                },
                capabilities: vec![Cow::Borrowed("event"), Cow::Borrowed("approve")],
                token: Cow::Borrowed("ab12"),
            }
        );

        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":2,"method":"attach","params":{"from":42}}"#),
            (Some(2), Request::Attach { from: 42 })
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":3,"method":"send","params":{"text":"hi","mode":"queue"}}"#
            ),
            (
                Some(3),
                Request::Send {
                    text: Cow::Borrowed("hi"),
                    mode: Mode::Queue,
                }
            )
        );
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":4,"method":"send","params":{"text":"hi"}}"#),
            (
                Some(4),
                Request::Send {
                    text: Cow::Borrowed("hi"),
                    mode: Mode::Steer,
                }
            )
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":5,"method":"approve","params":{"id":9,"answer":"session","reason":"a rule"}}"#
            ),
            (
                Some(5),
                Request::Approve {
                    id: 9,
                    answer: AnswerKind::Session,
                    reason: Some(Cow::Borrowed("a rule")),
                }
            )
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":6,"method":"approve","params":{"id":9,"answer":"skip"}}"#
            ),
            (
                Some(6),
                Request::Approve {
                    id: 9,
                    answer: AnswerKind::Skip,
                    reason: None,
                }
            )
        );
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":7,"method":"cancel"}"#),
            (Some(7), Request::Cancel)
        );
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":8,"method":"detach"}"#),
            (Some(8), Request::Detach)
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":9,"method":"set","params":{"model":"m","level":"high","mode":"bypass"}}"#
            ),
            (
                Some(9),
                Request::Set {
                    model: Some(Cow::Borrowed("m")),
                    level: Some(Cow::Borrowed("high")),
                    mode: Some(Cow::Borrowed("bypass")),
                }
            )
        );
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":10,"method":"set","params":{"level":"low"}}"#),
            (
                Some(10),
                Request::Set {
                    model: None,
                    level: Some(Cow::Borrowed("low")),
                    mode: None,
                }
            )
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":11,"method":"compact","params":{"note":"keep the API notes"}}"#
            ),
            (
                Some(11),
                Request::Compact {
                    note: Some(Cow::Borrowed("keep the API notes")),
                }
            )
        );
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":12,"method":"compact","params":{}}"#),
            (Some(12), Request::Compact { note: None })
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":13,"method":"command","params":{"name":"deploy","args":"staging now"}}"#
            ),
            (
                Some(13),
                Request::Command {
                    name: Cow::Borrowed("deploy"),
                    args: Cow::Borrowed("staging now"),
                }
            )
        );
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":14,"method":"command","params":{"name":"deploy"}}"#
            ),
            (
                Some(14),
                Request::Command {
                    name: Cow::Borrowed("deploy"),
                    args: Cow::Borrowed(""),
                }
            )
        );
    }

    #[test]
    fn text_with_escapes_decodes() {
        let (_, request) =
            decode_ok(br#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"a\nb\"c"}}"#);
        match request {
            Request::Send { text, .. } => {
                assert_eq!(text, "a\nb\"c");
                assert!(matches!(text, Cow::Owned(_)), "an escape needs a copy");
            }
            other => panic!("{other:?}"),
        }
    }

    fn borrowed_from(text: &str, line: &[u8]) -> bool {
        let (at, start) = (text.as_ptr() as usize, line.as_ptr() as usize);
        (start..start + line.len()).contains(&at)
    }

    #[test]
    fn optional_and_listed_strings_borrow_from_the_line() {
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"approve","params":{"id":3,"answer":"refuse","reason":"no"}}"#;
        match decode(line).expect("decodes").1 {
            Request::Approve {
                reason: Some(reason),
                ..
            } => assert!(borrowed_from(&reason, line)),
            other => panic!("{other:?}"),
        }
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"set","params":{"model":"m","level":"high","mode":"auto"}}"#;
        match decode(line).expect("decodes").1 {
            Request::Set {
                model: Some(model),
                level: Some(level),
                mode: Some(mode),
            } => {
                assert!(borrowed_from(&model, line));
                assert!(borrowed_from(&level, line));
                assert!(borrowed_from(&mode, line));
            }
            other => panic!("{other:?}"),
        }
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"major":1,"client":{"name":"t","version":"1"},"capabilities":["images"],"token":"ab"}}"#;
        match decode(line).expect("decodes").1 {
            Request::Initialize { capabilities, .. } => {
                assert!(borrowed_from(&capabilities[0], line))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn text_without_escapes_borrows_from_the_line() {
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":2,"method":"send","params":{"text":"hello"}}"#;
        let (_, request) = decode(line).expect("decodes");
        match request {
            Request::Send { text, .. } => {
                assert!(matches!(text, Cow::Borrowed(_)), "no escape, no copy");
                let at = text.as_ptr() as usize;
                let start = line.as_ptr() as usize;
                assert!(
                    (start..start + line.len()).contains(&at),
                    "points into the line"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":1,"method":"cancel","result":null,"params":{"x":true},"future":[]}"#
            ),
            (Some(1), Request::Cancel)
        );
    }

    #[test]
    fn a_notification_decodes_with_no_id() {
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","method":"detach"}"#),
            (None, Request::Detach)
        );
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let err = decode(br#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn bad_json_is_a_parse_error() {
        let err = decode(br#"{"id":"#).expect_err("fails");
        assert_eq!(err.code, RpcError::PARSE_ERROR);
        let err = decode(b"").expect_err("fails");
        assert_eq!(err.code, RpcError::PARSE_ERROR);
    }

    #[test]
    fn a_bad_envelope_is_an_invalid_request() {
        let err = decode(br#"{"jsonrpc":"2.0","id":1}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_REQUEST);
        let err = decode(br#"{"jsonrpc":"1.0","id":1,"method":"cancel"}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_REQUEST);
    }

    #[test]
    fn a_missing_field_is_invalid_params() {
        let err =
            decode(br#"{"jsonrpc":"2.0","id":1,"method":"send","params":{}}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        let err = decode(br#"{"jsonrpc":"2.0","id":1,"method":"send"}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    #[test]
    fn ask_answer_carries_the_picks_of_each_question() {
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":13,"method":"answer","params":{"id":4,"answers":[["sqlite"],["a","b"]]}}"#;
        match decode(line).expect("decodes").1 {
            Request::Answer { id, answers } => {
                assert_eq!(id, 4);
                let answers = answers.expect("an answer");
                assert_eq!(answers.len(), 2);
                assert_eq!(answers[0], [Cow::Borrowed("sqlite")]);
                assert_eq!(answers[1], [Cow::Borrowed("a"), Cow::Borrowed("b")]);
                assert!(borrowed_from(&answers[0][0], line));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decode_ok(
                br#"{"jsonrpc":"2.0","id":14,"method":"answer","params":{"id":4,"answers":null}}"#
            ),
            (
                Some(14),
                Request::Answer {
                    id: 4,
                    answers: None
                }
            )
        );
    }

    #[test]
    fn ask_a_bad_answers_shape_is_invalid_params() {
        let err = decode(
            br#"{"jsonrpc":"2.0","id":1,"method":"answer","params":{"id":4,"answers":"sqlite"}}"#,
        )
        .expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        let err = decode(br#"{"jsonrpc":"2.0","id":1,"method":"answer","params":{"id":4}}"#)
            .expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        let err = decode(
            br#"{"jsonrpc":"2.0","id":1,"method":"answer","params":{"id":4,"answers":[[1]]}}"#,
        )
        .expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    #[test]
    fn notify_writes_exact_bytes() {
        let mut buf = Vec::new();
        notify(&mut buf, methods::EVENT, br#"{"b":1,"a":2}"#);
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{\"b\":1,\"a\":2}}\n"
        );
    }

    #[test]
    fn result_writes_exact_bytes() {
        #[derive(Serialize)]
        struct Body {
            ok: bool,
        }
        let mut buf = Vec::new();
        result(&mut buf, 7, &Body { ok: true });
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n"
        );
    }

    #[test]
    fn the_attach_answer_carries_the_record_and_whether_a_run_is_open() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            3,
            &AttachResult {
                committed: 42,
                running: true,
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"committed\":42,\"running\":true}}\n"
        );
        let mut buf = Vec::new();
        result(
            &mut buf,
            4,
            &AttachResult {
                committed: 0,
                running: false,
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"committed\":0,\"running\":false}}\n"
        );
    }

    #[test]
    fn the_initialize_answer_names_the_model_and_its_window() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            1,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 2,
                capabilities: &[],
                committed: 9,
                model: "fi/m",
                context_window: Some(200_000),
                level: None,
                skills: &[],
                mode: None,
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"major\":1,\"version\":\"0.1.0\",\
             \"session\":\"s1\",\"client\":2,\"capabilities\":[],\"committed\":9,\
             \"model\":\"fi/m\",\"context_window\":200000,\"level\":null}}\n"
        );
        let mut buf = Vec::new();
        result(
            &mut buf,
            3,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 1,
                capabilities: &[],
                committed: 0,
                model: "m",
                context_window: None,
                level: None,
                skills: &[],
                mode: None,
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"major\":1,\"version\":\"0.1.0\",\
             \"session\":\"s1\",\"client\":1,\"capabilities\":[],\"committed\":0,\
             \"model\":\"m\",\"context_window\":null,\"level\":null}}\n"
        );
    }

    #[test]
    fn the_initialize_answer_names_the_level() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            1,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 1,
                capabilities: &[],
                committed: 0,
                model: "m",
                context_window: None,
                level: Some("high"),
                skills: &[],
                mode: None,
            },
        );
        assert!(text(&buf).contains("\"level\":\"high\""), "{}", text(&buf));
    }

    #[test]
    fn the_command_end_notice_names_the_command_and_whether_it_sent() {
        let mut buf = Vec::new();
        command_end(&mut buf, "deploy", true);
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"command_end\",\"params\":\
             {\"name\":\"deploy\",\"sent\":true}}\n"
        );
    }

    #[test]
    fn the_initialize_answer_names_the_mode_in_force() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            1,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 1,
                capabilities: &[],
                committed: 0,
                model: "m",
                context_window: None,
                level: None,
                skills: &[],
                mode: Some("auto"),
            },
        );
        assert!(text(&buf).contains("\"mode\":\"auto\""), "{}", text(&buf));
    }

    #[test]
    fn the_initialize_answer_names_the_skills_of_the_session() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            5,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 1,
                capabilities: &[],
                committed: 0,
                model: "m",
                context_window: None,
                level: None,
                skills: &[SkillInfo {
                    name: "demo",
                    path: "/w/.agents/skills/demo/SKILL.md",
                    scope: "project",
                }],
                mode: None,
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"major\":1,\"version\":\"0.1.0\",\
             \"session\":\"s1\",\"client\":1,\"capabilities\":[],\"committed\":0,\
             \"model\":\"m\",\"context_window\":null,\"level\":null,\"skills\":\
             [{\"name\":\"demo\",\"path\":\"/w/.agents/skills/demo/SKILL.md\",\
             \"scope\":\"project\"}]}}\n"
        );
    }

    #[test]
    fn an_initialize_answer_with_no_skills_names_no_skills_key() {
        let mut buf = Vec::new();
        result(
            &mut buf,
            6,
            &InitializeResult {
                major: 1,
                version: "0.1.0",
                session: "s1",
                client: 1,
                capabilities: &[],
                committed: 0,
                model: "m",
                context_window: None,
                level: None,
                skills: &[],
                mode: None,
            },
        );
        let line = text(&buf);
        assert!(!line.contains("skills"), "{line}");
        assert_eq!(
            line,
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{\"major\":1,\"version\":\"0.1.0\",\
             \"session\":\"s1\",\"client\":1,\"capabilities\":[],\"committed\":0,\
             \"model\":\"m\",\"context_window\":null,\"level\":null}}\n"
        );
    }

    #[test]
    fn the_settings_notice_names_the_model_its_window_the_level_and_the_mode() {
        let mut buf = Vec::new();
        settings(
            &mut buf,
            &SettingsNotice {
                model: "m2",
                context_window: Some(8_000),
                level: Some("high"),
                mode: "auto",
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"settings\",\"params\":\
             {\"model\":\"m2\",\"context_window\":8000,\"level\":\"high\",\"mode\":\"auto\"}}\n"
        );
        let mut buf = Vec::new();
        settings(
            &mut buf,
            &SettingsNotice {
                model: "m3",
                context_window: None,
                level: None,
                mode: "default",
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"settings\",\"params\":\
             {\"model\":\"m3\",\"context_window\":null,\"level\":null,\"mode\":\"default\"}}\n"
        );
    }

    #[test]
    fn error_writes_exact_bytes() {
        let mut buf = Vec::new();
        error(&mut buf, Some(4), &RpcError::wrong_token());
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":-32000,\"message\":\"wrong token\"}}\n"
        );
        let mut buf = Vec::new();
        error(&mut buf, None, &RpcError::parse_error());
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"parse error\"}}\n"
        );
    }

    /// The params bytes of a written line, which the notice decoders take.
    fn params_of(line: &[u8]) -> &[u8] {
        let marker = br#""params":"#;
        let at = line
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("params")
            + marker.len();
        &line[at..line.len() - 2]
    }

    #[test]
    fn a_plugin_command_request_decodes() {
        let line: &[u8] = br#"{"jsonrpc":"2.0","id":5,"method":"command","params":{"name":"deploy","args":"staging now"}}"#;
        match decode(line).expect("decodes").1 {
            Request::Command { name, args } => {
                assert_eq!(name, "deploy");
                assert_eq!(args, "staging now");
                assert!(borrowed_from(&name, line), "no escape, no copy");
                assert!(borrowed_from(&args, line), "no escape, no copy");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decode_ok(br#"{"jsonrpc":"2.0","id":6,"method":"command","params":{"name":"deploy"}}"#),
            (
                Some(6),
                Request::Command {
                    name: Cow::Borrowed("deploy"),
                    args: Cow::Borrowed(""),
                }
            )
        );
    }

    #[test]
    fn a_plugin_command_request_without_a_name_is_invalid() {
        let err =
            decode(br#"{"jsonrpc":"2.0","id":1,"method":"command","params":{"args":"staging"}}"#)
                .expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        let err = decode(br#"{"jsonrpc":"2.0","id":1,"method":"command"}"#).expect_err("fails");
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    #[test]
    fn plugin_commands_round_trip() {
        let commands = PluginCommands {
            commands: vec![
                PluginCommand {
                    name: Cow::Borrowed("deploy"),
                    description: Cow::Borrowed("ship it"),
                },
                PluginCommand {
                    name: Cow::Borrowed("test"),
                    description: Cow::Borrowed(""),
                },
            ],
        };
        let mut buf = Vec::new();
        plugin_commands(&mut buf, &commands);
        let decoded = decode_plugin_commands(params_of(&buf)).expect("decodes");
        assert_eq!(decoded, commands);
        assert!(
            borrowed_from(&decoded.commands[0].name, &buf),
            "no escape, no copy"
        );
    }

    #[test]
    fn plugin_panel_round_trip() {
        let panel = PluginPanel {
            id: Cow::Borrowed("build"),
            title: Cow::Borrowed("Build"),
            lines: vec![
                PanelLine::Plain(Cow::Borrowed("all green")),
                PanelLine::Text {
                    text: Cow::Borrowed("waiting"),
                    role: Some(Cow::Borrowed("warn")),
                },
                PanelLine::Row {
                    key: Cow::Borrowed("tests"),
                    value: Cow::Borrowed("12"),
                },
                PanelLine::Progress {
                    label: Cow::Borrowed("pack"),
                    value: 3,
                    max: 4,
                },
            ],
        };
        let mut buf = Vec::new();
        plugin_panel(&mut buf, &panel);
        assert_eq!(decode_plugin_panel(params_of(&buf)), Some(panel));

        let hidden = PluginPanel {
            id: Cow::Borrowed("build"),
            title: Cow::Borrowed("Build"),
            lines: Vec::new(),
        };
        let mut buf = Vec::new();
        plugin_panel(&mut buf, &hidden);
        assert_eq!(decode_plugin_panel(params_of(&buf)), Some(hidden));
    }

    #[test]
    fn plugin_note_round_trip() {
        let mut buf = Vec::new();
        note(&mut buf, "deployed to staging");
        let decoded = decode_note(params_of(&buf)).expect("decodes");
        assert_eq!(decoded.text, "deployed to staging");
        assert!(borrowed_from(&decoded.text, &buf), "no escape, no copy");
    }

    #[test]
    fn plugin_panel_lines_decode_to_their_shape() {
        fn line(json: &str) -> PanelLine<'_> {
            serde_json::from_str(json).expect("decodes")
        }
        assert_eq!(line(r#""x""#), PanelLine::Plain(Cow::Borrowed("x")));
        assert_eq!(
            line(r#"{"text":"x"}"#),
            PanelLine::Text {
                text: Cow::Borrowed("x"),
                role: None,
            }
        );
        assert_eq!(
            line(r#"{"text":"x","role":"bad"}"#),
            PanelLine::Text {
                text: Cow::Borrowed("x"),
                role: Some(Cow::Borrowed("bad")),
            }
        );
        assert_eq!(
            line(r#"{"key":"a","value":"b"}"#),
            PanelLine::Row {
                key: Cow::Borrowed("a"),
                value: Cow::Borrowed("b"),
            }
        );
        assert_eq!(
            line(r#"{"label":"l","value":3,"max":4}"#),
            PanelLine::Progress {
                label: Cow::Borrowed("l"),
                value: 3,
                max: 4,
            }
        );
        assert!(
            serde_json::from_str::<PanelLine<'_>>(r#"{"value":1}"#).is_err(),
            "no shape takes a bare value"
        );

        // The same shapes inside a panel, through its decoder.
        let params = br#"{"id":"build","title":"Build","lines":["x",{"text":"y","role":"good"},{"key":"a","value":"b"},{"label":"l","value":1,"max":2}]}"#;
        let panel = decode_plugin_panel(params).expect("decodes");
        assert_eq!(panel.lines.len(), 4);
        assert!(matches!(panel.lines[0], PanelLine::Plain(_)));
        assert!(matches!(panel.lines[1], PanelLine::Text { .. }));
        assert!(matches!(panel.lines[2], PanelLine::Row { .. }));
        assert!(matches!(panel.lines[3], PanelLine::Progress { .. }));
        assert!(decode_plugin_panel(br#"{"id":"b","title":"B","lines":[1]}"#).is_none());
    }

    #[test]
    fn plugin_notices_write_exact_bytes() {
        let mut buf = Vec::new();
        plugin_commands(
            &mut buf,
            &PluginCommands {
                commands: vec![PluginCommand {
                    name: Cow::Borrowed("deploy"),
                    description: Cow::Borrowed("ship it"),
                }],
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"plugin_commands\",\"params\":\
             {\"commands\":[{\"name\":\"deploy\",\"description\":\"ship it\"}]}}\n"
        );
        let mut buf = Vec::new();
        plugin_panel(
            &mut buf,
            &PluginPanel {
                id: Cow::Borrowed("build"),
                title: Cow::Borrowed("Build"),
                lines: vec![PanelLine::Plain(Cow::Borrowed("ok"))],
            },
        );
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"plugin_panel\",\"params\":\
             {\"id\":\"build\",\"title\":\"Build\",\"lines\":[\"ok\"]}}\n"
        );
        let mut buf = Vec::new();
        note(&mut buf, "hi");
        assert_eq!(
            text(&buf),
            "{\"jsonrpc\":\"2.0\",\"method\":\"note\",\"params\":{\"text\":\"hi\"}}\n"
        );
    }
}

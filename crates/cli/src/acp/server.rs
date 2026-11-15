//! The agent itself: the loop that reads the editor's requests from stdin and
//! answers on stdout.
//!
//! Each session of the editor is one reagent worker, which this process joins as
//! a socket client. The loop below owns stdin and the map of sessions; each
//! session's task owns its connection to its worker.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use reagent_protocol::discovery;
use reagent_protocol::lines::Lines;
use reagent_store::paths;
use reagent_tui::client::Client;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

use super::editor::Editor;
use super::message;
use super::session;
use super::session::Command;
use super::session::Session;

/// How many requests of the agent one session holds before it reads the next.
const QUEUE: usize = 16;

/// How long the lines still queued get to reach stdout when the editor goes.
const LEAVE: std::time::Duration = std::time::Duration::from_millis(100);

/// The protocol version this agent speaks, which is the only one it speaks.
const VERSION: u32 = 1;

/// Runs the agent until the editor closes its stdin.
pub(super) async fn run(argv: Vec<OsString>) -> ExitCode {
    let Some(state) = crate::session::state_home() else {
        eprintln!("reagent: no state folder, so no session can start");
        return ExitCode::from(2);
    };
    let (editor, lines) = Editor::new();
    let writing = tokio::spawn(Editor::write(lines));
    let mut agent = Agent {
        state,
        words: argv,
        editor,
        sessions: HashMap::new(),
        name: String::from("editor"),
        initialized: false,
    };
    agent.read().await;
    drop(agent);
    let _ = tokio::time::timeout(LEAVE, writing).await;
    ExitCode::SUCCESS
}

/// The agent, its sessions and the editor on the other end.
struct Agent {
    /// The state folder the workers write their entries under.
    state: PathBuf,
    /// The words this process was given, without the `acp` word.
    words: Vec<OsString>,
    /// The editor on the other end.
    editor: Editor,
    /// The sessions started so far, each with the channel of its task.
    sessions: HashMap<String, Sender<Command>>,
    /// The name the editor gave itself.
    name: String,
    /// Whether `initialize` arrived.
    initialized: bool,
}

impl Agent {
    /// Reads the lines of stdin and takes each one, until the editor closes it.
    async fn read(&mut self) {
        let mut input = tokio::io::stdin();
        let mut lines = Lines::new();
        loop {
            match lines.fill(&mut input).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            while let Some(line) = lines.next() {
                if let Ok(line) = line {
                    self.take(line).await;
                }
            }
        }
    }

    /// Takes one line of the editor.
    async fn take(&mut self, line: &[u8]) {
        let Ok(line) = serde_json::from_slice::<message::Line>(line) else {
            self.editor
                .fail(Value::Null, -32700, "the line is not JSON", None)
                .await;
            return;
        };
        match (line.method, line.id) {
            (Some(method), Some(id)) => self.request(&method, id, line.params).await,
            (Some(method), None) => self.notify(&method, line.params).await,
            (None, Some(id)) => self.answered(id, line.result, line.error),
            (None, None) => {}
        }
    }

    /// Takes one request of the editor.
    async fn request(&mut self, method: &str, id: Value, params: Option<Value>) {
        if !self.initialized && method != "initialize" {
            self.editor
                .fail(id, -32600, "the connection is not initialized", None)
                .await;
            return;
        }
        match method {
            "initialize" => self.initialize(id, params).await,
            "authenticate" => self.editor.respond(id, json!({})).await,
            "session/new" => self.new_session(id, params).await,
            "session/load" => self.load_session(id, params).await,
            "session/prompt" => self.prompt(id, params).await,
            _ => {
                self.editor.fail(id, -32601, "no such method", None).await;
            }
        }
    }

    /// Takes one notification of the editor.
    async fn notify(&mut self, method: &str, params: Option<Value>) {
        if method != "session/cancel" {
            return;
        }
        let Some(session) =
            params.and_then(|params| params["sessionId"].as_str().map(str::to_string))
        else {
            return;
        };
        if let Some(commands) = self.sessions.get(&session) {
            let _ = commands.send(Command::Cancel).await;
        }
    }

    /// Routes the answer of one of this agent's requests to the task that waits
    /// for it.
    fn answered(&mut self, id: Value, result: Option<Value>, error: Option<Value>) {
        let Some(id) = id.as_u64() else {
            return;
        };
        let reply = match (result, error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(Value::Null),
        };
        self.editor.route(id, reply);
    }

    /// Answers `initialize` with what this agent does, and takes the name the
    /// editor gave itself for the clients of the sessions it starts.
    async fn initialize(&mut self, id: Value, params: Option<Value>) {
        if let Some(name) = params
            .and_then(|params| serde_json::from_value::<message::Initialize>(params).ok())
            .and_then(|init| init.client_info)
            .and_then(|info| info.name)
            .filter(|name| !name.is_empty())
        {
            self.name = name;
        }
        self.initialized = true;
        self.editor
            .respond(
                id,
                json!({
                    "protocolVersion": VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": true,
                        },
                        "mcpCapabilities": {"http": true, "sse": false},
                    },
                    "agentInfo": {
                        "name": "reagent",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "authMethods": [],
                }),
            )
            .await;
    }

    /// Starts a worker for a new session, joins it and answers its id.
    async fn new_session(&mut self, id: Value, params: Option<Value>) {
        let Some(new) =
            params.and_then(|params| serde_json::from_value::<message::NewSession>(params).ok())
        else {
            self.editor
                .fail(id, -32602, "the params are not a new session", None)
                .await;
            return;
        };
        let Some(cwd) = folder(&new.cwd).await else {
            self.editor
                .fail(id, -32602, "cwd is not an existing folder", None)
                .await;
            return;
        };
        let flags = self.flags(
            crate::tui::new_flags(self.words.iter().cloned()),
            &cwd,
            &new.mcp_servers,
        );
        match crate::tui::start_worker(&self.state, flags).await {
            Ok(entry) => self.join(id, entry, &cwd, false).await,
            Err(error) => self.editor.fail(id, -32603, &error, None).await,
        }
    }

    /// Joins the worker of a session that exists, replays its history and then
    /// answers the request.
    async fn load_session(&mut self, id: Value, params: Option<Value>) {
        let Some(load) =
            params.and_then(|params| serde_json::from_value::<message::LoadSession>(params).ok())
        else {
            self.editor
                .fail(id, -32602, "the params are not a session to load", None)
                .await;
            return;
        };
        let Some(cwd) = folder(&load.cwd).await else {
            self.editor
                .fail(id, -32602, "cwd is not an existing folder", None)
                .await;
            return;
        };
        let live = discovery::list(&discovery::folder(&self.state))
            .await
            .into_iter()
            .find(|entry| entry.session == load.session_id);
        let entry = match live {
            Some(entry) => entry,
            None => {
                if session_dir(&self.state, &cwd, &load.session_id)
                    .await
                    .is_none()
                {
                    self.editor
                        .fail(
                            id,
                            -32002,
                            "no such session",
                            Some(json!({"sessionId": load.session_id})),
                        )
                        .await;
                    return;
                }
                let words = crate::tui::resume_flags(self.words.iter().cloned(), &load.session_id);
                let flags = self.flags(words, &cwd, &load.mcp_servers);
                match crate::tui::start_worker(&self.state, flags).await {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.editor.fail(id, -32603, &error, None).await;
                        return;
                    }
                }
            }
        };
        self.join(id, entry, &cwd, true).await;
    }

    /// Sends the prompt of a session to its task, which answers it when the run
    /// it starts ends.
    async fn prompt(&mut self, id: Value, params: Option<Value>) {
        let Some(prompt) =
            params.and_then(|params| serde_json::from_value::<message::Prompt>(params).ok())
        else {
            self.editor
                .fail(id, -32602, "the params are not a prompt", None)
                .await;
            return;
        };
        let Some(commands) = self.sessions.get(&prompt.session_id).cloned() else {
            self.editor
                .fail(
                    id,
                    -32002,
                    "no such session",
                    Some(json!({"sessionId": prompt.session_id})),
                )
                .await;
            return;
        };
        let Some(text) = text_of(&prompt.prompt) else {
            self.editor
                .fail(
                    id,
                    -32602,
                    "this agent takes text, resource links and text resources only",
                    None,
                )
                .await;
            return;
        };
        let taken = Command::Prompt {
            id: id.clone(),
            text,
        };
        if commands.send(taken).await.is_err() {
            self.editor
                .fail(
                    id,
                    -32003,
                    "the session's worker closed",
                    Some(json!({"sessionId": prompt.session_id})),
                )
                .await;
        }
    }

    /// Connects to `entry` as a client of the session, replays its history when
    /// `load` says so, and answers the request `id`.
    async fn join(&mut self, id: Value, entry: discovery::Entry, cwd: &Path, load: bool) {
        let name = format!("acp:{}", self.name);
        let (mut client, init) = match Client::connect(&entry.socket, &entry.token, &name).await {
            Ok(pair) => pair,
            Err(error) => {
                self.editor
                    .fail(
                        id,
                        -32603,
                        &format!("could not join the worker: {error}"),
                        None,
                    )
                    .await;
                return;
            }
        };
        let attached = match client.attach(init.committed).await {
            Ok(attached) => attached,
            Err(error) => {
                self.editor
                    .fail(id, -32603, &format!("could not attach: {error}"), None)
                    .await;
                return;
            }
        };
        let session = init.session;
        if load
            && let Some(dir) = session_dir(&self.state, cwd, &session).await
            && let Err(error) =
                session::replay(&self.editor, &session, cwd, dir, attached.committed).await
        {
            self.editor
                .fail(
                    id,
                    -32603,
                    &format!("could not read the session: {error}"),
                    None,
                )
                .await;
            return;
        }
        let (commands, requests) = mpsc::channel(QUEUE);
        let task = Session::new(
            session.clone(),
            cwd.to_path_buf(),
            self.editor.clone(),
            commands.clone(),
        );
        tokio::spawn(task.run(client, requests));
        self.sessions.insert(session.clone(), commands);
        let answer = if load {
            json!({})
        } else {
            json!({"sessionId": session})
        };
        self.editor.respond(id, answer).await;
    }

    /// The words of a worker, with the folder of its session and its MCP
    /// servers after them.
    fn flags(&self, mut flags: Vec<OsString>, cwd: &Path, servers: &[Value]) -> Vec<OsString> {
        flags.push(OsString::from("--cwd"));
        flags.push(cwd.as_os_str().to_os_string());
        for server in servers {
            let Ok(text) = serde_json::to_string(server) else {
                continue;
            };
            flags.push(OsString::from("--mcp-spec"));
            flags.push(OsString::from(text));
        }
        flags
    }
}

/// The folder of one session: the one keyed by `cwd`, else any other folder of
/// the state folder that holds it.
///
/// The key of a working directory is the text it was given, and a worker can be
/// given another form of the same folder, so the search covers that case.
async fn session_dir(state: &Path, cwd: &Path, session: &str) -> Option<PathBuf> {
    let keyed = paths::sessions_root(state)
        .join(paths::cwd_key(cwd))
        .join(session);
    if exists(&keyed).await {
        return Some(keyed);
    }
    let mut folders = tokio::fs::read_dir(paths::sessions_root(state))
        .await
        .ok()?;
    while let Ok(Some(entry)) = folders.next_entry().await {
        let folder = entry.path().join(session);
        if exists(&folder).await {
            return Some(folder);
        }
    }
    None
}

/// Whether `path` is there.
async fn exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// The folder of a session, which must be absolute and exist.
async fn folder(given: &str) -> Option<PathBuf> {
    let path = PathBuf::from(given);
    if !path.is_absolute() {
        return None;
    }
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_dir() => Some(path),
        _ => None,
    }
}

/// The message the blocks of a prompt make, and `None` for a block this agent
/// cannot take.
fn text_of(blocks: &[message::Block]) -> Option<String> {
    let mut text = String::new();
    for block in blocks {
        match block {
            message::Block::Text { text: chunk } => text.push_str(chunk),
            message::Block::ResourceLink { uri, name } => {
                separate(&mut text);
                text.push_str(&format!("[{name}]({uri})"));
            }
            message::Block::Resource { resource } => {
                separate(&mut text);
                let body = resource.text.as_ref()?;
                text.push_str(&format!("```{}\n{body}\n```", resource.uri));
            }
            message::Block::Image {} | message::Block::Audio {} | message::Block::Other => {
                return None;
            }
        }
    }
    Some(text)
}

/// Starts the next block of a message on its own line.
fn separate(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

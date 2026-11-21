//! The agent itself: the loop that reads the editor's requests from stdin and
//! answers on stdout.
//!
//! Each session of the editor is one reagent worker, which this process joins as
//! a socket client. The loop below owns stdin and the map of sessions; each
//! session's task owns its connection to its worker.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use reagent_ai::levels::Reasoning;
use reagent_protocol::discovery;
use reagent_protocol::lines::Lines;
use reagent_store::paths;
use reagent_tui::client::Client;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

use super::editor::Cancel;
use super::editor::Editor;
use super::editor::key_of;
use super::message;
use super::session;
use super::session::Command;
use super::session::Config;
use super::session::Session;

/// How many requests of the agent one session holds before it reads the next.
const QUEUE: usize = 16;

/// How long the lines still queued get to reach stdout when the editor goes.
const LEAVE: std::time::Duration = std::time::Duration::from_millis(100);

/// The protocol version this agent speaks, which is the only one it speaks.
const VERSION: u32 = 1;

/// The longest line the editor may send, in bytes: a picture of the largest size
/// a prompt takes, as base64 text, and the JSON around it.
const LINE: usize = 28 * 1024 * 1024;

/// How many sessions one page of `session/list` holds.
const PAGE: usize = 50;

/// How many seconds one day of `session/list` times holds.
const SECONDS_OF_DAY: i64 = 86_400;

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
        notices: false,
        elicits: false,
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
    sessions: HashMap<String, Served>,
    /// The name the editor gave itself.
    name: String,
    /// Whether `initialize` arrived.
    initialized: bool,
    /// Whether the editor advertised the notices of a session.
    notices: bool,
    /// Whether the editor advertised an elicitation form, which a question set
    /// of a session becomes.
    elicits: bool,
}

/// One session this agent serves: the channel of its task, and the folder it
/// runs in, which holds the images of its prompts.
struct Served {
    /// The channel of the task of the session.
    commands: Sender<Command>,
    /// The folder the session runs in.
    cwd: PathBuf,
}

impl Agent {
    /// The channel of the task of `session`, when this agent serves it.
    fn served(&self, session: &str) -> Option<Sender<Command>> {
        self.sessions
            .get(session)
            .map(|served| served.commands.clone())
    }

    /// The folder of the records of `session`, which holds the images of its
    /// prompts: the folder keyed by its `cwd`, and a session with no record yet
    /// is the folder that worker will write its records into.
    async fn records_dir(&self, served: &Served, session: &str) -> PathBuf {
        match session_dir(&self.state, &served.cwd, session).await {
            Some(dir) => dir,
            None => paths::sessions_root(&self.state)
                .join(paths::cwd_key(&served.cwd))
                .join(session),
        }
    }

    /// Reads the lines of stdin and takes each one, until the editor closes it.
    async fn read(&mut self) {
        let mut input = tokio::io::stdin();
        let mut lines = Lines::with_limit(LINE);
        loop {
            match lines.fill(&mut input).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            while let Some(line) = lines.next() {
                match line {
                    Ok(line) => self.take(line).await,
                    // The bytes of the line are gone, and so is its id.
                    Err(error) => {
                        self.editor
                            .fail(
                                Value::Null,
                                -32600,
                                &format!("the request is a {error}"),
                                None,
                            )
                            .await;
                    }
                }
            }
        }
    }

    /// Takes one line of the editor.
    async fn take(&mut self, line: &[u8]) {
        let Ok(head) = serde_json::from_slice::<message::Head>(line) else {
            self.editor
                .fail(Value::Null, -32700, "the line is not JSON", None)
                .await;
            return;
        };
        // A prompt may carry megabytes of base64 text, so its params borrow
        // from the line and only the decoded bytes are new.
        if head.method.as_deref() == Some("session/prompt")
            && let Some(id) = head.id
        {
            self.prompt_line(id, line).await;
            return;
        }
        let Ok(line) = serde_json::from_slice::<message::Line>(line) else {
            self.editor
                .fail(Value::Null, -32700, "the line is not JSON", None)
                .await;
            return;
        };
        match (line.method, line.id) {
            (Some(method), Some(id)) => {
                // A delete answers its real result: it is not marked, so a
                // `$/cancel_request` finds nothing to cancel.
                if method != "session/delete"
                    && let Some(key) = key_of(&id)
                {
                    self.editor.mark(&key);
                }
                self.request(&method, id, line.params).await;
            }
            (Some(method), None) => self.notify(&method, line.params).await,
            (None, Some(id)) => self.answered(id, line.result, line.error),
            (None, None) => {}
        }
    }

    /// Takes a `session/prompt` line, reading its params from the bytes of the
    /// line.
    async fn prompt_line(&mut self, id: Value, line: &[u8]) {
        if let Some(key) = key_of(&id) {
            self.editor.mark(&key);
        }
        if !self.initialized {
            self.editor
                .fail(id, -32600, "the connection is not initialized", None)
                .await;
            return;
        }
        let params = serde_json::from_slice::<message::PromptCall>(line)
            .ok()
            .and_then(|call| call.params);
        self.prompt(id, params).await;
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
            "session/resume" => self.resume_session(id, params).await,
            "session/list" => {
                tokio::spawn(Agent::list_sessions(
                    self.editor.clone(),
                    self.state.clone(),
                    id,
                    params,
                ));
            }
            "session/close" => self.close_session(id, params).await,
            "session/delete" => {
                let served = params
                    .as_ref()
                    .and_then(|params| params["sessionId"].as_str())
                    .is_some_and(|session| self.sessions.contains_key(session));
                tokio::spawn(Agent::delete_session(
                    self.editor.clone(),
                    self.state.clone(),
                    served,
                    id,
                    params,
                ));
            }
            "session/set_config_option" => self.set_option(id, params).await,
            "session/set_mode" => self.set_mode(id, params).await,
            _ => {
                self.editor.fail(id, -32601, "no such method", None).await;
            }
        }
    }

    /// Takes one notification of the editor.
    async fn notify(&mut self, method: &str, params: Option<Value>) {
        match method {
            "session/cancel" => self.cancel(params).await,
            "$/cancel_request" => self.cancel_request(params).await,
            _ => {}
        }
    }

    /// Takes `session/cancel`: the run of the session stops.
    async fn cancel(&mut self, params: Option<Value>) {
        let Some(session) =
            params.and_then(|params| params["sessionId"].as_str().map(str::to_string))
        else {
            return;
        };
        if let Some(commands) = self.served(&session) {
            let _ = commands.send(Command::Cancel).await;
        }
    }

    /// Takes `$/cancel_request`: the prompt the id names stops the way a cancel
    /// stops it, another request this agent still works on is answered `-32800`,
    /// and an id nothing waits for is ignored.
    ///
    /// A request of another kind is answered here because its task cannot take
    /// it: the task is inside the call that waits for the worker.
    async fn cancel_request(&mut self, params: Option<Value>) {
        let Some(id) = params.as_ref().map(|params| params["requestId"].clone()) else {
            return;
        };
        let Some(key) = key_of(&id) else {
            return;
        };
        match self.editor.cancel(&key) {
            Cancel::Prompt(session) => {
                if let Some(commands) = self.served(&session) {
                    let _ = commands.send(Command::Cancel).await;
                }
            }
            Cancel::Answered => self.editor.answer_cancelled(id).await,
            Cancel::None => {}
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
        if let Some(init) =
            params.and_then(|params| serde_json::from_value::<message::Initialize>(params).ok())
        {
            if let Some(name) = init.client_info.and_then(|info| info.name)
                && !name.is_empty()
            {
                self.name = name;
            }
            // Only an advertised `notices: {}` takes a notice: an editor that
            // leaves `session` or `notices` out, or sends null, takes none.
            self.notices = init
                .client_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.session.as_ref())
                .and_then(|session| session.notices.as_ref())
                .is_some_and(|notices| !notices.is_null());
            // The same rule for the form of a question set: `form: {}` takes
            // one, and a capability that leaves it out or names it null takes
            // the permission request instead.
            self.elicits = init
                .client_capabilities
                .and_then(|capabilities| capabilities.elicitation)
                .and_then(|elicitation| elicitation.form)
                .is_some_and(|form| !form.is_null());
        }
        self.initialized = true;
        self.editor
            .respond(
                id,
                json!({
                    "protocolVersion": VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "sessionCapabilities": {
                            "list": {},
                            "resume": {},
                            "close": {},
                            "delete": {},
                        },
                        "promptCapabilities": {
                            "image": true,
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
            Ok(entry) => self.join(id, entry, &cwd, true, false).await,
            Err(error) => self.editor.fail(id, -32603, &error, None).await,
        }
    }

    /// Joins the worker of a session that exists, replays its history and then
    /// answers the request.
    async fn load_session(&mut self, id: Value, params: Option<Value>) {
        self.open_session(id, params, true).await;
    }

    /// Joins the worker of a session that exists without replaying it, so the
    /// editor keeps its own copy of the conversation.
    async fn resume_session(&mut self, id: Value, params: Option<Value>) {
        self.open_session(id, params, false).await;
    }

    /// Joins the worker of a session that exists, replaying its history when
    /// `replay` says so, and then answers the request `id`.
    async fn open_session(&mut self, id: Value, params: Option<Value>, replay: bool) {
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
        self.join(id, entry, &cwd, false, replay).await;
    }

    /// Sends the prompt of a session to its task, which answers it when the run
    /// it starts ends.
    async fn prompt(&mut self, id: Value, prompt: Option<message::Prompt<'_>>) {
        let Some(prompt) = prompt else {
            self.editor
                .fail(id, -32602, "the params are not a prompt", None)
                .await;
            return;
        };
        let Some(served) = self.sessions.get(&prompt.session_id) else {
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
        let commands = served.commands.clone();
        let (text, pictures) = match text_of(prompt.prompt) {
            Ok(parts) => parts,
            Err(message) => {
                self.editor.fail(id, -32602, &message, None).await;
                return;
            }
        };
        let text = match pictures.is_empty() {
            true => text,
            false => {
                let dir = self.records_dir(served, &prompt.session_id).await;
                match pictures_of(&dir, text, pictures).await {
                    Ok(text) => text,
                    Err(message) => {
                        self.editor.fail(id, -32602, &message, None).await;
                        return;
                    }
                }
            }
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
            return;
        }
        // The prompt is the one request of the editor a cancel can take.
        if let Some(key) = key_of(&id) {
            self.editor.mark_prompt(&key, &prompt.session_id);
        }
    }

    /// Connects to `entry` as a client of the session, replays its history when
    /// `replay` says so, and answers the request `id` with the shape `new` names
    /// for a session this agent did not serve yet.
    async fn join(
        &mut self,
        id: Value,
        entry: discovery::Entry,
        cwd: &Path,
        new: bool,
        replay: bool,
    ) {
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
        if replay
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
        // A load of a session this agent already serves keeps the task it has:
        // a second task would send every update of the session twice.
        if self
            .sessions
            .get(&session)
            .is_some_and(|served| !served.commands.is_closed())
        {
            let _ = client.detach().await;
            if let Some(commands) = self.served(&session) {
                let _ = commands.send(Command::Opened { id, new: false }).await;
            }
            return;
        }
        let model = init.model.as_deref().unwrap_or_default().to_string();
        let level = init.level.as_deref().map(str::to_string);
        let mode = init.mode.as_deref().map(str::to_string);
        let title = stored_title(&self.state, cwd, &session).await;
        let (models, thought) = self.models(cwd).await;
        let (commands, requests) = mpsc::channel(QUEUE);
        let task = Session::new(
            session.clone(),
            init.client,
            cwd.to_path_buf(),
            self.editor.clone(),
            commands.clone(),
            model,
            level,
            mode,
            title,
            models,
            thought,
            init.skills,
            init.context_window,
            self.notices,
            self.elicits,
        );
        tokio::spawn(task.run(client, requests));
        // The task answers, with the config options it holds.
        let _ = commands.send(Command::Opened { id, new }).await;
        self.sessions.insert(
            session,
            Served {
                commands,
                cwd: cwd.to_path_buf(),
            },
        );
    }

    /// The `provider/model` texts the model sheet of the terminal UI lists for
    /// `cwd`: the recent ones, then each provider's, each once, with the
    /// reasoning levels each model takes.
    async fn models(&self, cwd: &Path) -> (Vec<String>, HashMap<String, Vec<String>>) {
        let cwd = cwd.to_path_buf();
        let state = self.state.clone();
        let profile = profile_of(&self.words);
        let listed = tokio::task::spawn_blocking(move || {
            crate::tui_host::models_of(&cwd, profile.as_deref(), &state)
        })
        .await
        .unwrap_or_default();
        let mut models: Vec<String> = Vec::new();
        let mut thought: HashMap<String, Vec<String>> = HashMap::new();
        for model in &listed.recent {
            add(&mut models, &mut thought, model, None);
        }
        for provider in &listed.providers {
            for model in &provider.models {
                add(
                    &mut models,
                    &mut thought,
                    &model.id,
                    model.reasoning.as_ref(),
                );
            }
        }
        (models, thought)
    }

    /// Takes `session/set_config_option`: the model, the reasoning level or the
    /// permission mode of a session.
    async fn set_option(&mut self, id: Value, params: Option<Value>) {
        let params = params.unwrap_or_default();
        let (Some(session), Some(config), Some(value)) = (
            params["sessionId"].as_str(),
            params["configId"].as_str(),
            params["value"].as_str(),
        ) else {
            self.editor
                .fail(id, -32602, "the params name no option to set", None)
                .await;
            return;
        };
        let Some(config) = Config::read(config) else {
            self.editor
                .fail(
                    id,
                    -32602,
                    "the config options are model, thought_level and mode",
                    None,
                )
                .await;
            return;
        };
        let Some(commands) = self.served(session) else {
            self.editor
                .fail(
                    id,
                    -32002,
                    "no such session",
                    Some(json!({"sessionId": session})),
                )
                .await;
            return;
        };
        let set = Command::SetConfig {
            id: id.clone(),
            config,
            value: value.to_string(),
        };
        if commands.send(set).await.is_err() {
            self.editor
                .fail(id, -32003, "the session's worker closed", None)
                .await;
        }
    }

    /// Takes `session/set_mode`: the mode the session decides with.
    async fn set_mode(&mut self, id: Value, params: Option<Value>) {
        let params = params.unwrap_or_default();
        let (Some(session), Some(mode)) = (params["sessionId"].as_str(), params["modeId"].as_str())
        else {
            self.editor
                .fail(id, -32602, "the params name no mode to set", None)
                .await;
            return;
        };
        let Some(commands) = self.served(session) else {
            self.editor
                .fail(
                    id,
                    -32002,
                    "no such session",
                    Some(json!({"sessionId": session})),
                )
                .await;
            return;
        };
        let set = Command::SetMode {
            id: id.clone(),
            mode: mode.to_string(),
        };
        if commands.send(set).await.is_err() {
            self.editor
                .fail(id, -32003, "the session's worker closed", None)
                .await;
        }
    }

    /// Takes `session/close`: the task of the session leaves it, so the worker
    /// stops when no client is left. A session this agent does not serve frees
    /// nothing, which makes a second close the same empty answer.
    async fn close_session(&mut self, id: Value, params: Option<Value>) {
        let params = params.unwrap_or_default();
        let Some(session) = params["sessionId"].as_str() else {
            self.editor
                .fail(id, -32602, "the params name no session to close", None)
                .await;
            return;
        };
        let Some(commands) = self.sessions.remove(session).map(|served| served.commands) else {
            self.editor.respond(id, json!({})).await;
            return;
        };
        // The task answers both lines of a close that holds a prompt: the
        // prompt `cancelled` first, then the close.
        if commands
            .send(Command::Close { id: id.clone() })
            .await
            .is_err()
        {
            self.editor.respond(id, json!({})).await;
        }
    }

    /// Takes `session/list`: the sessions of one folder, or of every folder,
    /// newest first.
    async fn list_sessions(editor: Editor, state: PathBuf, id: Value, params: Option<Value>) {
        let Ok(listed) =
            serde_json::from_value::<message::ListSessions>(params.unwrap_or_default())
        else {
            editor
                .fail(id, -32602, "the params are not a list of sessions", None)
                .await;
            return;
        };
        let cwd = match listed.cwd.as_deref() {
            Some(given) => match folder(given).await {
                Some(path) => Some(path),
                None => {
                    editor
                        .fail(id, -32602, "cwd is not an existing folder", None)
                        .await;
                    return;
                }
            },
            None => None,
        };
        let from = match listed.cursor.as_deref() {
            None => 0,
            Some(text) => match offset(text) {
                Some(from) => from,
                None => {
                    editor
                        .fail(id, -32602, "the cursor is not one this agent wrote", None)
                        .await;
                    return;
                }
            },
        };
        let listed =
            tokio::task::spawn_blocking(move || sessions(&state, cwd.as_deref(), from)).await;
        match listed {
            Ok(Ok(page)) if page.total < from => {
                editor
                    .fail(id, -32602, "the cursor is not one this agent wrote", None)
                    .await;
            }
            Ok(Ok(page)) => {
                let answer = json!({
                    "sessions": page.sessions,
                    "nextCursor": page.next.map(|at| at.to_string()),
                });
                editor.respond(id, answer).await;
            }
            Ok(Err(error)) => {
                editor
                    .fail(
                        id,
                        -32603,
                        &format!("could not read the sessions: {error}"),
                        None,
                    )
                    .await;
            }
            Err(error) => {
                editor
                    .fail(
                        id,
                        -32603,
                        &format!("could not read the sessions: {error}"),
                        None,
                    )
                    .await;
            }
        }
    }

    /// Takes `session/delete`: the folder of one session that no worker serves.
    async fn delete_session(
        editor: Editor,
        state: PathBuf,
        served: bool,
        id: Value,
        params: Option<Value>,
    ) {
        let params = params.unwrap_or_default();
        let Some(session) = params["sessionId"].as_str() else {
            editor
                .fail(id, -32602, "the params name no session to delete", None)
                .await;
            return;
        };
        if !plain(session) {
            editor
                .fail(
                    id,
                    -32602,
                    "the session id is not one this agent wrote",
                    None,
                )
                .await;
            return;
        }
        // A session a worker still serves is the editor's to close first, so no
        // delete takes a journal another process writes.
        let live = discovery::list(&discovery::folder(&state))
            .await
            .into_iter()
            .any(|entry| entry.session == session);
        if live || served {
            editor
                .fail(
                    id,
                    -32602,
                    "the session has a live worker",
                    Some(json!({"sessionId": session})),
                )
                .await;
            return;
        }
        let (folder, dir) = match session_folder(&state, session).await {
            Found::Folder(folder, dir) => (folder, dir),
            // A session that is gone is deleted already.
            Found::Gone => {
                editor.respond(id, json!({})).await;
                return;
            }
            Found::Outside => {
                editor
                    .fail(
                        id,
                        -32602,
                        "the session folder is not inside the sessions root",
                        None,
                    )
                    .await;
                return;
            }
        };
        let removed =
            tokio::task::spawn_blocking(move || crate::session::remove(&folder, &dir)).await;
        match removed {
            Ok(Ok(())) => editor.respond(id, json!({})).await,
            Ok(Err(error)) => {
                editor
                    .fail(
                        id,
                        -32603,
                        &format!("could not delete the session: {error}"),
                        None,
                    )
                    .await;
            }
            Err(error) => {
                editor
                    .fail(
                        id,
                        -32603,
                        &format!("could not delete the session: {error}"),
                        None,
                    )
                    .await;
            }
        }
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

/// The profile the words name, as `--profile <name>` or `--profile=<name>`.
fn profile_of(words: &[OsString]) -> Option<String> {
    let mut words = words.iter().filter_map(|word| word.to_str());
    while let Some(word) = words.next() {
        if word == "--profile" {
            return words.next().map(str::to_string);
        }
        if let Some(name) = word.strip_prefix("--profile=") {
            return Some(name.to_string());
        }
    }
    None
}

/// Adds `text` and the reasoning levels `reasoning` names to the lists of the
/// model sheet, `text` once.
///
/// The sheet lists the recent models first, which carry no cache entry of their
/// own, so a model it then lists under its provider takes the levels the
/// provider names: the fallback is for a model no provider names.
fn add(
    models: &mut Vec<String>,
    thought: &mut HashMap<String, Vec<String>>,
    text: &str,
    reasoning: Option<&Reasoning>,
) {
    if !models.iter().any(|known| known == text) {
        models.push(text.to_string());
    }
    match reasoning {
        Some(reasoning) => {
            thought.insert(text.to_string(), session::levels_of(Some(reasoning)));
        }
        None => {
            thought
                .entry(text.to_string())
                .or_insert_with(|| session::levels_of(None));
        }
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

/// The title of the first user text of a stored session, as the session store
/// keeps it, and nothing for a session with no user text yet.
async fn stored_title(state: &Path, cwd: &Path, session: &str) -> Option<String> {
    let dir = session_dir(state, cwd, session).await?;
    let folder = dir.parent()?.to_path_buf();
    let session = session.to_string();
    let entries = tokio::task::spawn_blocking(move || listed_sessions(&folder))
        .await
        .ok()?
        .ok()?;
    let entry = entries.into_iter().find(|entry| entry.session == session)?;
    session::title_of_text(&entry.title)
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

/// The place in a session list a cursor names, and `None` for a cursor this
/// agent never wrote.
fn offset(cursor: &str) -> Option<usize> {
    cursor.parse::<usize>().ok()
}

/// Whether `session` is one name inside a folder of the sessions root, and not a
/// path that leaves it.
fn plain(session: &str) -> bool {
    !session.is_empty()
        && !session.contains(['/', '\0'])
        && Path::new(session).components().count() == 1
}

/// Where the folder of a session to delete is.
enum Found {
    /// No folder of the sessions root holds the session.
    Gone,
    /// The folder of the sessions of one working directory, and the folder of
    /// the session in it.
    Folder(PathBuf, PathBuf),
    /// The session is reached through a link, or its real place is outside the
    /// sessions root.
    Outside,
}

/// The folder of the sessions of one working directory, and the folder of
/// `session` in it.
///
/// The search covers every folder of the sessions root, since the id of the
/// session is all a delete is given. A folder that is a link, or whose real path
/// is not inside the real sessions root, is `Outside`: no delete follows a link.
async fn session_folder(state: &Path, session: &str) -> Found {
    let root = paths::sessions_root(state);
    let Ok(mut folders) = tokio::fs::read_dir(&root).await else {
        return Found::Gone;
    };
    let real = tokio::fs::canonicalize(&root).await.ok();
    while let Ok(Some(entry)) = folders.next_entry().await {
        let dir = entry.path().join(session);
        let Ok(meta) = tokio::fs::symlink_metadata(&dir).await else {
            continue;
        };
        let linked = meta.file_type().is_symlink()
            || tokio::fs::symlink_metadata(entry.path())
                .await
                .is_ok_and(|meta| meta.file_type().is_symlink());
        let inside = match (&real, tokio::fs::canonicalize(&dir).await) {
            (Some(real), Ok(dir)) => dir.starts_with(real),
            _ => false,
        };
        if linked || !inside {
            return Found::Outside;
        }
        return Found::Folder(entry.path(), dir);
    }
    Found::Gone
}

/// The sessions of `cwd`, or of every folder when `cwd` is `None`, past the
/// first `from` of them, newest first.
///
/// The lists and the headers of the sessions are read on the blocking pool, so
/// a caller runs this in `spawn_blocking`.
///
/// The order is by the time of the last record of each session, and then by the
/// id, which counts from the start of the session. A cursor is the place a page
/// ends at, so a list that grows between two pages shows the new sessions in the
/// page of its own.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once for each session/list"
)]
fn sessions(state: &Path, cwd: Option<&Path>, from: usize) -> Result<Page, String> {
    let root = paths::sessions_root(state);
    let folders: Vec<PathBuf> = match cwd {
        Some(cwd) => folders_of(&root, cwd),
        None => {
            let mut listed = Vec::new();
            let Ok(entries) = std::fs::read_dir(&root) else {
                return Ok(Page {
                    total: 0,
                    sessions: Vec::new(),
                    next: None,
                });
            };
            for entry in entries.flatten() {
                listed.push(entry.path());
            }
            listed
        }
    };
    let mut found: Vec<(u64, Value)> = Vec::new();
    for folder in &folders {
        for entry in listed_sessions(folder)? {
            found.push((entry.updated_ms, info(folder, &entry, cwd)));
        }
    }
    found.sort_by(|one, two| {
        two.0.cmp(&one.0).then_with(|| {
            two.1["sessionId"]
                .as_str()
                .cmp(&one.1["sessionId"].as_str())
        })
    });
    let total = found.len();
    let page: Vec<Value> = found
        .into_iter()
        .skip(from)
        .take(PAGE)
        .map(|(_, info)| info)
        .collect();
    let next = (from + page.len() < total).then_some(from + page.len());
    Ok(Page {
        total,
        sessions: page,
        next,
    })
}

/// The folders of the sessions of `cwd`: the one its text keys, and the one the
/// folder it resolves to keys, since a worker keys the folder it canonicalized.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once for each session/list"
)]
fn folders_of(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut keys = vec![paths::cwd_key(cwd)];
    if let Ok(real) = std::fs::canonicalize(cwd) {
        let key = paths::cwd_key(&real);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.into_iter()
        .map(|key| root.join(key))
        .filter(|folder| folder.is_dir())
        .collect()
}

/// One page of a session list.
struct Page {
    /// How many sessions the list holds in all.
    total: usize,
    /// The sessions of the page.
    sessions: Vec<Value>,
    /// The place the page after this one starts at, when there is one.
    next: Option<usize>,
}

/// The entries of the session list of one folder, newest first, with the title
/// of each read from its journal when the list holds none.
fn listed_sessions(folder: &Path) -> Result<Vec<reagent_store::Entry>, String> {
    if !std::fs::exists(folder).map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let list = reagent_store::SessionList::open(folder, crate::session::entry_of)
        .map_err(|error| error.to_string())?;
    let mut entries = list.all().to_vec();
    reagent_ai::record::fill_titles(folder, &mut entries);
    Ok(entries)
}

/// One session as `session/list` answers it: its id, the folder it ran in, its
/// title and the time of its last record.
fn info(folder: &Path, entry: &reagent_store::Entry, cwd: Option<&Path>) -> Value {
    let cwd = cwd
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .or_else(|| meta_cwd(&folder.join(&entry.session)))
        .unwrap_or_else(|| folder.to_string_lossy().into_owned());
    json!({
        "sessionId": entry.session,
        "cwd": cwd,
        "title": (!entry.title.is_empty()).then_some(&entry.title),
        "updatedAt": iso(entry.updated_ms),
    })
}

/// The folder a session ran in, from the header of its newest part.
fn meta_cwd(dir: &Path) -> Option<String> {
    let session = reagent_store::Session::open(dir).ok()?;
    let cwd = crate::session::Meta::decode(&session.header().meta).cwd;
    (!cwd.is_empty()).then_some(cwd)
}

/// The time `ms` as ISO 8601 in UTC, such as `2026-10-27T14:03:11Z`.
pub(super) fn iso(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(SECONDS_OF_DAY);
    let rest = secs.rem_euclid(SECONDS_OF_DAY);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// The year, the month and the day of the day `days` after the epoch.
///
/// This is Howard Hinnant's `civil_from_days`, which is the one the session
/// stamp of this program writes its dates with too.
fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The longest picture one prompt may carry, in bytes of the picture itself.
const PICTURE_CAP: usize = 20 * 1024 * 1024;

/// One picture of a prompt, before it is a file: its bytes as base64 text and
/// the media type the editor named.
struct Picture<'a> {
    /// The bytes of the picture, as base64 text, borrowed from the line.
    data: Cow<'a, str>,
    /// The media type of the picture.
    mime_type: Cow<'a, str>,
}

/// The message the blocks of a prompt make and the pictures it carries, or why
/// this agent cannot take one of them. The text of each picture moves into its
/// `Picture`, so a payload of megabytes is not copied.
fn text_of<'a>(blocks: Vec<message::Block<'a>>) -> Result<(String, Vec<Picture<'a>>), String> {
    let mut text = String::new();
    let mut pictures = Vec::new();
    for block in blocks {
        match block {
            message::Block::Text { text: chunk } => text.push_str(&chunk),
            message::Block::ResourceLink { uri, name } => {
                separate(&mut text);
                text.push_str(&format!("[{name}]({uri})"));
            }
            message::Block::Resource { resource } => {
                let mime = resource.mime_type.as_deref().unwrap_or_default();
                match (resource.text, resource.blob) {
                    (Some(body), _) => {
                        separate(&mut text);
                        text.push_str(&format!("```{}\n{body}\n```", resource.uri));
                    }
                    (None, Some(blob)) if mime.starts_with("image/") => {
                        pictures.push(Picture {
                            data: blob,
                            mime_type: Cow::Owned(mime.to_string()),
                        });
                    }
                    _ => {
                        return Err(format!(
                            "this agent takes the text of {} or a picture it holds",
                            resource.uri
                        ));
                    }
                }
            }
            message::Block::Image { data, mime_type } => {
                if !mime_type.starts_with("image/") {
                    return Err(format!("{mime_type} is not an image this agent takes"));
                }
                pictures.push(Picture { data, mime_type });
            }
            message::Block::Audio {} => return Err(String::from("this agent takes no audio")),
            message::Block::Other => {
                return Err(String::from("this agent takes no such block"));
            }
        }
    }
    Ok((text, pictures))
}

/// `text` with a word naming each of `pictures`, which are written to `dir` for
/// the worker to attach, as a message of the terminal UI names an image with
/// `@path`.
async fn pictures_of(
    dir: &Path,
    text: String,
    pictures: Vec<Picture<'_>>,
) -> Result<String, String> {
    let mut text = text;
    for picture in pictures {
        let path = picture_file(dir, picture).await?;
        separate(&mut text);
        text.push_str(&format!("@\"{}\"", path.display()));
    }
    Ok(text)
}

/// The file one picture is written to, under `dir`, which is named by a hash of
/// the bytes of the picture.
#[expect(
    clippy::disallowed_methods,
    reason = "the write runs inside spawn_blocking"
)]
async fn picture_file(dir: &Path, picture: Picture<'_>) -> Result<PathBuf, String> {
    let Picture { data, mime_type } = picture;
    // A text this long cannot be a picture under the cap, whatever it holds.
    if data.len() > PICTURE_CAP / 3 * 4 + 4 {
        return Err(format!("an image over {PICTURE_CAP} bytes is not taken"));
    }
    let bytes = decode_in_pieces(&data).await?;
    if bytes.len() > PICTURE_CAP {
        return Err(format!("an image over {PICTURE_CAP} bytes is not taken"));
    }
    let images = dir.join("images");
    let extension = extension(&mime_type);
    // The bytes move to the blocking task, so no write copies them.
    tokio::task::spawn_blocking(move || {
        let write = |error: std::io::Error| format!("could not write the image: {error}");
        std::fs::create_dir_all(&images).map_err(write)?;
        let path = images.join(format!("{:016x}.{extension}", hash(&bytes)));
        std::fs::write(&path, &bytes).map_err(write)?;
        Ok(path)
    })
    .await
    .map_err(|_| String::from("the image could not be read"))?
}

/// How the decode of base64 text stands between two pieces of it.
#[derive(Default)]
struct Digits {
    /// The bits of the digits not yet written as a byte.
    buffer: u32,
    /// How many of those bits the buffer holds.
    bits: u32,
    /// How many digits were read.
    count: usize,
    /// Whether the padding ended the text.
    done: bool,
}

/// Decodes one piece of base64 text into `out`, which an editor may break into
/// lines.
fn decode_piece(out: &mut Vec<u8>, state: &mut Digits, piece: &[u8]) -> Result<(), String> {
    for &byte in piece {
        if state.done || byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            state.done = true;
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(String::from("the image is not base64 text")),
        };
        state.buffer = ((state.buffer << 6) | u32::from(value)) & 0xffff;
        state.bits += 6;
        state.count += 1;
        if state.bits >= 8 {
            state.bits -= 8;
            out.push((state.buffer >> state.bits) as u8);
        }
    }
    Ok(())
}

/// The bytes of the base64 text of a picture, all at once.
#[cfg(test)]
fn decode(data: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut state = Digits::default();
    decode_piece(&mut out, &mut state, data.as_bytes())?;
    finish(&state)?;
    Ok(out)
}

/// Whether the digits read so far end a base64 text.
fn finish(state: &Digits) -> Result<(), String> {
    if state.count % 4 == 1 {
        return Err(String::from("the image is not base64 text"));
    }
    Ok(())
}

/// The bytes of the base64 text of a picture, read in pieces so the task that
/// reads the editor's lines yields between them: the text borrows from the line
/// and cannot go to another thread, and only the decoded bytes are new.
async fn decode_in_pieces(data: &str) -> Result<Vec<u8>, String> {
    const PIECE: usize = 1024 * 1024;
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut state = Digits::default();
    for piece in data.as_bytes().chunks(PIECE) {
        decode_piece(&mut out, &mut state, piece)?;
        tokio::task::yield_now().await;
    }
    finish(&state)?;
    Ok(out)
}

/// The name a file of these bytes gets, which is an FNV-1a hash of them.
fn hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The extension the file of a picture of `mime_type` takes.
fn extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/heic" => "heic",
        _ => "img",
    }
}

/// Starts the next block of a message on its own line.
fn separate(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::PICTURE_CAP;
    use super::Picture;
    use super::add;
    use super::decode;
    use super::hash;
    use super::picture_file;
    use std::collections::HashMap;
    use std::path::Path;

    /// A model the sheet lists as recent comes with no cache entry, and then
    /// again under its provider with the levels the cache names: the levels the
    /// provider names are the ones the option offers.
    #[test]
    fn a_model_the_sheet_listed_twice_keeps_the_levels_its_provider_names() {
        let mut models = Vec::new();
        let mut thought = HashMap::new();
        add(&mut models, &mut thought, "provider/model", None);
        let reasoning = reagent_ai::levels::Reasoning::from_words(["low", "high", "xhigh"]);
        add(
            &mut models,
            &mut thought,
            "provider/model",
            Some(&reasoning),
        );
        assert_eq!(models, ["provider/model"]);
        assert_eq!(thought["provider/model"], ["low", "high", "xhigh"]);
    }

    /// The bytes of a picture are the ones its base64 text names, which an
    /// editor may break into lines, and a text that is not base64 names none.
    #[tokio::test]
    async fn the_base64_text_of_a_picture_names_its_bytes() {
        assert_eq!(decode("aGk=").expect("hi"), b"hi");
        let long = "QUJD".repeat(600_000);
        assert_eq!(
            super::decode_in_pieces(&long).await.expect("pieces"),
            decode(&long).expect("at once")
        );
        assert_eq!(decode("aG\nk=").expect("hi"), b"hi");
        assert_eq!(decode("aGk").expect("hi with no padding"), b"hi");
        assert_eq!(decode("").expect("no bytes"), b"");
        assert!(decode("a").is_err(), "a group of one digit");
        assert!(decode("a-").is_err(), "a digit base64 has not");
    }

    /// Two pictures of the same bytes take the same name, and two of other
    /// bytes another.
    #[test]
    fn a_picture_is_named_by_a_hash_of_its_bytes() {
        assert_eq!(hash(b"one"), hash(b"one"));
        assert_ne!(hash(b"one"), hash(b"two"));
    }

    /// A picture over the cap is refused before a file is written, whatever the
    /// folder the prompt names.
    #[tokio::test]
    async fn a_picture_over_the_cap_is_refused() {
        let over = Picture {
            data: std::borrow::Cow::Owned("A".repeat(PICTURE_CAP / 3 * 4 + 5)),
            mime_type: std::borrow::Cow::Borrowed("image/png"),
        };
        let error = picture_file(Path::new("/nonexistent"), over)
            .await
            .expect_err("a picture over the cap");
        assert!(error.contains("is not taken"), "{error}");
    }
}

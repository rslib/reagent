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
use reagent_tui::client::RunStop;
use reagent_tui::client::Settled;
use reagent_tui::client::Skill;
use reagent_tui::history;
use reagent_tui::picture;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

use super::editor::Editor;
use super::editor::Reply;
use super::message::AvailableCommand;
use super::message::CallRef;
use super::message::Change;
use super::message::Chunk;
use super::message::CommandInput;
use super::message::Cost;
use super::message::Elicit;
use super::message::ElicitMode;
use super::message::Kind;
use super::message::Location;
use super::message::OptionKind;
use super::message::Permission;
use super::message::PermissionOption;
use super::message::SessionUpdate;
use super::message::Severity;
use super::message::Status;
use super::message::ToolContent;

/// The longest text of one tool result this agent sends, in bytes.
const CAP: usize = 8 * 1024;

/// The permission modes the editor may pick, each with the line it shows.
const MODES: [(&str, &str); 4] = [
    ("default", "Default"),
    ("accept-edits", "Accept edits"),
    ("bypass", "Bypass"),
    ("auto", "Auto"),
];

/// The reasoning levels every model starts with, for one whose own levels the
/// models cache does not name.
const LEVELS: [&str; 4] = ["off", "low", "medium", "high"];

/// How many sets of one session may wait for the notice of the worker, which is
/// one per change it kept.
const ASKS: usize = 8;

/// One command a plugin offers, as the worker names it.
struct Plugin {
    /// The name the user types after the slash.
    name: String,
    /// What the command does.
    description: String,
}

/// One command of the session, which its update borrows.
struct Offered {
    /// The name the user types after the slash.
    name: String,
    /// The line the editor shows beside it.
    description: String,
    /// What the command takes, when it takes arguments.
    hint: Option<String>,
}

/// The settings of a session: the model it runs, the reasoning level it sends
/// and the permission mode it decides with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    /// The `provider/model` it runs.
    model: String,
    /// The level it sends, and nothing for a session that sends none.
    level: Option<String>,
    /// The mode it decides with.
    mode: String,
}

impl State {
    /// The state a worker opens a session with: the model, the level and the
    /// mode of its `initialize`, and the mode every run starts with for a worker
    /// that names none.
    fn new(model: String, level: Option<String>, mode: Option<String>) -> State {
        State {
            model,
            level,
            mode: mode.unwrap_or_else(|| String::from("default")),
        }
    }

    /// The value of `config`, as the protocol names it: the level of a session
    /// that sends none is `off`.
    fn value(&self, config: Config) -> &str {
        match config {
            Config::Model => &self.model,
            Config::Level => self.level.as_deref().unwrap_or("off"),
            Config::Mode => &self.mode,
        }
    }

    /// Takes `value` as the value of `config`.
    fn keep(&mut self, config: Config, value: &str) {
        match config {
            Config::Model => self.model = value.to_string(),
            Config::Level => self.level = Some(value.to_string()),
            Config::Mode => self.mode = value.to_string(),
        }
    }
}

/// One config option of a session, as `session/set_config_option` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Config {
    /// The `provider/model` the session runs.
    Model,
    /// The reasoning level it sends.
    Level,
    /// The permission mode it decides with.
    Mode,
}

impl Config {
    /// The option the editor's config id names, and nothing for an id this
    /// agent has no option for.
    pub(super) fn read(id: &str) -> Option<Config> {
        match id {
            "model" => Some(Config::Model),
            "thought_level" => Some(Config::Level),
            "mode" => Some(Config::Mode),
            _ => None,
        }
    }

    /// The words of this option in a message.
    fn words(self) -> &'static str {
        match self {
            Config::Model => "model",
            Config::Level => "reasoning level",
            Config::Mode => "mode",
        }
    }
}

/// Why a session did not take a value the editor set.
enum Refusal {
    /// The value is not one the option offers.
    Params(String),
    /// The worker refused the value.
    Worker(String),
}

/// The level values a model takes, and the four every model starts with when
/// the models cache does not name its own.
pub(super) fn levels_of(reasoning: Option<&reagent_ai::levels::Reasoning>) -> Vec<String> {
    let known = reasoning
        .map(|reasoning| {
            let mut values: Vec<String> = Vec::with_capacity(reasoning.levels.iter().count() + 1);
            if reasoning.off == Some(true) {
                values.push(String::from("off"));
            }
            values.extend(
                reasoning
                    .levels
                    .iter()
                    .map(|level| level.name().to_string()),
            );
            values
        })
        .unwrap_or_default();
    if known.is_empty() {
        return LEVELS.iter().map(|level| (*level).to_string()).collect();
    }
    known
}

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
    /// Answers the `session/new` (`new`) or `session/load` request `id` that
    /// opened this session.
    Opened { id: Value, new: bool },
    /// Sets the config option `config` to `value`, and answers the request `id`
    /// with every option.
    SetConfig {
        id: Value,
        config: Config,
        value: String,
    },
    /// Sets the mode, and answers the request `id` with nothing.
    SetMode { id: Value, mode: String },
    /// Ends this task: the work of the session stops as a cancel stops it, the
    /// turn that waits answers `cancelled`, and the request `id` is answered.
    Close { id: Value },
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
    /// A question set of the worker's question `id`, asked as one form, with
    /// the name of the field of each question.
    Elicit { id: u64, names: Vec<String> },
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
    /// The id the worker gave this connection.
    client: u64,
    /// The worker's id of the message of the open turn, once the worker named it.
    message: Option<u64>,
    /// Whether the message of the open turn entered the conversation, so the
    /// next run end is the end of its own run and not of one it queued behind.
    started: bool,
    /// The error text of the run, when one failed.
    failure: Option<String>,
    /// The settings of the session as the editor knows them, which the options
    /// and the answers name.
    state: State,
    /// The settings the worker reported last, which the notice of each change
    /// names.
    reported: State,
    /// The values this session set that the worker has not reported yet, oldest
    /// first: the answer of a set names its value, so the notice of it is no
    /// news to the editor.
    asked: Vec<(Config, String)>,
    /// The mode this session asked for, while the worker has not named it: a run
    /// whose guard refuses `bypass` keeps the mode it had.
    asked_mode: Option<String>,
    /// The `provider/model` texts the editor may pick, in the order shown.
    models: Vec<String>,
    /// The reasoning level values each of those models takes, by its text.
    thought: HashMap<String, Vec<String>>,
    /// Whether the editor advertised the notices of a session.
    notices: bool,
    /// Whether the editor advertised that it shows an elicitation form, which a
    /// question set becomes.
    elicits: bool,
    /// The context window of the model, when the run knows it.
    window: Option<u64>,
    /// The skills the session found, which its command list offers.
    skills: Vec<Skill>,
    /// The commands the plugins of the session offer, in load order.
    plugins: Vec<Plugin>,
    /// The id of the message the agent writes of the open turn.
    assistant: Option<String>,
    /// The count of the messages whose id this session named.
    messages: u64,
    /// The title the editor was told, once the session had one.
    title: Option<String>,
    /// The end of the run the last frame named, which this task answers once
    /// the frame is gone: the stop word of the run rides with the client.
    pending: Option<RunEnd>,
    /// The request id of the plugin command the worker still runs for the
    /// open turn.
    command: Option<u64>,
    /// Whether the editor cancelled while that command ran.
    cancelled: bool,
    /// The end of a run that came while that command ran, with its stop word:
    /// the run the command started, when it was quick.
    ended: Option<(RunEnd, Option<RunStop>)>,
}

impl Session {
    /// Makes the session of a worker, which the caller starts.
    #[expect(
        clippy::too_many_arguments,
        reason = "one field of the session per argument, which the caller has"
    )]
    pub(super) fn new(
        id: String,
        client: u64,
        cwd: PathBuf,
        editor: Editor,
        commands: Sender<Command>,
        model: String,
        level: Option<String>,
        mode: Option<String>,
        title: Option<String>,
        models: Vec<String>,
        thought: HashMap<String, Vec<String>>,
        skills: Vec<Skill>,
        window: Option<u64>,
        notices: bool,
        elicits: bool,
    ) -> Self {
        let state = State::new(model, level, mode);
        Session {
            id,
            cwd,
            editor,
            commands,
            tools: Vec::new(),
            open: HashMap::new(),
            turn: None,
            client,
            message: None,
            started: false,
            failure: None,
            reported: state.clone(),
            state,
            asked: Vec::new(),
            asked_mode: None,
            models,
            thought,
            notices,
            elicits,
            window,
            skills,
            plugins: Vec::new(),
            assistant: None,
            messages: 0,
            title,
            pending: None,
            command: None,
            cancelled: false,
            ended: None,
        }
    }

    /// The id of the next message this session names: opaque, unique, and
    /// prefixed by how the editor shows it.
    fn name_message(&mut self, who: &str) -> String {
        self.messages += 1;
        format!("{who}-{}", self.messages)
    }

    /// The commands the session offers: the skills it found and the commands its
    /// plugins registered, each once, in name order.
    fn offered(&self) -> Vec<Offered> {
        let mut offered: Vec<Offered> = Vec::with_capacity(self.skills.len() + self.plugins.len());
        for skill in &self.skills {
            offered.push(Offered {
                name: skill.name.to_string(),
                description: format!("the {} skill at {}", skill.scope.as_str(), skill.path),
                hint: None,
            });
        }
        for plugin in &self.plugins {
            if offered.iter().any(|other| other.name == plugin.name) {
                continue;
            }
            offered.push(Offered {
                name: plugin.name.clone(),
                description: plugin.description.clone(),
                hint: Some(String::from("arguments")),
            });
        }
        offered.sort_by(|left, right| left.name.cmp(&right.name));
        offered
    }

    /// Sends the commands the session offers, which replaces the list the editor
    /// holds.
    async fn offer(&self) {
        let offered = self.offered();
        let commands: Vec<AvailableCommand<'_>> = offered
            .iter()
            .map(|command| AvailableCommand {
                name: &command.name,
                description: &command.description,
                input: command.hint.as_deref().map(|hint| CommandInput { hint }),
            })
            .collect();
        self.send(Change::AvailableCommandsUpdate {
            available_commands: commands,
        })
        .await;
    }

    /// Sends the title of the session, once its first prompt names one. The
    /// title is cut as the session store cuts it, from the words of the prompt
    /// and not the files it attaches.
    async fn titled(&mut self, text: &str) {
        if self.title.is_some() {
            return;
        }
        let Some(title) = title_of_text(text) else {
            return;
        };
        self.title = Some(title.clone());
        self.send_title(&title).await;
    }

    /// Sends the title the editor shows, with the time of this change.
    async fn send_title(&self, title: &str) {
        self.send(Change::SessionInfoUpdate {
            title,
            updated_at: super::server::iso(now_ms()),
        })
        .await;
    }

    /// The config options of the session: the model it runs, of the models the
    /// editor may pick, the reasoning level it sends and the mode it decides
    /// with.
    fn options(&self) -> Value {
        json!([self.model_option(), self.level_option(), self.mode_option(),])
    }

    /// The option of the model the session runs, of the models the editor may
    /// pick.
    fn model_option(&self) -> Value {
        let model = &self.state.model;
        let mut models: Vec<&str> = self.models.iter().map(String::as_str).collect();
        if !model.is_empty() && !models.contains(&model.as_str()) {
            models.insert(0, model);
        }
        let options: Vec<Value> = models
            .into_iter()
            .map(|model| json!({"value": model, "name": model}))
            .collect();
        json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": model,
            "options": options,
        })
    }

    /// The option of the reasoning level, of the levels the current model
    /// takes.
    fn level_option(&self) -> Value {
        let current = self.state.value(Config::Level);
        let options: Vec<Value> = self
            .levels(current)
            .into_iter()
            .map(|level| json!({"value": level, "name": titled(&level)}))
            .collect();
        json!({
            "id": "thought_level",
            "name": "Reasoning",
            "category": "thought_level",
            "type": "select",
            "currentValue": current,
            "options": options,
        })
    }

    /// The values of the level option: the levels the model of the session
    /// takes, and `current` when they leave it out, so the value the editor
    /// shows is always one it may pick again.
    fn levels(&self, current: &str) -> Vec<String> {
        let mut levels = self
            .thought
            .get(&self.state.model)
            .cloned()
            .unwrap_or_else(|| LEVELS.iter().map(|level| (*level).to_string()).collect());
        if !levels.iter().any(|level| level == current) {
            levels.insert(0, current.to_string());
        }
        levels
    }

    /// The option of the permission mode.
    fn mode_option(&self) -> Value {
        let options: Vec<Value> = MODES
            .iter()
            .map(|(id, name)| json!({"value": id, "name": name}))
            .collect();
        json!({
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": self.state.mode,
            "options": options,
        })
    }

    /// The `modes` of the answer to `session/new` or `session/load`: the mode
    /// the session decides with, of the ones this agent offers.
    fn modes(&self) -> Value {
        let available: Vec<Value> = MODES
            .iter()
            .map(|(id, name)| json!({"id": id, "name": name}))
            .collect();
        json!({"currentModeId": self.state.mode, "availableModes": available})
    }

    /// Reads the frames of the worker and carries out what the agent asks,
    /// until the worker closes the connection.
    pub(super) async fn run(mut self, mut client: Client, mut commands: Receiver<Command>) {
        loop {
            let step = tokio::select! {
                frame = client.next_frame() => match frame {
                    Ok(Next::Frame(frame)) => {
                        self.take(frame).await;
                        // The frame is gone, so the stop word of its run can be
                        // read from the client that decoded it.
                        if let Some(end) = self.pending.take() {
                            let stop = client.last_stop();
                            self.end(end, stop).await;
                        }
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

    /// The name and the arguments of the plugin command a prompt asks for: the
    /// text starts with the slash name of a command the session's plugins offer.
    fn plugin_command<'a>(&self, text: &'a str) -> Option<(String, &'a str)> {
        let line = text.strip_prefix('/')?;
        let (name, args) = match line.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (line, ""),
        };
        self.plugins
            .iter()
            .any(|plugin| plugin.name == name)
            .then(|| (name.to_string(), args))
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
                let sent = match self.plugin_command(&text) {
                    Some((name, args)) => {
                        let sent = client.command(&name, args).await;
                        self.command = sent.as_ref().ok().copied();
                        self.cancelled = false;
                        sent.map(drop)
                    }
                    None => client.send(&text, Mode::Queue).await,
                };
                match sent {
                    Ok(()) => {
                        self.ended = None;
                        self.turn = Some(id);
                        self.message = None;
                        self.started = false;
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
                self.cancelled |= self.command.is_some();
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
            Command::Opened { id, new } => {
                let answer = if new {
                    json!({
                        "sessionId": self.id,
                        "configOptions": self.options(),
                        "modes": self.modes(),
                    })
                } else {
                    json!({"configOptions": self.options(), "modes": self.modes()})
                };
                // A new session sends the commands first, so an editor that
                // shows them with the session has the list before the answer
                // that names it. A resume replays nothing before its answer.
                if new {
                    self.offer().await;
                    self.editor.respond(id, answer).await;
                } else {
                    self.editor.respond(id, answer).await;
                    self.offer().await;
                    if let Some(title) = &self.title {
                        self.send_title(title).await;
                    }
                }
                true
            }
            Command::SetConfig { id, config, value } => {
                self.set(id, config, value, client).await;
                true
            }
            Command::SetMode { id, mode } => {
                self.set_mode(id, mode, client).await;
                true
            }
            Command::Close { id } => {
                // The work of the session stops the way a cancel stops it, and a
                // turn that waits for it is answered `cancelled` before the
                // close, which is the last line this task writes.
                let _ = client.cancel().await;
                if let Some(turn) = self.turn.take() {
                    self.editor
                        .respond(turn, json!({"stopReason": "cancelled"}))
                        .await;
                }
                self.editor.respond(id, json!({})).await;
                false
            }
        }
    }

    /// Sets `config` to `value` for the editor's request `id`, and answers every
    /// option as it stands after the change.
    async fn set(&mut self, id: Value, config: Config, value: String, client: &mut Client) {
        if let Err(refusal) = self.apply(config, &value, client).await {
            self.refuse(id, refusal).await;
            return;
        }
        self.editor
            .respond(id, json!({"configOptions": self.options()}))
            .await;
    }

    /// Sets the mode for the editor's request `id`, which answers nothing but
    /// success: the mode it keeps is the one the options name.
    async fn set_mode(&mut self, id: Value, mode: String, client: &mut Client) {
        if let Err(refusal) = self.apply(Config::Mode, &mode, client).await {
            self.refuse(id, refusal).await;
            return;
        }
        self.editor.respond(id, json!({})).await;
    }

    /// Checks `value` against the options of `config`, sets it with the worker
    /// and takes it as the value the editor knows, which the answer names.
    async fn apply(
        &mut self,
        config: Config,
        value: &str,
        client: &mut Client,
    ) -> Result<(), Refusal> {
        if !self.allows(config, value) {
            return Err(Refusal::Params(format!(
                "`{value}` is not a value of the option `{}`",
                config.words()
            )));
        }
        let asked = match config {
            Config::Model => client.set(Some(value), None, None).await,
            Config::Level => client.set(None, Some(value), None).await,
            Config::Mode => client.set(None, None, Some(value)).await,
        };
        if let Err(error) = asked {
            return Err(Refusal::Worker(format!(
                "the session took no {} `{value}`: {error}",
                config.words()
            )));
        }
        // The one mode a run refuses is `bypass`, which stays pending until the
        // worker names it in its settings notice: a run whose guard refuses it
        // keeps the mode it had, and the options must not name the refused one.
        // Any other value is the state the session holds, so the answer of this
        // request names it. The notice of that change names the same value, and
        // the marker below says so: the editor already heard it, and the order
        // of the notices of two sets is the worker's, not the editor's. A set of
        // the value the worker already reported changes nothing, and it sends no
        // notice, so it leaves no marker that would hide a later change of
        // another client to that value.
        if config == Config::Mode {
            self.asked_mode = None;
            if value == "bypass" && self.state.mode != value {
                self.asked_mode = Some(value.to_string());
                return Ok(());
            }
        }
        self.state.keep(config, value);
        if self.reported.value(config) != value {
            if self.asked.len() == ASKS {
                self.asked.remove(0);
            }
            self.asked.push((config, value.to_string()));
        }
        Ok(())
    }

    /// Whether the session takes `value` for `config`: a model is any text that
    /// is not empty, and a level or a mode is one the option lists.
    fn allows(&self, config: Config, value: &str) -> bool {
        match config {
            Config::Model => !value.is_empty(),
            Config::Level => self
                .levels(self.state.value(Config::Level))
                .iter()
                .any(|level| level == value),
            Config::Mode => MODES.iter().any(|(id, _)| *id == value),
        }
    }

    /// Answers the editor's request `id` with what went wrong with a value it
    /// set.
    async fn refuse(&self, id: Value, refusal: Refusal) {
        match refusal {
            Refusal::Params(message) => self.editor.fail(id, -32602, &message, None).await,
            Refusal::Worker(message) => self.editor.fail(id, -32603, &message, None).await,
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
            Waiting::Elicit { id, names } => {
                // A form the editor filled in is one list of picks per
                // question; a declined or a failed one is no answer at all.
                let answers = match reply {
                    Ok(result) if result["action"] == "accept" => Some(
                        names
                            .iter()
                            .map(|name| picks(&result["content"][name]))
                            .collect::<Vec<Vec<String>>>(),
                    ),
                    _ => None,
                };
                let _ = client.answer(id, answers.as_deref()).await;
            }
        }
    }

    /// Turns one frame of the worker into what the editor shows, or into the
    /// answer of the open turn.
    async fn take(&mut self, frame: Frame<'_>) {
        match frame {
            Frame::TurnStart => {
                self.assistant = Some(self.name_message("agent"));
            }
            Frame::TextDelta(text) => {
                self.send(Change::AgentMessageChunk {
                    content: Chunk::Text { text },
                    message_id: self.assistant.as_deref(),
                })
                .await;
            }
            Frame::ThinkingDelta(text) => {
                self.send(Change::AgentThoughtChunk {
                    content: Chunk::Text { text },
                    message_id: self.assistant.as_deref(),
                })
                .await;
            }
            Frame::UserMessage {
                text,
                client,
                message,
                ..
            } => {
                if self.turn.is_some() && self.message.is_none() && client == Some(self.client) {
                    self.message = message;
                }
                let id = self.name_message("user");
                self.titled(&text).await;
                self.send(Change::UserMessageChunk {
                    content: Chunk::Text { text },
                    message_id: Some(&id),
                })
                .await;
            }
            Frame::MessageState { message, state } if self.message == Some(message) => {
                match state {
                    Settled::Landed => self.started = true,
                    Settled::Cancelled => {
                        if let Some(id) = self.turn.take() {
                            self.editor
                                .respond(id, json!({"stopReason": "cancelled"}))
                                .await;
                        }
                    }
                    Settled::Dropped => {
                        if let Some(id) = self.turn.take() {
                            self.editor
                                .fail(id, -32603, "the session dropped the prompt", None)
                                .await;
                        }
                    }
                }
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
                    name: &name,
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
                    raw_output: None,
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
                        text: Cow::Owned(text.clone()),
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
                    raw_output: Some(&text),
                    locations: Vec::new(),
                    content,
                })
                .await;
            }
            Frame::Error(text) => {
                // A run error while this session's own mode ask waits is the
                // refusal of a guard, which keeps the mode the session had.
                if !self.refused(text.clone()).await {
                    // The error answer of the turn names the failure itself, so
                    // the notice is the live hint an editor shows while it runs.
                    if self.notices {
                        self.send(Change::Notice {
                            severity: Severity::Error,
                            title: String::from("the request failed"),
                            description: Some(text.to_string()),
                        })
                        .await;
                    }
                    self.failure = Some(text.into_owned());
                }
            }
            // A v1 editor takes a notice when it advertises one, and the wait
            // shows as a thought otherwise, where it does not join the reply.
            Frame::Retry { attempt, error } => {
                if self.notices {
                    self.send(Change::Notice {
                        severity: Severity::Warning,
                        title: format!("retry {attempt}"),
                        description: Some(error.to_string()),
                    })
                    .await;
                } else {
                    self.send(Change::AgentThoughtChunk {
                        content: Chunk::Text {
                            text: Cow::Owned(format!("retry {attempt}: {error}\n")),
                        },
                        message_id: self.assistant.as_deref(),
                    })
                    .await;
                }
            }
            Frame::ApprovalRequest { request, saves } => {
                self.approval(request, !saves.is_empty()).await;
            }
            Frame::ApprovalResolved { id } => {
                self.withdraw(|waiting| {
                    matches!(waiting, Waiting::Approval { id: other, .. } if *other == id)
                })
                .await;
            }
            Frame::QuestionRequest { id, questions } => self.questions(id, questions).await,
            Frame::QuestionResolved { id } => {
                self.withdraw(|waiting| match waiting {
                    Waiting::Question { id: other, .. } | Waiting::Elicit { id: other, .. } => {
                        *other == id
                    }
                    Waiting::Approval { .. } => false,
                })
                .await;
            }
            Frame::Settings {
                model,
                context_window,
                level,
                mode,
                ..
            } => {
                if context_window.is_some() {
                    self.window = context_window;
                }
                self.settings(&model, level.as_deref(), mode.as_deref())
                    .await;
            }
            Frame::Usage {
                input,
                cached,
                output,
                cost,
            } => {
                let used = input.saturating_add(cached).saturating_add(output);
                // The window of the model, or the budget the run keeps the
                // session under when the model's window is not known.
                let size = self
                    .window
                    .unwrap_or_else(|| crate::run::compact_budget(None));
                self.send(Change::UsageUpdate {
                    used,
                    size,
                    cost: cost.map(|amount| Cost {
                        amount,
                        currency: "USD",
                    }),
                })
                .await;
            }
            Frame::PluginCommands(commands) => {
                let plugins: Vec<Plugin> = commands
                    .into_iter()
                    .map(|command| Plugin {
                        name: command.name.to_string(),
                        description: command.description.to_string(),
                    })
                    .collect();
                let news = plugins.len() != self.plugins.len()
                    || plugins.iter().zip(&self.plugins).any(|(now, then)| {
                        now.name != then.name || now.description != then.description
                    });
                self.plugins = plugins;
                if news {
                    self.offer().await;
                }
            }
            Frame::RunEnd { end } => self.pending = Some(end),
            // What a command prints is its reply, since no model writes one.
            Frame::Note(text) if self.command.is_some() => {
                self.send(Change::AgentMessageChunk {
                    content: Chunk::Text { text },
                    message_id: None,
                })
                .await;
            }
            Frame::CommandEnd { sent } if self.command.is_some() => {
                self.command = None;
                match (sent, self.ended.take()) {
                    // The command queued a message, and that run is over.
                    (true, Some((end, stop))) => {
                        self.started = true;
                        self.end(end, stop).await;
                    }
                    // The run ends the turn.
                    (true, None) => self.started = true,
                    (false, _) => {
                        let reason = if self.cancelled {
                            "cancelled"
                        } else {
                            "end_turn"
                        };
                        if let Some(id) = self.turn.take() {
                            self.editor.respond(id, json!({"stopReason": reason})).await;
                        }
                    }
                }
            }
            // The worker refused the command, and sends no end for it. A
            // refusal of another request, such as a set, leaves it running.
            Frame::Refused { id, text } if id.is_some() && id == self.command => {
                self.command = None;
                if let Some(id) = self.turn.take() {
                    self.editor.fail(id, -32603, &text, None).await;
                }
            }
            _ => {}
        }
    }

    /// Takes the settings of the session the worker reports, and sends the
    /// updates of the ones this editor has not been told: a `set` of another
    /// client, or a mode the guard of the session kept against the one a set
    /// asked for. A change of this session's own set is already in its answer,
    /// so it sends nothing.
    async fn settings(&mut self, model: &str, level: Option<&str>, mode: Option<&str>) {
        let mut news = self.news(Config::Model, model);
        news |= self.news(Config::Level, level.unwrap_or("off"));
        let mut remoded = false;
        if let Some(mode) = mode {
            remoded = self.news(Config::Mode, mode);
            news |= remoded;
            // The worker spoke about the mode, so the one it names is the one
            // the session runs: an ask that waited for it needs no revert.
            self.asked_mode = None;
        }
        if !news {
            return;
        }
        self.send(Change::ConfigOptionUpdate {
            config_options: self.options(),
        })
        .await;
        if remoded {
            self.send(Change::CurrentModeUpdate {
                current_mode_id: &self.state.mode,
            })
            .await;
        }
    }

    /// Takes `value` as the value of `config` the worker reported, and answers
    /// whether the editor has not been told of it: the answer to a set of this
    /// session names the value of that set already, so its notice is no news.
    fn news(&mut self, config: Config, value: &str) -> bool {
        if self.reported.value(config) == value {
            return false;
        }
        self.reported.keep(config, value);
        self.state.keep(config, value);
        let asked = self
            .asked
            .iter()
            .position(|(asked, wanted)| *asked == config && wanted == value);
        match asked {
            Some(at) => {
                self.asked.remove(at);
                false
            }
            // The worker moved this value, and this session did not: a marker
            // for it that still waits is one the worker sent no notice for, so
            // it would hide the next change to its value.
            None => {
                self.asked.retain(|(asked, _)| *asked != config);
                true
            }
        }
    }

    /// Tells the editor a run refused the mode an ask of this session named, as
    /// a run does when its guard refuses `bypass`: the mode the session had
    /// stands, and the editor hears both why and that. Answers whether the ask
    /// was refused.
    async fn refused(&mut self, reason: Cow<'_, str>) -> bool {
        let Some(asked) = self.asked_mode.take() else {
            return false;
        };
        // The one mode a run refuses is `bypass`.
        if asked != "bypass" {
            return false;
        }
        // A v1 editor has no notice update for a refusal, so the reason shows
        // where the waits do, ahead of the mode the run kept.
        self.send(Change::AgentThoughtChunk {
            content: Chunk::Text { text: reason },
            message_id: self.assistant.as_deref(),
        })
        .await;
        self.send(Change::ConfigOptionUpdate {
            config_options: self.options(),
        })
        .await;
        self.send(Change::CurrentModeUpdate {
            current_mode_id: &self.state.mode,
        })
        .await;
        true
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
        let (request_id, waiting) = self.editor.reserve();
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
            let reply = editor
                .request(request_id, waiting, "session/request_permission", params)
                .await;
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
    /// An editor that advertises the elicitation form takes the whole set as one
    /// form, a field per question. One single-choice question is otherwise what a
    /// permission request can carry: an option per label. Everything else is
    /// answered with no answer, which the worker reads as a skip.
    async fn questions(&mut self, id: u64, questions: Vec<Question<'_>>) {
        if self.elicits {
            self.elicit(id, questions).await;
            return;
        }
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
                message_id: self.assistant.as_deref(),
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
        let (request_id, waiting) = self.editor.reserve();
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
            let reply = editor
                .request(request_id, waiting, "session/request_permission", params)
                .await;
            let _ = commands
                .send(Command::Answer {
                    request: request_id,
                    reply,
                })
                .await;
        });
    }

    /// Asks the editor the question set of the worker's question `id` as one
    /// elicitation form, and answers it with the picks of the form.
    async fn elicit(&mut self, id: u64, questions: Vec<Question<'_>>) {
        let mut properties = serde_json::Map::with_capacity(questions.len());
        let mut names = Vec::with_capacity(questions.len());
        let mut required = Vec::with_capacity(questions.len());
        for (at, question) in questions.iter().enumerate() {
            let name = format!("q{at}");
            properties.insert(name.clone(), field(question));
            required.push(Value::String(name.clone()));
            names.push(name);
        }
        // The line above the form is the first question, which a form of one
        // question then reads as its own.
        let message = questions
            .first()
            .map(|question| question.q.to_string())
            .unwrap_or_default();
        let (request_id, waiting) = self.editor.reserve();
        self.open.insert(request_id, Waiting::Elicit { id, names });
        let params = Elicit {
            session_id: self.id.clone(),
            mode: ElicitMode::Form,
            message,
            requested_schema: json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
        };
        let editor = self.editor.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let reply = editor
                .request(request_id, waiting, "elicitation/create", params)
                .await;
            let _ = commands
                .send(Command::Answer {
                    request: request_id,
                    reply,
                })
                .await;
        });
    }

    /// Takes back the requests to the editor whose entry `gone` picks, which no
    /// answer will settle: the editor hears a cancel of each, and the entry goes.
    async fn withdraw(&mut self, gone: impl Fn(&Waiting) -> bool) {
        let ids: Vec<u64> = self
            .open
            .iter()
            .filter(|(_, waiting)| gone(waiting))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.open.remove(&id);
            self.editor.withdraw(id).await;
        }
    }

    /// Answers the open turn with why the run ended.
    async fn end(&mut self, end: RunEnd, stop: Option<RunStop>) {
        self.withdraw(|_| true).await;
        self.tools.clear();
        // The run of a command that is not done yet is the one it queued, and
        // the command says so when it ends.
        if self.command.is_some() {
            self.ended = Some((end, stop));
            return;
        }
        // A prompt queued behind the run of another client waits for its own,
        // and the error of that other run is not its error.
        if !self.started {
            self.failure = None;
            return;
        }
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
            RunEnd::Stopped if stop == Some(RunStop::MaxTokens) => {
                self.editor
                    .respond(id, json!({"stopReason": "max_tokens"}))
                    .await;
            }
            RunEnd::Stopped if stop == Some(RunStop::Refusal) => {
                self.editor
                    .respond(id, json!({"stopReason": "refusal"}))
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

/// One question of an elicitation form: the field its answer is typed into.
fn field(question: &Question<'_>) -> Value {
    let title = question.q.to_string();
    let labels = || -> Vec<Value> {
        question
            .options
            .iter()
            .map(|option| json!({"const": &*option.label, "title": &*option.label}))
            .collect()
    };
    if question.options.is_empty() {
        // A question with no option takes any text.
        return json!({"type": "string", "title": title});
    }
    if question.multi {
        return json!({
            "type": "array",
            "title": title,
            "items": {"anyOf": labels()},
        });
    }
    json!({"type": "string", "title": title, "oneOf": labels()})
}

/// The answer of one field of a form, as the worker reads it: the option picked,
/// or the text typed.
fn picks(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
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
    // The ids of the replayed messages, as the live session names them.
    let mut named = 0u64;
    for message in &tail.messages {
        match message {
            Message::User { content } => {
                named += 1;
                let id = format!("user-{named}");
                for block in content {
                    if let Content::Text { text } = block {
                        note(
                            editor,
                            session,
                            Change::UserMessageChunk {
                                content: Chunk::Text {
                                    text: Cow::Borrowed(&**text),
                                },
                                message_id: Some(&id),
                            },
                        )
                        .await;
                    }
                }
            }
            Message::Assistant(reply) => {
                named += 1;
                let id = format!("agent-{named}");
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
                                    message_id: Some(&id),
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
                                    message_id: Some(&id),
                                },
                            )
                            .await;
                        }
                        Content::ToolCall(call) => {
                            let parsed = serde_json::from_str::<Value>(&call.arguments).ok();
                            let locations = locations_of(parsed.as_ref(), cwd);
                            calls.push(Tool {
                                id: call.id.to_string(),
                                name: call.name.to_string(),
                                arguments: parsed.clone(),
                            });
                            // The two updates the live run sends for a call: its
                            // start, then its arguments.
                            note(
                                editor,
                                session,
                                Change::ToolCall {
                                    tool_call_id: &call.id,
                                    title: call.name.to_string(),
                                    name: &call.name,
                                    kind: kind_of(&call.name),
                                    status: Status::Pending,
                                    raw_input: None,
                                },
                            )
                            .await;
                            note(
                                editor,
                                session,
                                Change::ToolCallUpdate {
                                    tool_call_id: &call.id,
                                    status: Some(Status::InProgress),
                                    title: Some(title_of(&call.name, parsed.as_ref())),
                                    raw_input: parsed,
                                    raw_output: None,
                                    locations,
                                    content: Vec::new(),
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
                        raw_output: Some(cut(&result.content)),
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

/// The title a prompt names: its text cut as the session store cuts a title,
/// without the `@"path"` words that attach a file, and nothing when no word is
/// left.
pub(super) fn title_of_text(text: &str) -> Option<String> {
    let mut words = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("@\"") {
        words.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        rest = match after.find('"') {
            Some(end) => &after[end + 1..],
            None => "",
        };
        words.push(' ');
    }
    words.push_str(rest);
    let title = reagent_store::list::title(words.trim());
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// The milliseconds since the epoch now.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The line the editor shows for the value of an option: its first letter in
/// capitals.
fn titled(value: &str) -> String {
    let mut letters = value.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
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

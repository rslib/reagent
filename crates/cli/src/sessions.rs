//! The sessions a plugin starts, and the client the run joins each one with.
//!
//! A plugin's `reagent.sessions` never touches a worker. It hands a
//! [`SessionAsk`] to the channel this host reads, and the host starts the
//! worker, joins it over the socket protocol and settles the plugin's Promise.
//! One task per session reads its frames with [`Client`] and forwards them to
//! the plugin that started it, and only while that plugin listens on it.
//!
//! The run makes the channel up front and hands it to the plugins as they load,
//! because the channel costs nothing. The host task starts only when at least
//! one loaded plugin may start a session: a run whose plugins ask for none
//! spawns no task and sends nothing on the channel.
//!
//! The host runs on the run's tokio runtime, off the plugin thread. When the
//! run ends the host leaves every session and returns: a session keeps running
//! its current turn, then stops the way a worker stops when it has no client and
//! no run open.
//!
//! A worker a session starts loads the same plugins as the run that started it,
//! so it may ask for a session of its own. [`DEPTH`] stops that: every worker a
//! session starts is one deeper than its run, and a run of depth one or more
//! starts no session at all.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reagent_protocol::message::Mode;
use reagent_script::plugin::Plugins;
use reagent_script::plugin::SessionAsk;
use reagent_script::plugin::SessionOp;
use reagent_script::plugin::SessionReply;
use reagent_tui::client::Client;
use reagent_tui::client::Frame;
use reagent_tui::client::Next;
use reagent_tui::client::RunEnd;
use serde_json::json;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// The most sessions one plugin may have open at once.
const LIMIT: usize = 8;

/// How long the end of a run waits for one client to leave its session.
const DETACH: Duration = Duration::from_secs(2);

/// The variable that says how many sessions deep a run is: a run that starts no
/// session has none, and a worker a session starts gets its run's depth plus
/// one.
const DEPTH: &str = "REAGENT_SESSION_DEPTH";

/// How many sessions deep this run is, which is 0 for a run no session started.
pub(crate) fn depth() -> u32 {
    std::env::var(DEPTH)
        .ok()
        .and_then(|given| given.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether this run may start no session, which is a run a session started: its
/// plugins load, guard hooks and all, and every `reagent.sessions` call rejects.
///
/// A session of a session would end only at the end of the machine, so one
/// level is all a run takes.
pub(crate) fn sealed() -> bool {
    depth() >= 1
}

/// The channel a plugin's `reagent.sessions` hands its calls to, and the side a
/// host serves them from.
///
/// Making the channel starts no task: a run hands the sender to its plugins as
/// they load and starts [`Sessions::serve`] only when one of them may use it.
pub(crate) fn channel() -> (UnboundedSender<SessionAsk>, UnboundedReceiver<SessionAsk>) {
    tokio::sync::mpsc::unbounded_channel()
}

/// The words a session's worker starts with: this run's own flags without the
/// task it runs and without the flags a worker cannot take, which
/// [`crate::tui::new_flags`] drops.
pub(crate) fn inherited() -> Vec<OsString> {
    crate::tui::new_flags(std::env::args_os().skip(1))
}

/// The run's handle on the sessions its plugins start.
///
/// A run whose plugins ask for no session takes [`Sessions::none`]: no host
/// task, and no frame reaches anything.
pub(crate) struct Sessions {
    /// The host's task, which is waited on when the run ends.
    host: Option<tokio::task::JoinHandle<()>>,
    /// Tells the host the run ended. Dropping it stops a host that never read
    /// it, so the end of a run never waits on a call it never made.
    stop: Option<oneshot::Sender<()>>,
    /// The plugins of the run, which the frames of a session reach.
    plugins: Arc<OnceLock<Arc<Plugins>>>,
}

impl Sessions {
    /// A run whose plugins asked for no session.
    pub(crate) fn none() -> Self {
        Self {
            host: None,
            stop: None,
            plugins: Arc::default(),
        }
    }

    /// Serves `asks` for the plugins that may start a session, whose names are
    /// `speakers`, for a run whose state folder is `state`, which holds the
    /// discovery entries of its workers, and whose worker words are `worker`.
    ///
    /// `speakers` is empty when no plugin asks for a session, and then this is
    /// [`Sessions::none`]: the receiver of `asks` goes with it, and no task
    /// starts.
    pub(crate) fn serve(
        state: &Path,
        worker: Worker,
        asks: UnboundedReceiver<SessionAsk>,
        speakers: Vec<String>,
    ) -> Self {
        if speakers.is_empty() {
            return Self::none();
        }
        let reach: Arc<OnceLock<Arc<Plugins>>> = Arc::default();
        // The sessions a starting task joined, which the host opens itself: a
        // start waits for a worker, and no other call of the run waits with it.
        let (made, joins) = tokio::sync::mpsc::unbounded_channel();
        let host = Host {
            state: state.to_path_buf(),
            worker,
            open: HashMap::new(),
            ended: Vec::new(),
            starting: HashMap::new(),
            made,
            speakers,
            reach: Arc::clone(&reach),
        };
        let (stop, stopped) = oneshot::channel();
        let host = tokio::task::spawn(host.serve(asks, joins, stopped));
        Self {
            host: Some(host),
            stop: Some(stop),
            plugins: reach,
        }
    }

    /// Whether a host task serves this run's sessions, which a run whose
    /// plugins ask for none has not.
    pub(crate) fn serving(&self) -> bool {
        self.host.is_some()
    }

    /// Names the plugins the frames of a session reach, once they loaded.
    pub(crate) fn attach(&self, plugins: Arc<Plugins>) {
        let _ = self.plugins.set(plugins);
    }

    /// Tells the host the run ended and waits until it left every session it
    /// held.
    pub(crate) async fn close(&mut self) {
        // The calls the plugins already made are answered first, then every
        // session is left and the host returns.
        self.stop = None;
        if let Some(host) = self.host.take() {
            let _ = host.await;
        }
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.stop = None;
        if let Some(host) = self.host.take() {
            // A run that never called `close` leaves its sessions the rough
            // way: the host stops and its connections go with it.
            host.abort();
        }
    }
}

/// The words and the state a session's worker starts with.
#[derive(Clone)]
pub(crate) struct Worker {
    /// The binary to run, which is this one.
    pub program: PathBuf,
    /// The run's own folder, which a `start` with no `cwd` uses.
    pub cwd: PathBuf,
    /// The flags of this run without its own task: its provider, its profile
    /// and its sandbox, which a session it starts inherits.
    pub flags: Vec<OsString>,
}

/// What one `start` call asks for: the task, the folder it works in, and the
/// model, the profile and the provider its worker starts on.
struct Start {
    /// The task, which is the first message of the session.
    task: String,
    /// The folder it works in, or `None` for the folder of the run.
    cwd: Option<PathBuf>,
    /// The model it starts on, or `None` for the one of the run.
    model: Option<String>,
    /// The config profile it reads, or `None` for the one of the run.
    profile: Option<String>,
    /// The provider it starts on, or `None` for the one of the run.
    provider: Option<String>,
    /// The tools its worker may use, or `None` for the tools of the run.
    tools: Option<String>,
}

/// The sessions of one run, on the run's runtime.
struct Host {
    /// The state folder of the run, which holds the discovery entries of the
    /// workers a session starts.
    state: PathBuf,
    /// What every session's worker starts with.
    worker: Worker,
    /// One entry per open session, by the id the worker named.
    open: HashMap<String, Open>,
    /// What the sessions whose task ended said, newest last: a `result` that
    /// comes after one of them answers it, and one that has nothing to say says
    /// the session is closed.
    ended: Vec<Ended>,
    /// The sessions whose worker is still starting, by the plugin that asked,
    /// which take a place in its limit until they are open.
    starting: HashMap<String, usize>,
    /// Tells the host a session is ready to open.
    made: UnboundedSender<Made>,
    /// The plugins that may start a session, in load order.
    speakers: Vec<String>,
    /// The plugins of the run, which a frame of a session reaches.
    reach: Arc<OnceLock<Arc<Plugins>>>,
}

/// A session whose task ended: the worker closed its socket, or the run left it.
struct Ended {
    /// The session, as its worker named it.
    id: String,
    /// The plugin that started it, which is the only plugin it answers.
    owner: String,
    /// Why its last run ended and its text, or `None` when it ended no run.
    last: Option<(String, String)>,
}

/// One open session of one plugin.
struct Open {
    /// The plugin that started it, which is the only plugin that sees its
    /// frames.
    owner: String,
    /// What the plugin asks its connection to do, since its task owns it.
    cmd: Sender<Cmd>,
    /// The text of its runs and the answers that wait for them.
    run: Arc<Mutex<Run>>,
    /// Whether the owner has a listener on this session.
    forward: Arc<AtomicBool>,
    /// The task that reads its frames.
    task: tokio::task::JoinHandle<()>,
}

/// A session a starting task joined, for the host to open on its own thread.
struct Made {
    /// The plugin that asked, whose place the start took.
    plugin: String,
    /// The session it made, the connection its task reads and the answer its
    /// `start` waits for, or `None` when the start failed and answered itself.
    open: Option<(String, Open, oneshot::Sender<SessionReply>)>,
}

/// What a session's task does to its connection, besides reading it.
enum Cmd {
    /// Queue or steer a message.
    Send {
        /// The text of the message.
        text: String,
        /// Queue it behind the run, or steer the run that is open.
        mode: Mode,
        /// Answers whether the worker took it.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Stop the turn that runs.
    Cancel {
        /// Answers whether the worker took it.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Leave the session, which is what the end of the run does.
    Detach {
        /// Answers once the connection is closed.
        reply: oneshot::Sender<()>,
    },
}

/// What the frames of one session have said so far.
#[derive(Default)]
struct Run {
    /// The assistant text of the turn that runs now.
    text: String,
    /// Why the last run ended and its text, kept until the next run starts, so
    /// every `result` that comes after the end answers it.
    last: Option<(String, String)>,
    /// The answers of the `result` calls that wait for the next run end.
    waiters: Vec<oneshot::Sender<SessionReply>>,
}

impl Run {
    /// Answers every `result` that waits with the run that just ended, which
    /// stays the last one for the calls that come after it.
    fn settle(&mut self) {
        if self.waiters.is_empty() {
            return;
        }
        let Some((reason, text)) = self.last.clone() else {
            return;
        };
        for waiter in std::mem::take(&mut self.waiters) {
            let _ = waiter.send(SessionReply::Ended {
                reason: reason.clone(),
                text: text.clone(),
            });
        }
    }
}

impl Host {
    /// Answers one call of one plugin's `reagent.sessions`, until the run ends
    /// and every session is left.
    async fn serve(
        mut self,
        mut asks: UnboundedReceiver<SessionAsk>,
        mut joins: UnboundedReceiver<Made>,
        mut stop: oneshot::Receiver<()>,
    ) {
        loop {
            // A call the plugins already made is answered before the end of the
            // run, which is why the calls come first.
            let next = tokio::select! {
                biased;
                asked = asks.recv() => asked.map(Job::Ask),
                made = joins.recv() => made.map(Job::Made),
                _ = &mut stop => None,
            };
            match next {
                Some(Job::Ask(ask)) => self.answer(ask).await,
                Some(Job::Made(made)) => self.opened(made),
                // The plugins went, or the run ended: no call is left to
                // answer.
                _ => break,
            }
        }
        // The run ended: every client leaves its session. A worker that still
        // runs its turn keeps running it, then stops with no client left.
        for (_, open) in std::mem::take(&mut self.open) {
            let (reply, done) = oneshot::channel();
            let sent = open.cmd.send(Cmd::Detach { reply }).await;
            if sent.is_err() || tokio::time::timeout(DETACH, done).await.is_err() {
                open.task.abort();
            }
        }
    }

    /// Answers one call of one plugin's `reagent.sessions`.
    async fn answer(&mut self, ask: SessionAsk) {
        // A session whose worker went frees its place, and leaves what it said,
        // before anything counts, lists or reads the sessions that are open.
        self.reap();
        let SessionAsk { plugin, op, reply } = ask;
        match op {
            SessionOp::Start {
                task,
                cwd,
                model,
                profile,
                provider,
                tools,
            } => {
                let it = Start {
                    task,
                    cwd,
                    model,
                    profile,
                    provider,
                    tools,
                };
                self.start(plugin, it, reply);
            }
            SessionOp::Send { id, text } => {
                let answer = self.send(&plugin, &id, text, Mode::Queue).await;
                let _ = reply.send(answer);
            }
            SessionOp::Steer { id, text } => {
                let answer = self.send(&plugin, &id, text, Mode::Steer).await;
                let _ = reply.send(answer);
            }
            SessionOp::Cancel { id } => {
                let _ = reply.send(self.cancel(&plugin, &id).await);
            }
            SessionOp::Result { id } => self.result(&plugin, &id, reply),
            SessionOp::Watch { id, forward } => {
                if let Some(open) = self.open.get(&id).filter(|open| open.owner == plugin) {
                    open.forward.store(forward, Ordering::Relaxed);
                }
            }
            SessionOp::List => {
                let mut ids: Vec<String> = self
                    .open
                    .iter()
                    .filter(|(_, open)| open.owner == plugin)
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.sort();
                let _ = reply.send(SessionReply::List(ids));
            }
        }
    }

    /// Takes the sessions whose task ended out of `open`, so a start may take
    /// their place and `list` leaves them out, and keeps what their last run
    /// said for a `result` that comes after them.
    fn reap(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let mut gone = Vec::new();
        self.open.retain(|id, open| {
            if !open.task.is_finished() {
                return true;
            }
            gone.push(Ended {
                id: id.clone(),
                owner: open.owner.clone(),
                last: lock(&open.run).last.clone(),
            });
            false
        });
        if gone.is_empty() {
            return;
        }
        self.ended.extend(gone);
        // A run that starts many sessions keeps the last few answers back: the
        // rest are the text of runs nobody asked about.
        let over = self.ended.len().saturating_sub(LIMIT);
        if over > 0 {
            self.ended.drain(..over);
        }
    }

    /// The answer a call about a session this plugin does not own gets.
    fn unknown(&self, id: &str) -> SessionReply {
        if self.open.contains_key(id) {
            SessionReply::Failed(format!("session {id} belongs to another plugin"))
        } else {
            SessionReply::Failed(format!("no open session {id}"))
        }
    }

    /// Starts a session for `plugin`, whose id is answered once its worker
    /// serves.
    ///
    /// The worker itself starts in a task of its own: a start waits for a
    /// process to come up, and the calls of this run do not wait with it.
    fn start(&mut self, plugin: String, it: Start, reply: oneshot::Sender<SessionReply>) {
        if !self.speakers.contains(&plugin) {
            let _ = reply.send(SessionReply::Failed(format!(
                "plugin {plugin} has no sessions permission"
            )));
            return;
        }
        // A start whose worker is still coming takes a place of its own, so
        // nine starts at once still leave the ninth out.
        let taken = self
            .open
            .values()
            .filter(|open| open.owner == plugin)
            .count()
            + self.starting.get(&plugin).copied().unwrap_or(0);
        if taken >= LIMIT {
            let _ = reply.send(SessionReply::Failed(format!(
                "plugin {plugin} has {LIMIT} sessions open, the limit"
            )));
            return;
        }
        *self.starting.entry(plugin.clone()).or_insert(0) += 1;
        let made = self.made.clone();
        let worker = self.worker.clone();
        let state = self.state.clone();
        let reach = Arc::clone(&self.reach);
        tokio::task::spawn(async move {
            let open = match launch(&worker, &state, &plugin, &it, reach).await {
                Ok((key, session)) => Some((key, session, reply)),
                Err(reason) => {
                    let _ = reply.send(SessionReply::Failed(reason));
                    None
                }
            };
            if let Err(unsent) = made.send(Made { plugin, open }) {
                // The run ended while this worker started: nothing serves it,
                // and the start that waits is answered instead of left waiting.
                if let Some((_, _, reply)) = unsent.0.open {
                    let _ = reply.send(SessionReply::Failed(String::from(
                        "the run ended before the session started",
                    )));
                }
            }
        });
    }

    /// Opens a session a starting task joined, and answers the `start` that
    /// asked for it.
    fn opened(&mut self, made: Made) {
        let Made { plugin, open } = made;
        if let Some(left) = self.starting.get_mut(&plugin) {
            *left = left.saturating_sub(1);
            if *left == 0 {
                self.starting.remove(&plugin);
            }
        }
        let Some((key, session, reply)) = open else {
            return;
        };
        self.open.insert(key.clone(), session);
        let _ = reply.send(SessionReply::Started(key));
    }

    /// Queues or steers `text` in the session `key`.
    async fn send(&self, plugin: &str, key: &str, text: String, mode: Mode) -> SessionReply {
        let Some(open) = self.open.get(key).filter(|open| open.owner == plugin) else {
            return self.unknown(key);
        };
        let (reply, answer) = oneshot::channel();
        let cmd = Cmd::Send { text, mode, reply };
        match open.cmd.send(cmd).await {
            Ok(()) => told(key, answer).await,
            Err(_) => SessionReply::Failed(format!("session {key} is closed")),
        }
    }

    /// Stops the turn the session `key` runs.
    async fn cancel(&self, plugin: &str, key: &str) -> SessionReply {
        let Some(open) = self.open.get(key).filter(|open| open.owner == plugin) else {
            return self.unknown(key);
        };
        let (reply, answer) = oneshot::channel();
        match open.cmd.send(Cmd::Cancel { reply }).await {
            Ok(()) => told(key, answer).await,
            Err(_) => SessionReply::Failed(format!("session {key} is closed")),
        }
    }

    /// Answers `reply` with the end of the run that ends next, or at once with
    /// the end of the last run, or with what a session that is gone left: a
    /// call never waits for a run that nothing starts.
    fn result(&self, plugin: &str, key: &str, reply: oneshot::Sender<SessionReply>) {
        if let Some(open) = self.open.get(key).filter(|open| open.owner == plugin) {
            let mut run = lock(&open.run);
            // The end of a run stays the answer of every `result` that comes
            // after it: a second call answers it at once instead of waiting for
            // a run that only a `send` starts.
            match &run.last {
                Some((reason, text)) => {
                    let _ = reply.send(SessionReply::Ended {
                        reason: reason.clone(),
                        text: text.clone(),
                    });
                }
                None => run.waiters.push(reply),
            }
            return;
        }
        let answer = match self.ended.iter().find(|ended| ended.id == key) {
            Some(ended) if ended.owner == plugin => match &ended.last {
                Some((reason, text)) => SessionReply::Ended {
                    reason: reason.clone(),
                    text: text.clone(),
                },
                None => SessionReply::Failed(format!("session {key} is closed")),
            },
            Some(_) => SessionReply::Failed(format!("session {key} belongs to another plugin")),
            None => self.unknown(key),
        };
        let _ = reply.send(answer);
    }
}

/// What the host reads next: a call of a plugin, or a session that is ready.
enum Job {
    /// A call of one plugin's `reagent.sessions`.
    Ask(SessionAsk),
    /// A session a starting task joined.
    Made(Made),
}

/// Starts the worker of one session and joins it, off the host's serve loop: a
/// start waits for a process, and no other call of the run waits with it.
async fn launch(
    worker: &Worker,
    state: &Path,
    plugin: &str,
    it: &Start,
    reach: Arc<OnceLock<Arc<Plugins>>>,
) -> Result<(String, Open), String> {
    let cwd = match &it.cwd {
        Some(given) => existing(given)
            .await
            .ok_or_else(|| format!("no folder {}", given.display()))?,
        None => worker.cwd.clone(),
    };
    let flags = worker_flags(&worker.flags, &cwd, it);
    // A worker a session starts is one session deeper than this run, and a run
    // that is that deep starts none of its own.
    let env = [(DEPTH.to_string(), (depth() + 1).to_string())];
    let log = crate::tui::log_path().await;
    let entry =
        crate::tui::spawn_worker(&worker.program, state, flags, log.as_deref(), &env).await?;
    // The worker names the session, and that name is what a plugin gets back
    // and passes to every other call.
    let key = entry.session;
    let name = format!("plugin:{plugin}");
    let (mut client, _init) = Client::connect(&entry.socket, &entry.token, &name)
        .await
        .map_err(|error| format!("could not join the worker of {key}: {error}"))?;
    client
        .attach(0)
        .await
        .map_err(|error| format!("could not attach to session {key}: {error}"))?;
    let run = Arc::new(Mutex::new(Run::default()));
    let forward = Arc::new(AtomicBool::new(false));
    let (cmd, commands) = tokio::sync::mpsc::channel(8);
    let reading = Reading {
        key: key.clone(),
        plugin: plugin.to_string(),
        run: Arc::clone(&run),
        forward: Arc::clone(&forward),
        reach,
    };
    let task = tokio::task::spawn(reading.frames(client, commands));
    let open = Open {
        owner: plugin.to_string(),
        cmd,
        run,
        forward,
        task,
    };
    Ok((key, open))
}

/// What a session's task answered a send or a cancel with, as the plugin's
/// call reads it.
async fn told(key: &str, answer: oneshot::Receiver<Result<(), String>>) -> SessionReply {
    match answer.await {
        Ok(Ok(())) => SessionReply::Done,
        Ok(Err(error)) => SessionReply::Failed(format!("session {key} is closed: {error}")),
        Err(_) => SessionReply::Failed(format!("session {key} is closed")),
    }
}

/// The frames of one session, read by its own task.
struct Reading {
    /// The session, as its worker names it.
    key: String,
    /// The plugin that started it, and the only one that sees its frames.
    plugin: String,
    /// The text of its runs and the answers that wait for them.
    run: Arc<Mutex<Run>>,
    /// Whether the plugin has a listener on it.
    forward: Arc<AtomicBool>,
    /// The plugins of the run, which a frame reaches.
    reach: Arc<OnceLock<Arc<Plugins>>>,
}

/// What one turn of a session's task came to.
enum Step {
    /// The reader gave a frame, which its own arm took, or a line that is not
    /// complete yet.
    Frame,
    /// The plugin asked for something on the connection.
    Cmd(Cmd),
    /// The worker closed the session, or the run's channel did.
    Closed,
}

impl Reading {
    /// Reads the frames of one session until its worker closes the connection,
    /// and carries out the sends and the cancels of the plugin that owns it.
    ///
    /// The read and the writes never share the connection: the frame arm takes
    /// what it read before a command writes, so the client is free for it.
    async fn frames(self, mut client: Client, mut commands: Receiver<Cmd>) {
        loop {
            let step = tokio::select! {
                frame = client.next_frame() => match frame {
                    Ok(Next::Frame(frame)) => {
                        self.take(frame);
                        Step::Frame
                    }
                    Ok(Next::More) => Step::Frame,
                    Ok(Next::End) | Err(_) => Step::Closed,
                },
                cmd = commands.recv() => match cmd {
                    Some(cmd) => Step::Cmd(cmd),
                    None => Step::Closed,
                },
            };
            match step {
                Step::Frame => {}
                Step::Closed => break,
                Step::Cmd(Cmd::Send { text, mode, reply }) => {
                    let out = client.send(&text, mode).await;
                    // A message the worker took starts the next run, and that
                    // run's end is the one a `result` after it answers.
                    if out.is_ok() {
                        lock(&self.run).last = None;
                    }
                    let _ = reply.send(out.map_err(|error| error.to_string()));
                }
                Step::Cmd(Cmd::Cancel { reply }) => {
                    let out = client.cancel().await;
                    let _ = reply.send(out.map_err(|error| error.to_string()));
                }
                Step::Cmd(Cmd::Detach { reply }) => {
                    let _ = client.detach().await;
                    let _ = reply.send(());
                    break;
                }
            }
        }
        // The worker went, or this run did: a `result` that still waits hears
        // that a run ended, and a `result` that comes later answers it, and
        // never waits for ever.
        let mut run = lock(&self.run);
        if run.last.is_none() {
            run.last = Some((String::from("error"), std::mem::take(&mut run.text)));
        }
        run.settle();
    }

    /// Records what one frame says, and hands the plugin the event it sees for
    /// it, when the frame has one and the plugin listens.
    fn take(&self, frame: Frame<'_>) {
        let listening = self.forward.load(Ordering::Relaxed);
        let event = match frame {
            Frame::TurnStart => {
                let mut run = lock(&self.run);
                run.text.clear();
                // A turn that starts is a run that started: the end of the run
                // before it is no longer what `result` answers.
                run.last = None;
                listening.then(|| json!({ "type": "turn_start" }))
            }
            Frame::TextDelta(text) => {
                lock(&self.run).text.push_str(&text);
                listening.then(|| json!({ "type": "text_delta", "text": text }))
            }
            Frame::ThinkingDelta(text) => {
                listening.then(|| json!({ "type": "thinking_delta", "text": text }))
            }
            Frame::ToolStart { id, name } => {
                listening.then(|| json!({ "type": "tool_start", "tool": name, "call": id }))
            }
            Frame::ToolCallReady { id, arguments, .. } => listening
                .then(|| json!({ "type": "tool_call_ready", "call": id, "arguments": arguments })),
            Frame::ToolResult {
                id, failed, error, ..
            } => listening.then(|| {
                json!({
                    "type": "tool_result",
                    "call": id,
                    "isError": failed,
                    "error": error,
                })
            }),
            Frame::TurnEnd => listening.then(|| json!({ "type": "turn_end" })),
            Frame::RunEnd { end } => {
                let reason = reason_of(end);
                let mut run = lock(&self.run);
                run.last = Some((reason.to_string(), std::mem::take(&mut run.text)));
                run.settle();
                listening.then(|| json!({ "type": "run_end", "reason": reason }))
            }
            Frame::UserMessage { text, .. } => {
                listening.then(|| json!({ "type": "user_message", "text": text }))
            }
            Frame::Error(message) => {
                listening.then(|| json!({ "type": "error", "message": message }))
            }
            _ => None,
        };
        if let (Some(event), Some(plugins)) = (event, self.reach.get()) {
            plugins.session_event(&self.plugin, &self.key, event);
        }
    }
}

/// The word a plugin's listener reads for the end of a run.
fn reason_of(end: RunEnd) -> &'static str {
    match end {
        RunEnd::EndTurn => "end_turn",
        RunEnd::Stopped => "stopped",
        RunEnd::MaxTurns => "max_turns",
        RunEnd::Cancelled => "cancelled",
        RunEnd::Error => "error",
        RunEnd::Other => "other",
    }
}

/// The folder `given` names, made absolute, or `None` when it is no folder.
async fn existing(given: &Path) -> Option<PathBuf> {
    let cwd = tokio::fs::canonicalize(given).await.ok()?;
    tokio::fs::metadata(&cwd)
        .await
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|_| cwd)
}

/// A lock that ignores poisoning: a panic in a frame leaves the text as it was.
fn lock(run: &Arc<Mutex<Run>>) -> std::sync::MutexGuard<'_, Run> {
    run.lock().unwrap_or_else(|error| error.into_inner())
}

/// The flags of the worker of one session: this run's own words, which
/// [`inherited`] already opens with `worker`, then the session's own, so its own
/// `--tools`, `--cwd`, `--provider` and `-p` come after the ones it inherits and
/// win. A `--tools` of its own replaces the list it inherits, since the last
/// list a run reads is the one it takes.
fn worker_flags(base: &[OsString], cwd: &Path, it: &Start) -> Vec<OsString> {
    let mut flags: Vec<OsString> = base.to_vec();
    flags.push(OsString::from("--cwd"));
    flags.push(cwd.as_os_str().to_owned());
    for (flag, value) in [
        ("--provider", &it.provider),
        ("--model", &it.model),
        ("--profile", &it.profile),
        ("--tools", &it.tools),
    ] {
        if let Some(value) = value {
            flags.push(OsString::from(flag));
            flags.push(OsString::from(value));
        }
    }
    flags.push(OsString::from("-p"));
    flags.push(OsString::from(&it.task));
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_session_flags_put_the_session_last() {
        let it = Start {
            task: String::from("a task"),
            cwd: Some(PathBuf::from("/work")),
            model: Some("p/m".into()),
            profile: Some("dev".into()),
            provider: Some("fake:y.json".into()),
            tools: None,
        };
        let flags = worker_flags(
            &[
                OsString::from("worker"),
                OsString::from("--provider"),
                OsString::from("fake:x.json"),
            ],
            Path::new("/work"),
            &it,
        );
        let words: Vec<String> = flags
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            words,
            [
                "worker",
                "--provider",
                "fake:x.json",
                "--cwd",
                "/work",
                "--provider",
                "fake:y.json",
                "--model",
                "p/m",
                "--profile",
                "dev",
                "-p",
                "a task",
            ]
        );
    }

    /// A start of a session whose own `tools` come after the ones it inherits:
    /// the list of `--tools` the worker reads is the last one.
    #[test]
    fn plugin_session_tools_follow_the_inherited_flags() {
        let it = Start {
            task: String::from("a task"),
            cwd: None,
            model: None,
            profile: None,
            provider: None,
            tools: Some("read,grep".into()),
        };
        let flags = worker_flags(
            &[
                OsString::from("worker"),
                OsString::from("--tools"),
                OsString::from("read,write"),
            ],
            Path::new("/work"),
            &it,
        );
        let words: Vec<String> = flags
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            words,
            [
                "worker",
                "--tools",
                "read,write",
                "--cwd",
                "/work",
                "--tools",
                "read,grep",
                "-p",
                "a task",
            ]
        );
    }

    /// A run whose plugins may start no session spawns no host task, and the
    /// channel of a call it never makes is gone.
    #[tokio::test]
    async fn plugin_session_no_speaker_starts_no_host() {
        let (talk, asks) = channel();
        let sessions = Sessions::serve(Path::new("/none"), worker(), asks, Vec::new());
        assert!(!sessions.serving(), "a host task started with no speaker");
        let (reply, _answer) = oneshot::channel();
        let ask = SessionAsk {
            plugin: String::from("boss"),
            op: SessionOp::List,
            reply,
        };
        assert!(talk.send(ask).is_err(), "the call reached a host");
    }

    /// A run with a plugin that may start a session gets a host task, which the
    /// end of the run stops.
    #[tokio::test]
    async fn plugin_session_a_speaker_starts_the_host() {
        let (_talk, asks) = channel();
        let mut sessions = Sessions::serve(
            Path::new("/none"),
            worker(),
            asks,
            vec![String::from("boss")],
        );
        assert!(sessions.serving(), "no host task started for a speaker");
        sessions.close().await;
        assert!(!sessions.serving(), "the host task outlived the run");
    }

    /// A start whose worker prints a reason instead of its ready line answers
    /// with that reason, and not with a hang.
    #[tokio::test]
    async fn plugin_session_a_start_without_a_ready_line_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("a temp folder");
        let script = root.path().join("worker.sh");
        tokio::fs::write(
            &script,
            "#!/bin/sh\necho 'the model of this session is missing' >&2\nsleep 1\nexit 1\n",
        )
        .await
        .expect("the script");
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .await
            .expect("a runnable script");
        let state = root.path().join("state");
        tokio::fs::create_dir_all(&state)
            .await
            .expect("the state folder");
        // The stderr of the worker goes to a log of this test's own, which it
        // removes: the name of a worker's log is this process's, and a test
        // leaves nothing of its own behind.
        let log = root.path().join("worker.log");
        let error = crate::tui::spawn_worker(&script, &state, Vec::new(), Some(&log), &[])
            .await
            .expect_err("a worker that prints no ready line started");
        assert!(
            error.contains("the model of this session is missing"),
            "the reason the worker printed: {error}"
        );
    }

    /// A host that serves the sessions of `speakers`, whose workers this test
    /// never starts.
    fn host(speakers: &[&str]) -> Host {
        Host {
            state: std::env::temp_dir(),
            worker: worker(),
            open: HashMap::new(),
            ended: Vec::new(),
            starting: HashMap::new(),
            made: tokio::sync::mpsc::unbounded_channel().0,
            speakers: speakers.iter().map(|name| name.to_string()).collect(),
            reach: Arc::default(),
        }
    }

    /// One session of `owner`: `alive` says whether its task still reads, and
    /// `last` is what its last run said.
    async fn open(owner: &str, alive: bool, last: Option<(&str, &str)>) -> Open {
        let run = Arc::new(Mutex::new(Run {
            last: last.map(|(reason, text)| (reason.to_string(), text.to_string())),
            ..Run::default()
        }));
        let (cmd, _commands) = tokio::sync::mpsc::channel(8);
        let task = if alive {
            tokio::task::spawn(std::future::pending::<()>())
        } else {
            // A task that ran to its end is finished, which is what a worker
            // that closed its socket leaves.
            let (done, went) = oneshot::channel::<()>();
            let task = tokio::task::spawn(async move {
                let _ = done.send(());
            });
            let _ = went.await;
            assert!(task.is_finished(), "the task of a closed session runs");
            task
        };
        Open {
            owner: owner.to_string(),
            cmd,
            run,
            forward: Arc::new(AtomicBool::new(false)),
            task,
        }
    }

    /// The sessions the plugin `plugin` has open, as `list` answers them.
    async fn list(host: &mut Host, plugin: &str) -> Vec<String> {
        let (reply, answer) = oneshot::channel();
        host.answer(SessionAsk {
            plugin: plugin.to_string(),
            op: SessionOp::List,
            reply,
        })
        .await;
        match answer.await.expect("an answer") {
            SessionReply::List(ids) => ids,
            other => panic!("list answered {other:?}"),
        }
    }

    /// The answer of one `start` of `plugin` with no folder that exists, which
    /// is a start that never reaches a worker.
    async fn start(host: &mut Host, plugin: &str) -> SessionReply {
        let (reply, answer) = oneshot::channel();
        host.answer(SessionAsk {
            plugin: plugin.to_string(),
            op: SessionOp::Start {
                task: String::from("say it"),
                cwd: Some(PathBuf::from("/no such folder")),
                model: None,
                profile: None,
                provider: None,
                tools: None,
            },
            reply,
        })
        .await;
        answer.await.expect("an answer")
    }

    /// The answer of one `result` of `plugin`, which must come at once.
    async fn result(host: &Host, plugin: &str, id: &str) -> SessionReply {
        let (reply, answer) = oneshot::channel();
        host.result(plugin, id, reply);
        tokio::time::timeout(Duration::from_secs(1), answer)
            .await
            .expect("a result that answers at once")
            .expect("an answer")
    }

    /// A session whose worker went frees its place: `list` leaves it out, a
    /// start takes the place it held, and its last run still answers.
    #[tokio::test]
    async fn plugin_session_a_closed_session_frees_its_place() {
        let mut host = host(&["boss"]);
        for at in 0..LIMIT {
            let id = format!("s{at}");
            host.open.insert(id, open("boss", true, None).await);
        }
        assert!(
            matches!(start(&mut host, "boss").await, SessionReply::Failed(reason) if reason.contains("limit")),
            "a ninth start was not refused"
        );
        // The worker of one of them closed its socket, and its task ended with
        // the run it read.
        host.open.insert(
            String::from("s0"),
            open("boss", false, Some(("end_turn", "bye"))).await,
        );
        let ids = list(&mut host, "boss").await;
        assert_eq!(ids.len(), LIMIT - 1, "the closed session is still listed");
        assert!(
            !ids.contains(&String::from("s0")),
            "the closed session: {ids:?}"
        );
        // Its place is free, so a start reaches the worker it names and not the
        // limit, and what it said is still there to read.
        assert!(
            matches!(start(&mut host, "boss").await, SessionReply::Failed(reason) if reason.contains("no folder")),
            "the place of a closed session was not freed"
        );
        assert_eq!(
            result(&host, "boss", "s0").await,
            SessionReply::Ended {
                reason: String::from("end_turn"),
                text: String::from("bye"),
            }
        );
    }

    /// A `result` after the end of a run answers it at once, and so does the
    /// one after that: the end of a run is not used up by a call.
    #[tokio::test]
    async fn plugin_session_result_answers_the_same_end_twice() {
        let mut host = host(&["boss"]);
        host.open.insert(
            String::from("s"),
            open("boss", true, Some(("end_turn", "hi"))).await,
        );
        let answered = SessionReply::Ended {
            reason: String::from("end_turn"),
            text: String::from("hi"),
        };
        for _ in 0..2 {
            assert_eq!(result(&host, "boss", "s").await, answered);
        }
    }

    /// A `result` of a session that is gone answers the end it left, and a
    /// session that ended no run says it is closed instead of waiting.
    #[tokio::test]
    async fn plugin_session_result_of_a_closed_session_never_waits() {
        let mut host = host(&["boss"]);
        host.open.insert(
            String::from("said"),
            open("boss", false, Some(("error", "gone"))).await,
        );
        host.open
            .insert(String::from("quiet"), open("boss", false, None).await);
        host.reap();
        assert_eq!(
            result(&host, "boss", "said").await,
            SessionReply::Ended {
                reason: String::from("error"),
                text: String::from("gone"),
            }
        );
        assert_eq!(
            result(&host, "boss", "quiet").await,
            SessionReply::Failed(String::from("session quiet is closed"))
        );
        // Another plugin reads neither of them.
        assert_eq!(
            result(&host, "other", "said").await,
            SessionReply::Failed(String::from("session said belongs to another plugin"))
        );
    }

    /// A worker the tests start with: this binary in this folder, with no flag.
    fn worker() -> Worker {
        Worker {
            program: std::env::current_exe().unwrap_or_default(),
            cwd: std::env::temp_dir(),
            flags: Vec::new(),
        }
    }
}

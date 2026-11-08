//! The worker of one session: it takes the session before it names it, serves
//! it to the clients that present its token, and stops when the last one
//! leaves.

use std::borrow::Cow;
use std::fs::Permissions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use reagent_agent::agent_loop::Agent;
use reagent_agent::event::Event;
use reagent_agent::event::TurnEndReason;
use reagent_agent::guard::Answer;
use reagent_agent::journal::Journal;
use reagent_agent::journal::resume::PendingCall;
use reagent_agent::live;
use reagent_agent::live::Inbox;
use reagent_agent::live::SendMode;
use reagent_agent::live::Sender;
use reagent_ai::Message;
use reagent_ai::Text;
use reagent_ai::oauth::session::Session;
use reagent_config::permissions::PermissionMode;
use reagent_protocol::discovery;
use reagent_protocol::discovery::Entry;
use reagent_protocol::hub;
use reagent_protocol::lines::Lines;
use reagent_protocol::message;
use reagent_protocol::message::InitializeResult;
use reagent_protocol::message::MAJOR;
use reagent_protocol::message::Request;
use reagent_protocol::message::RpcError;
use reagent_protocol::token;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::args::Args;
use crate::session::Plan;

mod fanout;

use fanout::Encoder;

/// How long a client has to send `initialize` after it connects.
const HANDSHAKE: Duration = Duration::from_secs(5);

/// How long a worker with no work of its own waits for its first client.
const FIRST_CLIENT: Duration = Duration::from_secs(10);

/// How long a worker stays up after a client goes without `detach`, so one the
/// hub cut can come back and catch up on what it missed.
const LINGER: Duration = Duration::from_secs(5);

/// The client id of the worker itself.
const WORKER: live::ClientId = 0;

/// The mode of the discovery folder.
const DIR_MODE: u32 = 0o700;

/// The mode of the lock file.
const FILE_MODE: u32 = 0o600;

/// The room one answer line needs before it grows.
const LINE: usize = 256;

/// The empty result of `detach`.
#[derive(Serialize)]
struct Empty {}

/// Starts the worker of `plan`, or returns `None` when this run is not one.
///
/// `model` is the provider and model text the run uses, `context_window` its
/// context window and `level` the reasoning level it sends: the answer to
/// `initialize` names the first two, and the `settings` notice every one.
///
/// A worker takes its session here: the lock, the socket and the entry of the discovery folder are all
/// there before the ready line goes out.
pub(crate) async fn start(
    args: &Args,
    plan: Option<&Plan>,
    cwd: &Path,
    model: &str,
    context_window: Option<u64>,
    level: Option<&str>,
) -> Result<Option<Worker>, String> {
    if !args.worker {
        return Ok(None);
    }
    let Some(plan) = plan else {
        return Err("a worker needs a session".to_string());
    };
    let state = crate::session::state_home()
        .ok_or_else(|| "no state folder; set HOME or XDG_STATE_HOME".to_string())?;
    let folder = discovery::folder(&state);
    let session = plan.id.clone();
    let (at, id) = (folder.clone(), session.clone());
    let lock = match tokio::task::spawn_blocking(move || take(&at, &id))
        .await
        .map_err(|_| "the worker lock failed".to_string())?
    {
        Ok(Some(lock)) => lock,
        Ok(None) => return Err(busy(&folder, &session).await),
        Err(error) => return Err(format!("{}: {error}", folder.display())),
    };
    let socket = discovery::socket_path(&folder, &session).map_err(|error| error.to_string())?;
    // From here every way out removes the socket and the entry, so an early
    // error leaves no file that names this worker.
    let files = Files {
        socket: socket.clone(),
        entry: folder.join(format!("{session}.json")),
    };
    // The lock says no live worker uses this path, so a file here is the
    // leftover of one that stopped.
    match tokio::fs::remove_file(&socket).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", socket.display())),
    }
    let listener =
        UnixListener::bind(&socket).map_err(|error| format!("{}: {error}", socket.display()))?;
    let token = token::new_token()
        .await
        .map_err(|error| format!("no join token: {error}"))?;
    let instance = token::new_token()
        .await
        .map_err(|error| format!("no instance id: {error}"))?;
    let entry = Entry {
        pid: std::process::id(),
        instance,
        major: MAJOR,
        token: token.clone(),
        socket: socket.clone(),
        session: session.clone(),
        cwd: cwd.to_path_buf(),
        started_ms: reagent_store::paths::now_ms(),
    };
    discovery::write(&folder, &entry)
        .await
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    ready(&session, &socket, entry.pid)
        .await
        .map_err(|error| format!("could not write the ready line: {error}"))?;
    // Finding the state folder and resolving the project folder touch the
    // file system, so they run on the blocking pool.
    let at = cwd.to_path_buf();
    let saved = tokio::task::spawn_blocking(move || {
        reagent_config::auth::dir()
            .ok()
            .map(|dir| reagent_config::saved::path(&dir, &at))
    })
    .await
    .ok()
    .flatten();
    let (sender, inbox) = live::channel();
    let model = Model::new(
        Arc::from(model),
        context_window,
        level.map(Arc::from),
        args.provider_name.as_deref().map(Arc::from),
        crate::models::cache_dir(),
    );
    Ok(Some(Worker {
        session,
        cwd: cwd.to_path_buf(),
        saved,
        _files: files,
        _lock: lock,
        token: Arc::from(token),
        listener: Some(listener),
        shared: Arc::new(Shared::new(model)),
        events: None,
        sender: Some(sender),
        inbox: Some(inbox),
        tracker: None,
        leaving: None,
        acceptor: None,
        stop: None,
        stopping: false,
    }))
}

/// The lock file of `session`, or `None` when another process holds it.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking, once per worker"
)]
fn take(folder: &Path, session: &str) -> io::Result<Option<Lock>> {
    std::fs::create_dir_all(folder)?;
    std::fs::set_permissions(folder, Permissions::from_mode(DIR_MODE))?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(FILE_MODE)
        .open(folder.join(format!("{session}.lock")))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// The reason a worker of `session` cannot start: the worker that holds it.
async fn busy(folder: &Path, session: &str) -> String {
    match discovery::list(folder)
        .await
        .into_iter()
        .find(|entry| entry.session == session)
    {
        Some(entry) => format!("session {session} has a worker (pid {})", entry.pid),
        None => format!("session {session} has a worker"),
    }
}

/// Prints the one line a worker writes on stdout.
async fn ready(session: &str, socket: &Path, pid: u32) -> io::Result<()> {
    let socket = serde_json::to_string(&socket.to_string_lossy()).map_err(io::Error::other)?;
    let session = serde_json::to_string(session).map_err(io::Error::other)?;
    let line =
        format!("{{\"type\":\"ready\",\"session\":{session},\"socket\":{socket},\"pid\":{pid}}}\n");
    let mut out = tokio::io::stdout();
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}

/// The open lock file of one session.
#[expect(
    clippy::disallowed_types,
    reason = "a lock file opened inside spawn_blocking and held open"
)]
struct Lock {
    /// Held open for the life of the worker; the lock goes when it drops.
    _file: std::fs::File,
}

/// The files that name a running worker, removed when it stops.
///
/// The lock file is never removed: a worker that unlinks it while it holds the
/// lock lets a second worker lock the unlinked file and a third a new one.
struct Files {
    socket: PathBuf,
    entry: PathBuf,
}

impl Drop for Files {
    #[expect(
        clippy::disallowed_methods,
        reason = "a drop cannot await; two unlinks when a worker stops"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.entry);
    }
}

/// One session served to its clients.
pub(crate) struct Worker {
    /// The session this worker serves.
    session: String,
    /// The folder its run works in, where an `@path` word of a message
    /// resolves.
    cwd: PathBuf,
    /// The file the rules a client saves go to, when the state folder has a
    /// path.
    saved: Option<PathBuf>,
    /// The socket and the entry, removed before the lock goes.
    _files: Files,
    /// The lock that keeps it the only worker of its session.
    _lock: Lock,
    /// The token a client must present. It is never printed.
    token: Arc<str>,
    /// The listener clients connect to, until it is handed to the acceptor.
    listener: Option<UnixListener>,
    /// What the client tasks and the tracker share.
    shared: Arc<Shared>,
    /// The events of the agent, until the tracker takes them.
    events: Option<mpsc::Receiver<Event>>,
    /// The side the worker sends its own commands on.
    sender: Option<Sender>,
    /// The commands of the session.
    inbox: Option<Inbox>,
    /// The task that reads the events of the run.
    tracker: Option<JoinHandle<TurnEndReason>>,
    /// Fires when the last client leaves.
    leaving: Option<oneshot::Receiver<()>>,
    /// The task that accepts clients; the connections are its own tasks and
    /// end with it.
    acceptor: Option<JoinHandle<()>>,
    /// Cancelled by a signal: the worker stops whether clients are attached or
    /// not.
    stop: Option<CancellationToken>,
    /// Whether the worker stopped serving before its session loop began.
    stopping: bool,
}

impl Worker {
    /// Takes the events of the agent, which the tracker reads.
    pub(crate) fn watch(&mut self, events: mpsc::Receiver<Event>) {
        self.events = Some(events);
    }

    /// Queues the message the worker was started with, as its own.
    pub(crate) async fn task(&mut self, text: String) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let blocks = match crate::attach::blocks(&self.cwd, &text).await {
            Ok(blocks) => blocks,
            Err(message) => {
                eprintln!("reagent: {message}");
                Vec::new()
            }
        };
        self.shared.queued();
        let command = live::Command::Send {
            client: WORKER,
            text: Text::from(text),
            blocks,
            mode: SendMode::Queue,
        };
        let _ = sender.send(command).await;
    }

    /// Starts serving clients: the tracker, the acceptor, and the record a
    /// client resumes its view from.
    ///
    /// `committed` is the number of the session's last record before this run,
    /// and `listed` is the session the tracker lists when the journal commits a
    /// record after it, or `None` for a session the list holds already.
    pub(crate) fn begin(&mut self, committed: u64, listed: Option<crate::session::Listing>) {
        self.shared.set_committed(committed);
        if let Some(events) = self.events.take() {
            let saved = self.saved.take();
            self.tracker = Some(tokio::spawn(track(
                events,
                Arc::clone(&self.shared),
                saved,
                committed,
                listed,
            )));
        }
        let (leaving, wait) = oneshot::channel();
        self.leaving = Some(wait);
        let Some(listener) = self.listener.take() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let token = Arc::clone(&self.token);
        let sender = self.sender.clone();
        let session: Arc<str> = Arc::from(self.session.as_str());
        let cwd = self.cwd.clone();
        self.acceptor = Some(tokio::spawn(async move {
            accept(listener, shared, token, sender, session, cwd).await;
            let _ = leaving.send(());
        }));
    }

    /// Stops the worker when `stop` is cancelled, as a signal does.
    pub(crate) fn stop_on(&mut self, stop: CancellationToken) {
        self.stop = Some(stop);
    }

    /// Runs the calls a resumed session left open, answered by its clients.
    ///
    /// The worker leaves as it does from its session loop: when the last client
    /// is gone while the calls wait for an approval, or on a signal. The
    /// session loop then does not start.
    pub(crate) async fn pending(
        &mut self,
        agent: &mut Agent<'_>,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        pending: &[PendingCall],
    ) -> TurnEndReason {
        self.shared.opened();
        let stop = self.stop.clone().unwrap_or_default();
        let (Some(inbox), Some(sender)) = (self.inbox.as_mut(), self.sender.as_ref()) else {
            return TurnEndReason::Error;
        };
        let run = agent.run_pending_with_inbox(history, journal, events, pending, inbox);
        tokio::pin!(run);
        let reason = match self.leaving.as_mut() {
            Some(leaving) => tokio::select! {
                reason = &mut run => reason,
                _ = leaving => {
                    self.stopping = true;
                    let _ = sender.send(live::Command::Cancel { client: WORKER }).await;
                    run.await
                }
                () = stop.cancelled() => {
                    self.stopping = true;
                    run.await
                }
            },
            None => run.await,
        };
        self.shared.settled();
        reason
    }

    /// Runs the session from the inbox until the last client leaves.
    pub(crate) async fn serve(
        &mut self,
        agent: &mut Agent<'_>,
        history: &mut Vec<Message>,
        journal: Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) {
        // The worker's own calls are done or queued: from here the run state
        // comes from the events, and a client that leaves is missed.
        self.shared.armed();
        let acceptor = self.acceptor.take();
        if self.stopping {
            stop_accepting(acceptor).await;
            return;
        }
        let Some(mut inbox) = self.inbox.take() else {
            return;
        };
        let sender = self.sender.take();
        let shared = Arc::clone(&self.shared);
        let stop = self.stop.clone().unwrap_or_default();
        let mut leaving = self.leaving.take();
        let live = live::run_live(agent, history, journal, events, &mut inbox);
        tokio::pin!(live);
        let Some(leaving) = leaving.as_mut() else {
            live.await;
            return;
        };
        tokio::select! {
            () = &mut live => {}
            _ = leaving => {
                // A run that waits for an answer no client can give is
                // cancelled, then the senders go so the session returns.
                if shared.waits()
                    && let Some(sender) = &sender
                {
                    let _ = sender.send(live::Command::Cancel { client: WORKER }).await;
                }
                drop(sender);
                live.await;
            }
            () = stop.cancelled() => {
                // The signal cancelled the run; the connections go with the
                // acceptor, and with them the last senders of the session.
                drop(sender);
                stop_accepting(acceptor).await;
                live.await;
                return;
            }
        }
        stop_accepting(acceptor).await;
    }

    /// How the last run of the session ended, once every event is seen.
    pub(crate) async fn reason(&mut self) -> TurnEndReason {
        match self.tracker.take() {
            Some(tracker) => tracker.await.unwrap_or(TurnEndReason::Error),
            None => self.shared.end(),
        }
    }
}

/// The session as the worker sees it now.
#[derive(Debug)]
struct State {
    /// Whether a run is open.
    running: bool,
    /// The approval the open run waits for, if it waits for one.
    approval: Option<u64>,
    /// Whether the worker has queued its own work.
    armed: bool,
    /// The newest record the journal committed.
    committed: u64,
    /// The clients that joined and have not left.
    clients: u32,
    /// Whether a client has ever joined.
    seen: bool,
    /// The deadline a client that left without `detach` keeps the worker up to,
    /// or `None` when no such client left.
    linger: Option<tokio::time::Instant>,
    /// The id the next client gets.
    next: live::ClientId,
    /// Why the last run ended.
    end: TurnEndReason,
}

impl Default for State {
    fn default() -> Self {
        Self {
            running: false,
            approval: None,
            armed: false,
            committed: 0,
            clients: 0,
            seen: false,
            linger: None,
            next: 1,
            end: TurnEndReason::EndTurn,
        }
    }
}

impl State {
    /// Takes one event of the run.
    fn observe(&mut self, event: &Event) {
        match event {
            Event::UserMessage { .. } | Event::TurnStart => self.running = true,
            Event::RunEnd(reason) => {
                self.running = false;
                self.approval = None;
                self.end = *reason;
            }
            Event::ApprovalRequest(request) => self.approval = Some(request.id),
            Event::ApprovalResolved { .. } => self.approval = None,
            Event::Committed { record } => self.committed = self.committed.max(*record),
            _ => {}
        }
    }

    /// Whether the worker may stop: no client is left, its own work is queued,
    /// no run is open (or the open run waits for an approval no client can
    /// give), and the linger of a client that left without `detach` is over.
    fn idle(&self, now: impl FnOnce() -> tokio::time::Instant) -> bool {
        // `now` runs only when a linger is set, so a run a client watches
        // reads no clock here.
        self.alone() && self.linger.is_none_or(|at| at <= now())
    }

    /// Whether the worker may stop now, given that it may still be waiting for
    /// its first client, which the first one that joins ends.
    fn may_stop(&self, waiting: bool) -> bool {
        self.idle(tokio::time::Instant::now) && (!waiting || self.seen)
    }

    /// The deadline of the linger that keeps the worker up, or `None` when a
    /// client left with `detach` or none is left to come back.
    ///
    /// One timer at most: a client that is still attached, or a run that is
    /// open, needs no deadline of its own.
    fn lingering(&self, now: tokio::time::Instant) -> Option<tokio::time::Instant> {
        match self.linger {
            Some(at) if self.alone() && at > now => Some(at),
            _ => None,
        }
    }

    /// Whether no client is left, the worker queued its work, and no run is
    /// open, or the open run waits for an approval no client can give.
    fn alone(&self) -> bool {
        self.clients == 0 && self.armed && (!self.running || self.approval.is_some())
    }
}

/// The model a session runs, its context window and the settings a client set
/// last, which the answer to `initialize` and the `settings` notice name.
#[derive(Debug)]
struct Model {
    /// The provider and model text the run uses.
    text: Arc<str>,
    /// The context window of `text`, from the models cache or the config.
    window: Option<u64>,
    /// The reasoning level the session sends, when it sends one.
    level: Option<Arc<str>>,
    /// The permission mode the session decides with: the one the run started
    /// with, which is `default`, replaced by the mode of every
    /// `Event::Settings` that names one (the mode the session's guard took).
    mode: PermissionMode,
    /// The provider whose models cache names the window of another model.
    provider: Option<Arc<str>>,
    /// The models cache folder, when the run has one.
    cache: Option<PathBuf>,
}

impl Model {
    /// The model a run starts with: the level it sends and the cache its other
    /// models are looked up in.
    fn new(
        text: Arc<str>,
        window: Option<u64>,
        level: Option<Arc<str>>,
        provider: Option<Arc<str>>,
        cache: Option<PathBuf>,
    ) -> Self {
        Self {
            text,
            window,
            level,
            mode: PermissionMode::default(),
            provider,
            cache,
        }
    }

    /// The text a client sees for the provider model id `id`: the provider the
    /// run names and the id, as `crate::run::model_text` writes it for the
    /// start line and the answer to `initialize`, or `id` alone when the run
    /// names no provider.
    ///
    /// A `set` carries the bare provider model id, so its comparison with the
    /// model of the run and the state it leaves are this text.
    fn text_of(&self, id: &str) -> Arc<str> {
        match &self.provider {
            Some(name) => Arc::from(format!("{name}/{id}")),
            None => Arc::from(id),
        }
    }
}

/// The state every task of one worker shares.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// The frames of the session, and the queues of its clients.
    hub: Mutex<hub::Hub>,
    /// The model of the session and the settings a client set last.
    model: Mutex<Model>,
    /// The login of the provider a client switched to, kept so its refresh
    /// keeps working.
    held: Mutex<Option<Arc<Session>>>,
    /// Bumped on every change of `state`, so a wait wakes on it.
    changed: watch::Sender<u64>,
}

impl Shared {
    fn new(model: Model) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Mutex::new(State::default()),
            hub: Mutex::new(hub::Hub::new()),
            model: Mutex::new(model),
            held: Mutex::new(None),
            changed,
        }
    }

    /// Switches the model of the session to the provider `name` a client set,
    /// and keeps `session` alive for the session, so the refresh of a login
    /// keeps working.
    fn switch_provider(&self, name: &str, session: Option<Arc<Session>>) {
        self.model().provider = Some(Arc::from(name));
        // The login of the provider the session leaves is dropped: its
        // refresh would serve no request.
        *self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session;
    }

    /// The model of the session, locked for one short read or write.
    fn model(&self) -> MutexGuard<'_, Model> {
        self.model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues the `settings` notice to every attached client, and for the ones
    /// that attach after it.
    fn notice(&self, frame: hub::Frame) {
        self.hub().send(frame, hub::Kind::Other);
    }

    /// The hub, locked for one short step.
    fn hub(&self) -> MutexGuard<'_, hub::Hub> {
        // Every step is one change of the hub, so a panic elsewhere cannot
        // leave half a frame queued.
        self.hub
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes the client of `id` with the name it gave, its queue, and the
    /// signal that cuts it.
    fn listen(
        &self,
        id: live::ClientId,
        name: Text,
        queue: mpsc::Sender<hub::Entry>,
        closing: oneshot::Sender<()>,
    ) {
        self.hub().join(id, name, queue, closing);
    }

    /// Attaches the client of `id` for its request, and queues the frames it
    /// missed, and the answer says whether a run is open now.
    fn attach(&self, id: live::ClientId, request: Option<u64>) {
        let running = self.state().running;
        let mut frames = self.hub();
        frames.set_running(running);
        frames.attach(id, request);
    }

    /// Queues the answer in `out` to the client of `id`.
    fn reply(&self, id: live::ClientId, out: &[u8]) {
        self.hub().reply(id, out);
    }

    /// Takes the client of `id` out of the hub.
    fn left_client(&self, id: live::ClientId) {
        self.hub().leave(id);
    }

    /// The state, locked for one short read or write.
    fn state(&self) -> MutexGuard<'_, State> {
        // Every change is one assignment, so a panic elsewhere cannot leave
        // half a state here.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wakes every wait that looks at the state.
    fn tell(&self) {
        self.changed
            .send_modify(|seen| *seen = seen.wrapping_add(1));
    }

    /// Wakeups for the tasks that wait on the state.
    fn changed(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Takes one event of the run: the state, then the frame every client gets.
    fn observe(&self, event: &Event, frame: hub::Frame) {
        self.state().observe(event);
        let mut frames = self.hub();
        if let Event::Committed { record } = event {
            frames.set_committed(*record);
        }
        frames.send(frame, fanout::kind(event));
        drop(frames);
        self.tell();
    }

    /// Gives one client its id and counts it as joined.
    fn joined(&self) -> live::ClientId {
        let client = {
            let mut state = self.state();
            let client = state.next;
            state.next += 1;
            state.clients += 1;
            state.seen = true;
            // A client that came back ends the linger an earlier drop armed, so
            // a later `detach` stops the worker at once.
            state.linger = None;
            client
        };
        self.tell();
        client
    }

    /// Counts a client that detached as gone: the worker stops as soon as
    /// nothing else is open.
    fn left(&self) {
        let mut state = self.state();
        state.clients = state.clients.saturating_sub(1);
        drop(state);
        self.tell();
    }

    /// Counts a client that went away without `detach`, or that the hub cut, as
    /// gone, and keeps the worker up for [`LINGER`] so it can come back.
    fn dropped(&self) {
        let mut state = self.state();
        state.clients = state.clients.saturating_sub(1);
        state.linger = Some(tokio::time::Instant::now() + LINGER);
        drop(state);
        self.tell();
    }

    /// The newest record the journal committed.
    fn committed(&self) -> u64 {
        self.state().committed
    }

    /// Sets the newest record the journal committed.
    fn set_committed(&self, record: u64) {
        self.state().committed = record;
        self.hub().set_committed(record);
    }

    /// Whether the open run waits for an approval.
    fn waits(&self) -> bool {
        self.state().approval.is_some()
    }

    /// Notes that a message the worker sent waits for a run.
    fn queued(&self) {
        self.state().running = true;
        self.tell();
    }

    /// Notes that the calls a resumed session left open are done.
    fn settled(&self) {
        let mut state = self.state();
        state.running = false;
        state.approval = None;
        drop(state);
        self.tell();
    }

    /// Notes that the calls a resumed session left open run now: the worker's
    /// own work, which an approval with no client to answer it may stop.
    fn opened(&self) {
        let mut state = self.state();
        state.running = true;
        state.armed = true;
        drop(state);
        self.tell();
    }

    /// Notes that the worker has queued its own work.
    fn armed(&self) {
        self.state().armed = true;
        self.tell();
    }

    /// Why the last run ended.
    fn end(&self) -> TurnEndReason {
        self.state().end
    }
}

/// Ends the acceptor and, with it, every connection it serves.
async fn stop_accepting(acceptor: Option<JoinHandle<()>>) {
    if let Some(acceptor) = acceptor {
        acceptor.abort();
        let _ = acceptor.await;
    }
}

/// Accepts clients until the last one leaves and no run is open.
///
/// Each connection is a task of `joins`, so a connection still in its
/// handshake ends with the acceptor and holds no sender of the session after
/// it.
async fn accept(
    listener: UnixListener,
    shared: Arc<Shared>,
    token: Arc<str>,
    sender: Option<Sender>,
    session: Arc<str>,
    cwd: PathBuf,
) {
    let mut changed = shared.changed();
    let mut joins = tokio::task::JoinSet::new();
    // A worker waits a little for its first client, so the one that started
    // it can connect.
    let deadline = tokio::time::Instant::now() + FIRST_CLIENT;
    let mut waiting = true;
    loop {
        // The one deadline the wait needs: the linger of a client that left
        // without `detach`, armed only while that is all that keeps the worker
        // up.
        let linger = {
            let state = shared.state();
            if state.may_stop(waiting) {
                return;
            }
            if state.alone() && state.linger.is_some() {
                state.lingering(tokio::time::Instant::now())
            } else {
                None
            }
        };
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        joins.spawn(join(
                            stream,
                            Arc::clone(&shared),
                            Arc::clone(&token),
                            sender.clone(),
                            Arc::clone(&session),
                            cwd.clone(),
                        ));
                    }
                    // A client that left before its connection was accepted is
                    // not an error; any other failure stops the worker.
                    Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => {}
                    Err(_) => return,
                }
            }
            _ = changed.changed() => {}
            Some(_) = joins.join_next(), if !joins.is_empty() => {}
            () = tokio::time::sleep_until(deadline), if waiting => {
                waiting = false;
            }
            // The linger ran out: the next look at the state stops the worker.
            () = waited(linger) => {}
        }
    }
}

/// Waits for `until`, or for nothing when the worker has no linger armed.
async fn waited(until: Option<tokio::time::Instant>) {
    match until {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Serves one client: the handshake, then its requests until it detaches.
async fn join(
    stream: UnixStream,
    shared: Arc<Shared>,
    token: Arc<str>,
    sender: Option<Sender>,
    session: Arc<str>,
    cwd: PathBuf,
) {
    let mut stream = stream;
    // Only a process of this user may join a session.
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == nix::unistd::getuid().as_raw() => {}
        _ => return,
    }
    let mut reader = Reader::new();
    let mut out = Vec::with_capacity(LINE);
    // The handshake is the first line, and it owns what it reads: a client
    // that never sends one is closed instead of held.
    let first = tokio::time::timeout(HANDSHAKE, async {
        loop {
            let line = match reader.fetch(&mut stream).await {
                Ok(Read::Line(line)) => line,
                Ok(Read::More) => continue,
                Ok(Read::End) | Err(_) => return Err((None, RpcError::not_initialized())),
            };
            return match message::decode(line) {
                Ok((
                    id,
                    Request::Initialize {
                        major,
                        token: given,
                        client,
                        ..
                    },
                )) => Ok((id, major, given.into_owned(), client.name.into_owned())),
                Ok((id, _)) => Err((id, RpcError::not_initialized())),
                Err(error) => Err((None, error)),
            };
        }
    })
    .await;
    let (id, major, given, name) = match first {
        Ok(Ok(found)) => found,
        Ok(Err((id, error))) => {
            refuse(&mut stream, &mut out, id, error).await;
            return;
        }
        Err(_) => {
            refuse(&mut stream, &mut out, None, RpcError::not_initialized()).await;
            return;
        }
    };
    let Some(id) = id else {
        refuse(&mut stream, &mut out, None, RpcError::invalid_request()).await;
        return;
    };
    if major != MAJOR {
        refuse(&mut stream, &mut out, Some(id), RpcError::wrong_version()).await;
        return;
    }
    if !token::same(&given, &token) {
        refuse(&mut stream, &mut out, Some(id), RpcError::wrong_token()).await;
        return;
    }
    let client = shared.joined();
    let (queue, frames) = hub::channel();
    let (closing, cut) = oneshot::channel();
    shared.listen(client, Text::from(name), queue, closing);
    let (mut read, write) = stream.into_split();
    let mut writer = tokio::spawn(hub::write(write, frames, cut));
    let (model, context_window, level) = {
        let model = shared.model();
        (Arc::clone(&model.text), model.window, model.level.clone())
    };
    let body = InitializeResult {
        major: MAJOR,
        version: env!("CARGO_PKG_VERSION"),
        session: &session,
        client: u64::from(client),
        capabilities: &[],
        committed: shared.committed(),
        model: &model,
        context_window,
        level: level.as_deref(),
    };
    message::result(&mut out, id, &body);
    shared.reply(client, &out);
    // This connection holds the sender of the session, which keeps it open for
    // its client until the connection ends.
    let mut cut = false;
    // A client that detaches leaves at once; one that goes without it keeps
    // the worker up for [`LINGER`], so it can come back.
    let mut detached = false;
    loop {
        let next = tokio::select! {
            // The hub cut this client, or it went away: its connection ends
            // here, and the reading side ends with it.
            _ = &mut writer => {
                cut = true;
                break;
            }
            next = reader.fetch(&mut read) => next,
        };
        let (id, request) = match next {
            Ok(Read::Line(line)) => match message::decode(line) {
                Ok(decoded) => decoded,
                Err(error) => {
                    out.clear();
                    message::error(&mut out, None, &error);
                    shared.reply(client, &out);
                    continue;
                }
            },
            Ok(Read::More) => continue,
            Ok(Read::End) | Err(_) => break,
        };
        match request {
            // `from` is the record the client read in the journal itself: the
            // hub holds every frame after the last committed record, which is
            // all a client needs on top of that read.
            Request::Attach { .. } => shared.attach(client, id),
            Request::Send { text, mode } => {
                // The image files the message names by `@path` go with it. A
                // file that cannot be prepared is told to the client that named
                // it, and the text goes as it was written.
                let blocks = match crate::attach::blocks(&cwd, text.as_ref()).await {
                    Ok(blocks) => blocks,
                    Err(message) => {
                        let message = format!("{message}: the image was not attached");
                        let frame = Encoder::default().frame(&Event::Error(Text::from(message)));
                        shared.reply(client, &frame);
                        Vec::new()
                    }
                };
                let send = live::Command::Send {
                    client,
                    text: Text::from(text.as_ref()),
                    blocks,
                    mode: send_mode(mode),
                };
                deliver(&shared, sender.as_ref(), client, id, &mut out, send).await;
            }
            Request::Cancel => {
                deliver(
                    &shared,
                    sender.as_ref(),
                    client,
                    id,
                    &mut out,
                    live::Command::Cancel { client },
                )
                .await;
            }
            Request::Set { model, level, mode } => {
                let mode = match mode.map(|text| text.parse::<PermissionMode>()) {
                    Some(Ok(mode)) => Some(mode),
                    Some(Err(error)) => {
                        out.clear();
                        message::error(&mut out, id, &RpcError::invalid_params(error));
                        shared.reply(client, &out);
                        continue;
                    }
                    None => None,
                };
                // The model text is a bare provider model id, which the
                // session's provider takes, or the `provider/model` of another
                // provider, which the worker builds and switches to.
                let (model, provider, config, max_tokens) = match model {
                    Some(text) => match switched(&shared, text.as_ref()).await {
                        Ok(asked) => (
                            Some(asked.model),
                            asked.swap,
                            asked.config,
                            asked.max_tokens,
                        ),
                        Err(error) => {
                            out.clear();
                            message::error(&mut out, id, &error);
                            shared.reply(client, &out);
                            continue;
                        }
                    },
                    None => (None, None, None, None),
                };
                // A model the session does not run brings the limits of its
                // cache entry and the level it takes, so the first request it
                // serves is the one that model takes.
                let limits = match &model {
                    Some(id) => limits_of(&shared, id, level.as_deref(), max_tokens).await,
                    None => Limits::default(),
                };
                let settings = live::Settings {
                    provider,
                    config_provider: config,
                    model,
                    level: limits
                        .level
                        .or_else(|| level.map(|text| Text::from(text.as_ref()))),
                    mode,
                    images: limits.images,
                    max_tokens: limits.max_tokens,
                    compact_budget: limits.compact_budget,
                };
                let set = live::Command::Set { client, settings };
                // The settings the session applied come back as an event: the
                // notice every client gets is sent once the session has them.
                deliver(&shared, sender.as_ref(), client, id, &mut out, set).await;
            }
            Request::Compact { note } => {
                let compact = live::Command::Compact {
                    client,
                    note: note.map(|text| Text::from(text.as_ref())),
                };
                deliver(&shared, sender.as_ref(), client, id, &mut out, compact).await;
            }
            Request::Approve {
                id: ask,
                answer,
                reason,
            } => {
                let approve = live::Command::Approve {
                    client,
                    id: ask,
                    answer: answer_of(answer, reason),
                };
                deliver(&shared, sender.as_ref(), client, id, &mut out, approve).await;
            }
            Request::Detach => {
                out.clear();
                if let Some(id) = id {
                    message::result(&mut out, id, &Empty {});
                    shared.reply(client, &out);
                }
                detached = true;
                break;
            }
            Request::Initialize { .. } => {
                out.clear();
                message::error(&mut out, id, &RpcError::method_not_found());
                shared.reply(client, &out);
            }
        }
    }
    // The queued frames go out before the writer closes, and the worker sees
    // this client leave only after them.
    shared.left_client(client);
    // The writer drains what is queued, but a client that stopped reading
    // cannot hold the worker: after the bound it is closed as it is.
    if !cut
        && tokio::time::timeout(hub::CLOSE_WAIT, &mut writer)
            .await
            .is_err()
    {
        writer.abort();
    }
    if detached {
        shared.left();
    } else {
        shared.dropped();
    }
}

/// Sends one command to the session, and answers `id` once it is in.
///
/// It says whether the session's channel took the command.
async fn deliver(
    shared: &Shared,
    sender: Option<&Sender>,
    client: live::ClientId,
    id: Option<u64>,
    out: &mut Vec<u8>,
    command: live::Command,
) -> bool {
    let sent = match sender {
        Some(sender) => sender.send(command).await.is_ok(),
        None => false,
    };
    let Some(id) = id else {
        return sent;
    };
    out.clear();
    if sent {
        message::result(out, id, &Empty {});
    } else {
        message::error(
            out,
            Some(id),
            &RpcError::new(RpcError::INVALID_REQUEST, "the session is closed"),
        );
    }
    shared.reply(client, out);
    sent
}

/// Takes the settings a session applied, from its `Event::Settings`, and
/// returns the frame of the `settings` notice to send every client when the
/// model, the level or the mode changed.
///
/// The event names what the session kept, so the mode here is the one its guard
/// took: a mode the guard refused is not in the event, and a `set` the session
/// applied nothing of sends no event at all. The window of another model comes
/// from the models cache, which is a file: it is read off this task, and the
/// state changes after it.
async fn took_settings(shared: &Shared, set: &live::Settings) -> Option<hub::Frame> {
    // The `set` names the bare provider model id and the model of the run is
    // the text a client sees, so the two are compared as that text.
    let (asked, changed) = {
        let model = shared.model();
        let asked = set.model.as_deref().map(|id| model.text_of(id));
        let changed = asked
            .as_ref()
            .is_some_and(|text| text.as_ref() != &*model.text);
        (asked, changed)
    };
    // The window is looked up under the bare id, which is what the cache of the
    // provider lists and what the run looks up at its start.
    let window = match (&set.model, changed) {
        (Some(id), true) => window_of(shared, id.to_string()).await,
        _ => None,
    };
    let mut model = shared.model();
    let mut changed = changed;
    if let (Some(text), true) = (&asked, changed) {
        model.text = Arc::clone(text);
        model.window = window;
    }
    if let Some(level) = &set.level
        && model.level.as_deref() != Some(&**level)
    {
        model.level = Some(Arc::clone(level));
        changed = true;
    }
    // The mode is the one the session now decides with, since its guard kept
    // it: nothing here has to guess which mode the run started with.
    if let Some(mode) = set.mode
        && mode != model.mode
    {
        model.mode = mode;
        changed = true;
    }
    if !changed {
        return None;
    }
    let mut out = Vec::with_capacity(LINE);
    message::settings(
        &mut out,
        &message::SettingsNotice {
            model: &model.text,
            context_window: model.window,
            level: model.level.as_deref(),
            mode: model.mode.as_str(),
        },
    );
    Some(Arc::from(out))
}

/// The model a `set` of `text` sends, and the provider the session switches to
/// when `text` names another one.
#[derive(Debug)]
struct Asked {
    /// The provider model id the session takes.
    model: Text,
    /// The provider the session switches to, when it switches.
    swap: Option<live::Swap>,
    /// The config provider id of `model`, which a resume builds the provider
    /// from, when the session stays on a provider the config names.
    config: Option<Text>,
    /// The cap on reply tokens the config of another provider names.
    max_tokens: Option<u32>,
}

/// What a `set` that changes the model takes from the cache entry of the new
/// model.
#[derive(Debug, Default)]
struct Limits {
    /// Whether the new model takes images.
    images: Option<bool>,
    /// The cap on reply tokens of another provider's config.
    max_tokens: Option<u32>,
    /// The prompt size, in tokens, that compacts the new model's window.
    compact_budget: Option<u64>,
    /// The level the new model takes for the reasoning the session sends.
    level: Option<Text>,
}

/// The limits a `set` gives the session for the model `id`, read from the cache
/// entry of the provider the session now runs.
///
/// `asked` is the level the `set` carries, or `None` when it carries none, and
/// `max_tokens` is the cap the config of another provider names. A `set` of the
/// model the session runs and a cache that holds no entry for `id` change no
/// limit.
async fn limits_of(
    shared: &Shared,
    id: &str,
    asked: Option<&str>,
    max_tokens: Option<u32>,
) -> Limits {
    let mut limits = Limits {
        max_tokens,
        ..Limits::default()
    };
    let (cache, provider, current, changed) = {
        let model = shared.model();
        let changed = model.text_of(id).as_ref() != &*model.text;
        (
            model.cache.clone(),
            model.provider.clone(),
            model.level.clone(),
            changed,
        )
    };
    let (Some(cache), Some(provider), true) = (cache, provider, changed) else {
        return limits;
    };
    // The cache is a file, and the entry of a provider lists many models: it is
    // read off this task.
    let id = id.to_string();
    let entry =
        tokio::task::spawn_blocking(move || crate::run::cached_entry(&cache, Some(&provider), &id))
            .await
            .ok()
            .flatten();
    let Some(entry) = entry else {
        return limits;
    };
    limits.images = Some(entry.images != Some(false));
    limits.compact_budget = entry
        .context_window
        .map(|window| crate::run::compact_budget(Some(window)));
    let level = asked.or(current.as_deref());
    let mapped = level
        .and_then(|level| crate::run::reasoning_text(level, entry.reasoning.as_ref()))
        .map(Text::from);
    // A level the session already sends, which the new model maps to itself, is
    // no change: sending it would record the same settings again.
    limits.level = mapped.filter(|mapped| asked.is_some() || current.as_deref() != Some(&**mapped));
    limits
}

/// The model a `set` of `text` sends.
///
/// A text with no `/` is a bare model of the provider the session runs. So is a
/// prefix the config does not know, and the `provider/model` of that provider,
/// whose model part goes alone. The `provider/model` of another configured
/// provider builds that provider and switches the session to it; a switch that
/// fails leaves the session as it was and says why, naming the provider.
async fn switched(shared: &Shared, text: &str) -> Result<Asked, RpcError> {
    let Some((name, id)) = text.split_once('/') else {
        return Ok(Asked {
            model: Text::from(text),
            swap: None,
            config: None,
            max_tokens: None,
        });
    };
    let current = shared.model().provider.clone();
    if current.as_deref() == Some(name) {
        return Ok(Asked {
            model: Text::from(id),
            swap: None,
            config: current,
            max_tokens: None,
        });
    }
    // Naming the model reads the config files, which blocks.
    let asked = text.to_string();
    let next = tokio::task::spawn_blocking(move || crate::args::reparse(&asked))
        .await
        .map_err(|error| failed(name, &format!("the config could not be read: {error}")))?;
    let next = match next {
        Ok(crate::args::Outcome::Run(next)) if next.provider_name.as_deref() == Some(name) => next,
        // A prefix no provider has is part of the model id.
        Ok(_) => {
            return Ok(Asked {
                model: Text::from(text),
                swap: None,
                config: None,
                max_tokens: None,
            });
        }
        Err(message) => return Err(failed(name, &message)),
    };
    let (provider, session, _) = crate::run::build_provider(&next)
        .await
        .map_err(|message| failed(name, &message))?;
    shared.switch_provider(name, session);
    Ok(Asked {
        model: Text::from(next.model),
        swap: Some(live::Swap(Arc::from(provider))),
        config: Some(Text::from(name)),
        // The config of the new provider names the cap its model takes.
        max_tokens: Some(next.max_tokens),
    })
}

/// The error a `set` answers when the switch to the provider `name` fails.
fn failed(name: &str, reason: &str) -> RpcError {
    RpcError::invalid_params(format!("provider {name}: {reason}"))
}

/// The context window the models cache names for `text`, the model a `set`
/// asks for, or `None` when no cache names it.
async fn window_of(shared: &Shared, text: String) -> Option<u64> {
    let (cache, provider) = {
        let model = shared.model();
        (model.cache.clone()?, Arc::clone(model.provider.as_ref()?))
    };
    tokio::task::spawn_blocking(move || crate::run::window_of(&cache, &provider, &text))
        .await
        .ok()
        .flatten()
}

/// How a `send` joins the run.
fn send_mode(mode: message::Mode) -> SendMode {
    match mode {
        message::Mode::Steer => SendMode::Steer,
        message::Mode::Queue => SendMode::Queue,
    }
}

/// The answer a client gives to an approval request.
fn answer_of(kind: message::AnswerKind, reason: Option<Cow<'_, str>>) -> Answer {
    match kind {
        message::AnswerKind::Once => Answer::Once,
        message::AnswerKind::Session => Answer::Session,
        message::AnswerKind::Save => Answer::Save,
        message::AnswerKind::Refuse => Answer::Refuse {
            reason: reason.map(|text| Text::from(text.as_ref())),
        },
        message::AnswerKind::Skip => Answer::Skip,
    }
}

/// One attempt at the next line of a connection.
enum Read<'a> {
    /// The line, without its newline.
    Line(&'a [u8]),
    /// The buffer holds no line yet.
    More,
    /// The client closed the connection.
    End,
}

/// The reading side of one connection: one reused buffer of lines.
struct Reader {
    lines: Lines,
    /// Whether the buffer holds no complete line yet.
    missing: bool,
}

impl Reader {
    fn new() -> Self {
        Self {
            lines: Lines::new(),
            missing: false,
        }
    }

    /// The next line the buffer holds, reading one buffer more when needed.
    async fn fetch<R: AsyncRead + Unpin>(&mut self, stream: &mut R) -> io::Result<Read<'_>> {
        if self.missing {
            if self.lines.fill(stream).await? == 0 {
                return Ok(Read::End);
            }
            self.missing = false;
        }
        match self.lines.next() {
            Some(Ok(line)) => Ok(Read::Line(line)),
            Some(Err(too_long)) => Err(io::Error::other(too_long.to_string())),
            None => {
                self.missing = true;
                Ok(Read::More)
            }
        }
    }
}

/// Writes the line in `out` and clears it for the next one.
async fn flush(stream: &mut UnixStream, out: &mut Vec<u8>) -> io::Result<()> {
    let written = stream.write_all(out).await;
    out.clear();
    written
}

/// Answers `id` with `error` and writes the line.
async fn refuse(stream: &mut UnixStream, out: &mut Vec<u8>, id: Option<u64>, error: RpcError) {
    message::error(out, id, &error);
    let _ = flush(stream, out).await;
}

/// Takes the events of the run into the shared state and the frames every
/// client gets, and says how it ended.
///
/// The rule of a save is written to the project's saved-rules file off this
/// task: the observer runs under the shared lock, and a file write must not
/// hold it. The settings a session applied become the `settings` notice of the
/// clients instead of a frame of their own, since the notice names the same
/// values in the protocol a client reads them in. The writes left when the
/// events end are awaited here, so a session that saved a rule has it on disk
/// before the worker reports its end.
async fn track(
    mut events: mpsc::Receiver<Event>,
    shared: Arc<Shared>,
    saved: Option<PathBuf>,
    committed: u64,
    mut listed: Option<crate::session::Listing>,
) -> TurnEndReason {
    let mut encode = Encoder::default();
    let mut writes = Vec::new();
    while let Some(event) = events.recv().await {
        // A worker lists its session at its first committed record, so a
        // session it never runs a message in keeps its place in the list. A
        // resumed session starts after the records of the parts it continues.
        if let Event::Committed { record } = &event
            && *record > committed
            && let Some(listing) = listed.take()
        {
            match tokio::task::spawn_blocking(move || {
                crate::session::started(&listing.plan, listing.turns)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!("reagent: warning: could not record the session: {error}")
                }
                Err(_) => eprintln!("reagent: warning: the session store failed"),
            }
        }
        if let Event::RuleToSave { rule } = &event {
            let rule = rule.clone();
            let path = saved.clone();
            writes.push(tokio::task::spawn_blocking(move || match path {
                Some(path) => save(&path, &rule),
                None => eprintln!(
                    "reagent: the saved rule was not written: no state folder; set HOME or XDG_STATE_HOME"
                ),
            }));
        }
        if let Event::Settings { model, level, mode } = &event {
            let applied = live::Settings {
                model: model.clone(),
                level: level.clone(),
                mode: *mode,
                ..live::Settings::default()
            };
            if let Some(frame) = took_settings(&shared, &applied).await {
                shared.notice(frame);
            }
            continue;
        }
        let frame = encode.frame(&event);
        shared.observe(&event, frame);
    }
    for write in writes {
        let _ = write.await;
    }
    shared.end()
}

/// Adds `rule` to the file of `path`, and warns once when it cannot: a rule
/// that does not reach the file costs the next session one question, not the
/// run.
fn save(path: &Path, rule: &str) {
    if let Err(error) = reagent_config::saved::add(path, rule) {
        eprintln!(
            "reagent: could not save the rule in {}: {error}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use reagent_agent::event::ApprovalRequest;
    use reagent_agent::guard::Answer;

    use super::*;

    fn event_turn_start(state: &mut State) {
        state.observe(&Event::TurnStart);
    }

    #[test]
    fn a_run_is_open_from_its_first_turn_until_its_end() {
        let mut state = State::default();
        assert!(!state.running);
        event_turn_start(&mut state);
        assert!(state.running);
        state.observe(&Event::RunEnd(TurnEndReason::EndTurn));
        assert!(!state.running);
        assert_eq!(state.end, TurnEndReason::EndTurn);
    }

    #[test]
    fn a_user_message_opens_a_run_before_its_first_turn() {
        let mut state = State::default();
        state.observe(&Event::UserMessage {
            client: 1,
            mode: SendMode::Queue,
            text: Text::from("hi"),
            images: Vec::new(),
        });
        assert!(state.running, "a queued message is work in progress");
    }

    #[test]
    fn an_approval_holds_the_run_open_until_its_answer() {
        let mut state = State::default();
        event_turn_start(&mut state);
        state.observe(&Event::ApprovalRequest(Box::new(ApprovalRequest {
            id: 7,
            tool: Text::from("bash"),
            call: Text::from("rm -rf /"),
            rule: Text::from("default"),
            save: None,
        })));
        assert_eq!(state.approval, Some(7));
        state.observe(&Event::ApprovalResolved {
            id: 7,
            client: 1,
            answer: Answer::Once,
        });
        assert_eq!(state.approval, None);
        assert!(state.running, "the run goes on after the answer");
    }

    #[test]
    fn the_newest_committed_record_wins() {
        let mut state = State::default();
        state.observe(&Event::Committed { record: 4 });
        state.observe(&Event::Committed { record: 2 });
        assert_eq!(state.committed, 4);
    }

    #[test]
    fn idle_needs_no_client_a_queued_work_and_no_open_run() {
        let now = tokio::time::Instant::now();
        let mut state = State::default();
        assert!(!state.idle(|| now), "the worker has not queued its work");
        state.armed = true;
        assert!(state.idle(|| now));
        state.clients = 1;
        assert!(!state.idle(|| now), "a client is still here");
        state.clients = 0;
        event_turn_start(&mut state);
        assert!(!state.idle(|| now), "a run is open");
        state.approval = Some(3);
        assert!(state.idle(|| now), "no client can answer the approval");
    }

    #[test]
    fn a_client_that_left_without_detach_keeps_the_worker_up() {
        let mut state = State {
            armed: true,
            ..State::default()
        };
        let now = tokio::time::Instant::now();
        assert!(state.idle(|| now), "nothing is open and no client is left");
        state.linger = Some(now + LINGER);
        assert!(
            !state.idle(|| now),
            "a client that went without detach may come back"
        );
        assert_eq!(state.lingering(now), state.linger, "one deadline is armed");
        assert!(
            state.idle(|| now + LINGER),
            "the worker stops once the linger ran out"
        );
        assert_eq!(state.lingering(now + LINGER), None, "no timer is left");
        // A client that is back, or a run that is open, needs no timer.
        state.linger = Some(now + LINGER);
        state.clients = 1;
        assert_eq!(state.lingering(now), None, "a client is attached");
        state.clients = 0;
        event_turn_start(&mut state);
        assert_eq!(state.lingering(now), None, "a run is open");
        assert!(
            !state.idle(|| now + LINGER * 2),
            "a run that is open outlives the linger"
        );
    }

    #[test]
    fn a_worker_waits_for_a_first_client_until_one_joins() {
        let mut state = State {
            armed: true,
            ..State::default()
        };
        assert!(state.idle(tokio::time::Instant::now), "nothing is open");
        assert!(!state.may_stop(true), "a worker still waits for a client");
        state.seen = true;
        assert!(state.may_stop(true), "a client that joined ends the wait");
        assert!(state.may_stop(false));
    }

    #[test]
    fn a_client_takes_the_next_id_from_one() {
        let shared = Shared::new(Model::new(Arc::from("m"), None, None, None, None));
        assert_eq!(shared.joined(), 1);
        assert_eq!(shared.joined(), 2);
        shared.left();
        assert_eq!(shared.state().clients, 1);
        assert_eq!(shared.joined(), 3);
        shared.left();
        assert_eq!(shared.state().clients, 1);
        assert!(shared.state().linger.is_none(), "a detach arms no linger");
        shared.dropped();
        assert_eq!(shared.state().clients, 0);
        assert!(
            shared.state().linger.is_some(),
            "a client that went without detach arms the linger"
        );
    }

    /// The limits a `set` sends are the ones the cache entry of the new model
    /// names, under the provider the session switched to.
    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn switch_limits_take_the_limits_of_the_new_model() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cache = dir.path().join("models");
        tokio::fs::create_dir_all(&cache)
            .await
            .expect("the cache folder");
        tokio::fs::write(
            cache.join("two.json"),
            concat!(
                r#"{"fetched_at":1,"models":[{"id":"model-b","context_window":32000,"#,
                r#""images":false,"reasoning":{"levels":["low","high"]}}]}"#
            ),
        )
        .await
        .expect("the cache file");
        let shared = Shared::new(Model::new(
            Arc::from("one/model-a"),
            Some(200_000),
            Some(Arc::from("medium")),
            Some(Arc::from("one")),
            Some(cache),
        ));
        shared.switch_provider("two", None);
        let limits = limits_of(&shared, "model-b", None, Some(16_000)).await;
        assert_eq!(
            limits.compact_budget,
            Some(24_000),
            "three quarters of the window of the new model"
        );
        assert_eq!(limits.images, Some(false), "a model that takes no images");
        assert_eq!(
            limits.level.as_deref(),
            Some("high"),
            "the level the new model takes for the one the session sends"
        );
        assert_eq!(
            limits.max_tokens,
            Some(16_000),
            "the cap the config of the new provider names"
        );
    }

    /// A `set` of the model the session runs changes no limit.
    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn switch_limits_of_the_same_model_change_nothing() {
        let shared = Shared::new(Model::new(
            Arc::from("one/model-a"),
            Some(200_000),
            Some(Arc::from("medium")),
            Some(Arc::from("one")),
            Some(std::path::PathBuf::from("/nonexistent/reagent-test-models")),
        ));
        let limits = limits_of(&shared, "model-a", Some("high"), None).await;
        assert_eq!(limits.compact_budget, None);
        assert_eq!(limits.images, None);
        assert_eq!(limits.level, None, "the level of the set goes as written");
        assert_eq!(limits.max_tokens, None);
    }
}

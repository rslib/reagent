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
/// A worker takes its session here: the lock, the socket and the entry of the discovery folder are all
/// there before the ready line goes out.
pub(crate) async fn start(
    args: &Args,
    plan: Option<&Plan>,
    cwd: &Path,
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
    Ok(Some(Worker {
        session,
        saved,
        _files: files,
        _lock: lock,
        token: Arc::from(token),
        listener: Some(listener),
        shared: Arc::new(Shared::new()),
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
        self.shared.queued();
        let command = live::Command::Send {
            client: WORKER,
            text: Text::from(text),
            mode: SendMode::Queue,
        };
        let _ = sender.send(command).await;
    }

    /// Starts serving clients: the tracker, the acceptor, and the record a
    /// client resumes its view from.
    pub(crate) fn begin(&mut self, committed: u64) {
        self.shared.set_committed(committed);
        if let Some(events) = self.events.take() {
            let saved = self.saved.take();
            self.tracker = Some(tokio::spawn(track(events, Arc::clone(&self.shared), saved)));
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
        self.acceptor = Some(tokio::spawn(async move {
            accept(listener, shared, token, sender, session).await;
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
    /// and no run is open, or the open run waits for an approval no client can
    /// give.
    fn idle(&self) -> bool {
        self.clients == 0 && self.armed && (!self.running || self.approval.is_some())
    }

    /// Whether the worker may stop now, given that it may still be waiting for
    /// its first client, which the first one that joins ends.
    fn may_stop(&self, waiting: bool) -> bool {
        self.idle() && (!waiting || self.seen)
    }
}

/// The state every task of one worker shares.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// The frames of the session, and the queues of its clients.
    hub: Mutex<hub::Hub>,
    /// Bumped on every change of `state`, so a wait wakes on it.
    changed: watch::Sender<u64>,
}

impl Shared {
    fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Mutex::new(State::default()),
            hub: Mutex::new(hub::Hub::new()),
            changed,
        }
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
    /// missed.
    fn attach(&self, id: live::ClientId, request: Option<u64>) {
        self.hub().attach(id, request);
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
            client
        };
        self.tell();
        client
    }

    /// Counts a client as gone.
    fn left(&self) {
        let mut state = self.state();
        state.clients = state.clients.saturating_sub(1);
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
) {
    let mut changed = shared.changed();
    let mut joins = tokio::task::JoinSet::new();
    // A worker waits a little for its first client, so the one that started
    // it can connect.
    let deadline = tokio::time::Instant::now() + FIRST_CLIENT;
    let mut waiting = true;
    loop {
        {
            let state = shared.state();
            if state.may_stop(waiting) {
                return;
            }
        }
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
        }
    }
}

/// Serves one client: the handshake, then its requests until it detaches.
async fn join(
    stream: UnixStream,
    shared: Arc<Shared>,
    token: Arc<str>,
    sender: Option<Sender>,
    session: Arc<str>,
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
    let body = InitializeResult {
        major: MAJOR,
        version: env!("CARGO_PKG_VERSION"),
        session: &session,
        client: u64::from(client),
        capabilities: &[],
        committed: shared.committed(),
    };
    message::result(&mut out, id, &body);
    shared.reply(client, &out);
    // This connection holds the sender of the session, which keeps it open for
    // its client until the connection ends.
    let mut cut = false;
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
                let send = live::Command::Send {
                    client,
                    text: Text::from(text.as_ref()),
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
                let settings = live::Settings {
                    model: model.map(|text| Text::from(text.as_ref())),
                    level: level.map(|text| Text::from(text.as_ref())),
                    mode,
                };
                let set = live::Command::Set { client, settings };
                deliver(&shared, sender.as_ref(), client, id, &mut out, set).await;
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
    shared.left();
}

/// Sends one command to the session, and answers `id` once it is in.
async fn deliver(
    shared: &Shared,
    sender: Option<&Sender>,
    client: live::ClientId,
    id: Option<u64>,
    out: &mut Vec<u8>,
    command: live::Command,
) {
    let sent = match sender {
        Some(sender) => sender.send(command).await.is_ok(),
        None => false,
    };
    let Some(id) = id else {
        return;
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
/// hold it. The writes left when the events end are awaited here, so a session
/// that saved a rule has it on disk before the worker reports its end.
async fn track(
    mut events: mpsc::Receiver<Event>,
    shared: Arc<Shared>,
    saved: Option<PathBuf>,
) -> TurnEndReason {
    let mut encode = Encoder::default();
    let mut writes = Vec::new();
    while let Some(event) = events.recv().await {
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
        let mut state = State::default();
        assert!(!state.idle(), "the worker has not queued its work");
        state.armed = true;
        assert!(state.idle());
        state.clients = 1;
        assert!(!state.idle(), "a client is still here");
        state.clients = 0;
        event_turn_start(&mut state);
        assert!(!state.idle(), "a run is open");
        state.approval = Some(3);
        assert!(state.idle(), "no client can answer the approval");
    }

    #[test]
    fn a_worker_waits_for_a_first_client_until_one_joins() {
        let mut state = State {
            armed: true,
            ..State::default()
        };
        assert!(state.idle(), "nothing is open");
        assert!(!state.may_stop(true), "a worker still waits for a client");
        state.seen = true;
        assert!(state.may_stop(true), "a client that joined ends the wait");
        assert!(state.may_stop(false));
    }

    #[test]
    fn a_client_takes_the_next_id_from_one() {
        let shared = Shared::new();
        assert_eq!(shared.joined(), 1);
        assert_eq!(shared.joined(), 2);
        shared.left();
        assert_eq!(shared.state().clients, 1);
        assert_eq!(shared.joined(), 3);
    }
}

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
use std::sync::OnceLock;
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
use reagent_script::plugin::PanelView;
use reagent_script::plugin::Plugins;
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
mod stdin;

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

/// How long a panel frame keeps the next one back: a burst of `set` calls costs
/// a client at most one frame per gap, and the lines of the last one go out.
const PANEL_GAP: Duration = Duration::from_millis(100);

/// The empty result of `detach`.
#[derive(Serialize)]
struct Empty {}

/// The result of a `send`: whether the message starts a run now or waits for
/// the open one, which it joins as a steer or after which it runs, and the id
/// the session reports its fate with.
#[derive(Serialize)]
struct Sent {
    disposition: &'static str,
    message_id: u64,
}

/// Starts the session of `plan`, or returns `None` when this run serves no
/// session of its own.
///
/// `model` is the provider and model text the run uses, `context_window` its
/// context window and `level` the reasoning level it sends: the answer to
/// `initialize` names the first two, and the `settings` notice every one.
///
/// A worker, a run that takes clients on `--listen`, a run that reads its
/// requests on `--commands` and a `-p` run that serves its socket all serve
/// their session as a live one. A run that takes clients on its socket takes
/// the session here: the lock, the socket and the entry of the discovery folder
/// are all there before the ready line goes out, which only a worker writes. A
/// run that reads its requests on stdin alone takes no lock and writes no entry:
/// it has no socket for another process to find, and a `--no-serve` run keeps
/// no file of its own either.
///
/// A served `-p` run whose socket cannot open is a plain run: it warns on stderr
/// and returns `None`.
pub(crate) async fn start(
    args: &Args,
    plan: Option<&Plan>,
    cwd: &Path,
    model: &str,
    context_window: Option<u64>,
    level: Option<&str>,
) -> Result<Option<Worker>, String> {
    let steered = args.commands || args.listen;
    if !args.worker && !steered && !args.serve {
        return Ok(None);
    }
    let Some(plan) = plan else {
        return Err("a worker needs a session".to_string());
    };
    // Finding the state folder and resolving the project folder touch the
    // file system, so they run on the blocking pool. A served run refuses every
    // call the rules send to a person, so it never saves a rule.
    let at = cwd.to_path_buf();
    let saved = if args.serve {
        None
    } else {
        tokio::task::spawn_blocking(move || {
            reagent_config::auth::dir()
                .ok()
                .map(|dir| reagent_config::saved::path(&dir, &at))
        })
        .await
        .ok()
        .flatten()
    };
    let (sender, inbox) = live::channel();
    let shared = Shared::solo(args, model, context_window, level);
    let mut worker = Worker {
        session: plan.id.clone(),
        cwd: cwd.to_path_buf(),
        saved,
        mark: None,
        _files: None,
        _lock: None,
        token: Arc::from(""),
        listener: None,
        shared: Arc::new(shared),
        events: None,
        sender: Some(sender),
        inbox: Some(inbox),
        print: None,
        tracker: None,
        stdin: None,
        leaving: None,
        acceptor: None,
        stop: None,
        stopping: false,
        // A served run ends on its own rule: its clients hold nothing.
        steered: steered || args.serve,
    };
    if args.worker || args.listen {
        worker.listening(args, cwd).await?;
    } else if args.serve
        && let Err(reason) = worker.listening(args, cwd).await
    {
        // A `-p` run serves its socket when it can, and is a plain run when it
        // cannot: one warning says why, and no file of the run is left behind.
        eprintln!("reagent: warning: {reason}; the run takes no clients");
        return Ok(None);
    }
    Ok(Some(worker))
}

impl Worker {
    /// Takes the session a run that takes clients on its socket serves: the
    /// lock, the socket, the entry a client reads and the ready line a worker
    /// writes.
    async fn listening(&mut self, args: &Args, cwd: &Path) -> Result<(), String> {
        let state = crate::session::state_home()
            .ok_or_else(|| "no state folder; set HOME or XDG_STATE_HOME".to_string())?;
        let folder = discovery::folder(&state);
        let session = self.session.clone();
        let (at, id, place) = (folder.clone(), session.clone(), cwd.to_path_buf());
        // Every file call of the socket runs in one job, so opening it costs
        // one trip to the blocking pool and not one per call.
        let opened = tokio::task::spawn_blocking(move || open(&at, &id, place))
            .await
            .map_err(|_| "the worker lock failed".to_string())?;
        let Opened {
            lock,
            files,
            listener,
            token,
            entry,
        } = match opened {
            Ok(Some(opened)) => opened,
            Ok(None) => return Err(busy(&folder, &session).await),
            Err(error) => return Err(error),
        };
        let listener = UnixListener::from_std(listener)
            .map_err(|error| format!("{}: {error}", entry.socket.display()))?;
        let socket = entry.socket.clone();
        if args.worker {
            ready(&session, &socket, entry.pid)
                .await
                .map_err(|error| format!("could not write the ready line: {error}"))?;
        }
        self.mark = Some(Mark { folder, entry });
        self.token = Arc::from(token);
        self.listener = Some(listener);
        self._files = Some(files);
        self._lock = Some(lock);
        Ok(())
    }
}

/// The socket of a session, open and named in the discovery folder.
struct Opened {
    lock: Lock,
    files: Files,
    listener: std::os::unix::net::UnixListener,
    token: String,
    entry: Entry,
}

/// Takes the lock of `session`, binds its socket and writes its entry, or
/// `None` when another process holds the lock. An error leaves no socket and
/// no entry behind.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn open(folder: &Path, session: &str, cwd: PathBuf) -> Result<Option<Opened>, String> {
    let lock = match take(folder, session) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", folder.display())),
    };
    let socket = discovery::socket_path(folder, session).map_err(|error| error.to_string())?;
    // From here every way out removes the socket and the entry, so an early
    // error leaves no file that names this run.
    let files = Files {
        socket: socket.clone(),
        entry: folder.join(format!("{session}.json")),
    };
    // The lock says no live run uses this path, so a file here is the
    // leftover of one that stopped.
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", socket.display())),
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
        .map_err(|error| format!("{}: {error}", socket.display()))?;
    let token = token::new_token_blocking().map_err(|error| format!("no join token: {error}"))?;
    let instance =
        token::new_token_blocking().map_err(|error| format!("no instance id: {error}"))?;
    let entry = Entry {
        pid: std::process::id(),
        instance,
        major: MAJOR,
        token: token.clone(),
        socket,
        session: session.to_string(),
        cwd,
        started_ms: reagent_store::paths::now_ms(),
        waiting: false,
    };
    discovery::write_blocking(folder, &entry)
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    Ok(Some(Opened {
        lock,
        files,
        listener,
        token,
        entry,
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
    /// The discovery entry of this run and the folder it goes in, rewritten
    /// when the run starts or stops waiting for a person; `None` for a run
    /// that takes no client on a socket.
    mark: Option<Mark>,
    /// The socket and the entry, removed before the lock goes.
    _files: Option<Files>,
    /// The lock that keeps it the only run of its session.
    _lock: Option<Lock>,
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
    /// The events of a steered run, forwarded to the task that prints them.
    print: Option<mpsc::Sender<Event>>,
    /// The task that reads the events of the run.
    tracker: Option<JoinHandle<TurnEndReason>>,
    /// The task that reads the session's requests on stdin, for a run that
    /// takes them there.
    stdin: Option<JoinHandle<()>>,
    /// Fires when the last client leaves.
    leaving: Option<oneshot::Receiver<()>>,
    /// The task that accepts clients; the connections are its own tasks and
    /// end with it.
    acceptor: Option<JoinHandle<()>>,
    /// Cancelled by a signal: the run stops whether clients are attached or
    /// not.
    stop: Option<CancellationToken>,
    /// Whether the run stopped serving before its session loop began.
    stopping: bool,
    /// Whether this run is a `-p` run that takes its requests on stdin or on
    /// its socket, instead of a worker.
    steered: bool,
}

impl Worker {
    /// Takes the events of the agent, which the tracker reads.
    pub(crate) fn watch(&mut self, events: mpsc::Receiver<Event>) {
        self.events = Some(events);
    }

    /// Forwards the events of the run to `print`, so a steered run prints them
    /// while its tracker sends them to the clients of its socket.
    pub(crate) fn print(&mut self, print: mpsc::Sender<Event>) {
        self.print = Some(print);
    }

    /// Takes the client that reads the session's requests on `input`, whose
    /// answers go on `answers`, one line each.
    ///
    /// The client is one more of the session, and the run it sees is the one
    /// the session takes: it starts the first run when the run has no task.
    pub(crate) fn stdin_client<R: AsyncRead + Unpin + Send + 'static>(
        &mut self,
        input: R,
        answers: mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let (_, task) = commands(&self.shared, self.sender.clone(), &self.cwd, input, answers);
        self.stdin = Some(task);
    }

    /// Takes the plugins of the run: their commands, their panels, their notes
    /// and the messages they send go to every client of the session.
    pub(crate) fn plugins(&mut self, plugins: Arc<Plugins>) {
        self.shared.set_plugins(plugins);
    }

    /// The socket this run takes clients on, when it takes any.
    pub(crate) fn socket(&self) -> Option<String> {
        self.mark
            .as_ref()
            .map(|mark| mark.entry.socket.to_string_lossy().into_owned())
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
        let message = self.shared.message();
        let command = live::Command::Send {
            message,
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
            let mark = self.mark.clone();
            let print = self.print.take();
            self.tracker = Some(tokio::spawn(track(
                events,
                Arc::clone(&self.shared),
                saved,
                mark,
                committed,
                listed,
                print,
            )));
        }
        let Some(listener) = self.listener.take() else {
            return;
        };
        // Only a worker leaves when its last client goes: a steered run ends on
        // its own rule, and its clients hold nothing.
        let steered = self.steered;
        let (leaving, wait) = oneshot::channel();
        self.leaving = (!steered).then_some(wait);
        let shared = Arc::clone(&self.shared);
        let token = Arc::clone(&self.token);
        let sender = self.sender.clone();
        let first = !steered;
        let session: Arc<str> = Arc::from(self.session.as_str());
        let cwd = self.cwd.clone();
        self.acceptor = Some(tokio::spawn(async move {
            accept(listener, shared, token, sender, session, cwd, first).await;
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
    /// is gone while the calls wait for a person, or on a signal. The
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

    /// Runs the turn a resumed session left unanswered once, with the clients of
    /// the session answering it: opened before its first request, settled after.
    pub(crate) async fn resume(
        &mut self,
        agent: &mut Agent<'_>,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) -> TurnEndReason {
        self.shared.opened();
        let stop = self.stop.clone().unwrap_or_default();
        let Some(inbox) = self.inbox.as_mut() else {
            return TurnEndReason::Error;
        };
        let run = agent.run_with_inbox(history, journal.as_deref_mut(), events, inbox);
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => {}
            () = stop.cancelled() => run.await,
        }
        // The tracker is what sees the end of the run: it clears the open run
        // from the state, and the reason it keeps there is this run's.
        let mut changed = self.shared.changed();
        while self.shared.running() {
            if changed.changed().await.is_err() {
                break;
            }
        }
        self.shared.settled();
        self.shared.end()
    }

    /// Runs the session from the inbox until the last client leaves, or, in a
    /// steered run, until nothing is left to run and stdin has ended.
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
        if self.steered {
            self.serve_steered(agent, history, journal, events, acceptor)
                .await;
            return;
        }
        let Some(mut inbox) = self.inbox.take() else {
            return;
        };
        let sender = self.sender.take();
        let shared = Arc::clone(&self.shared);
        let stop = self.stop.clone().unwrap_or_default();
        let mut leaving = self.leaving.take();
        // What the plugins publish reaches the clients while the session lives;
        // a run with no plugins starts no task.
        let mut pump = Pump::start(&shared, sender.as_ref());
        let live = live::run_live(agent, history, journal, events, &mut inbox);
        tokio::pin!(live);
        let Some(leaving) = leaving.as_mut() else {
            // No client can leave: the pump goes, and its sender with it, so the
            // session returns at the end of its work.
            end_pump(&mut pump).await;
            drop(sender);
            live.await;
            return;
        };
        tokio::select! {
            () = &mut live => {}
            _ = leaving => {
                // A run that waits for an answer no client can give is
                // cancelled, then the senders go so the session returns.
                end_pump(&mut pump).await;
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
                end_pump(&mut pump).await;
                drop(sender);
                stop_accepting(acceptor).await;
                live.await;
                return;
            }
        }
        end_pump(&mut pump).await;
        stop_accepting(acceptor).await;
    }

    /// Runs a session that takes its requests while it runs, until it has no
    /// run open, nothing queued and no request left to read on stdin.
    ///
    /// The clients of its socket do not hold it: when the end comes, the socket
    /// stops taking connections and every client gets the frames it was owed
    /// before its connection goes. A signal or the deadline ends the session
    /// whether a run is open or not, and whether it waits for a person or
    /// for a request on stdin.
    async fn serve_steered(
        &mut self,
        agent: &mut Agent<'_>,
        history: &mut Vec<Message>,
        journal: Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        acceptor: Option<JoinHandle<()>>,
    ) {
        let Some(mut inbox) = self.inbox.take() else {
            stop_accepting(acceptor).await;
            return;
        };
        let sender = self.sender.take();
        let shared = Arc::clone(&self.shared);
        let stop = self.stop.clone().unwrap_or_default();
        let input = self.stdin.take();
        // The clients of a steered run see what the plugins publish too.
        let mut pump = Pump::start(&shared, sender.as_ref());
        let mut changed = shared.changed();
        let live = live::run_live(agent, history, journal, events, &mut inbox);
        tokio::pin!(live);
        // Whether the session loop ended before the end of the rule below, so
        // its own return is awaited once.
        let mut returned = false;
        loop {
            // A request still on its way in holds the session up as a run does:
            // the loop of `run_live` takes the commands the channel holds before
            // it returns, so closing the senders loses none of them.
            if shared.stdin_ended() && !shared.running() {
                break;
            }
            tokio::select! {
                () = &mut live => {
                    returned = true;
                    break;
                }
                _ = changed.changed() => {}
                () = stop.cancelled() => break,
            }
        }
        if stop.is_cancelled() {
            shared.stopped();
        }
        shared.finish();
        shared.release();
        if let Some(input) = input {
            input.abort();
        }
        end_pump(&mut pump).await;
        drop(sender);
        stop_clients(acceptor).await;
        if !returned {
            live.await;
        }
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
    /// The approval or the question the open run waits for a person to
    /// answer, if it waits for one.
    wait: Option<u64>,
    /// Whether the worker has queued its own work.
    armed: bool,
    /// The newest record the journal committed.
    committed: u64,
    /// The clients that joined and have not left.
    clients: u32,
    /// The clients that attached, in the order they attached: every one of
    /// them gets what the plugins publish.
    attached: Vec<live::ClientId>,
    /// Whether a client has ever joined.
    seen: bool,
    /// The deadline a client that left without `detach` keeps the worker up to,
    /// or `None` when no such client left.
    linger: Option<tokio::time::Instant>,
    /// The id the next client gets.
    next: live::ClientId,
    /// The id the next message gets.
    next_message: u64,
    /// Why the last run ended.
    end: TurnEndReason,
    /// Whether no request comes on stdin from now on: true for a run that takes
    /// none there.
    stdin_done: bool,
    /// Whether a signal or the deadline stopped the session.
    stopped: bool,
    /// Whether the session is over and takes no client of its socket.
    done: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            running: false,
            wait: None,
            armed: false,
            committed: 0,
            clients: 0,
            attached: Vec::new(),
            seen: false,
            linger: None,
            next: 1,
            next_message: 1,
            end: TurnEndReason::EndTurn,
            stdin_done: true,
            stopped: false,
            done: false,
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
                self.wait = None;
                self.end = *reason;
            }
            Event::ApprovalRequest(request) => self.wait = Some(request.id),
            Event::QuestionRequest(request) => self.wait = Some(request.id),
            Event::ApprovalResolved { .. } | Event::QuestionResolved { .. } => self.wait = None,
            Event::Committed { record } => self.committed = self.committed.max(*record),
            _ => {}
        }
    }

    /// Whether the worker may stop: no client is left, its own work is queued,
    /// no run is open (or the open run waits for a person no client can
    /// answer), and the linger of a client that left without `detach` is over.
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
    /// open, or the open run waits for a person no client can answer.
    fn alone(&self) -> bool {
        self.clients == 0 && self.armed && (!self.running || self.wait.is_some())
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

/// The discovery entry of one worker and the folder it goes in.
///
/// Only the run's wait for a person changes it, and only after the frame of
/// that change has gone to the clients of the session.
#[derive(Clone)]
struct Mark {
    folder: PathBuf,
    entry: Entry,
}

impl Mark {
    /// Writes the entry again, saying whether the run waits for a person.
    ///
    /// The write of a value the entry already carries does nothing, and a
    /// write that fails costs the mark, not the run.
    async fn waiting(&mut self, waiting: bool) {
        if self.entry.waiting == waiting {
            return;
        }
        self.entry.waiting = waiting;
        if let Err(error) = discovery::write(&self.folder, &self.entry).await {
            eprintln!("reagent: warning: could not mark the waiting request: {error}");
        }
    }
}

/// The state every task of one worker shares.
#[derive(Debug)]
pub(crate) struct Shared {
    state: Mutex<State>,
    /// The frames of the session, and the queues of its clients.
    hub: Mutex<hub::Hub>,
    /// The model of the session and the settings a client set last.
    model: Mutex<Model>,
    /// The client that reads its requests on stdin, if the run has one: its id,
    /// and the channel the answers to its requests go on.
    stdin: Mutex<Option<(live::ClientId, mpsc::UnboundedSender<Vec<u8>>)>>,
    /// The login of the provider a client switched to, kept so its refresh
    /// keeps working.
    held: Mutex<Option<Arc<Session>>>,
    /// The plugins of the run, set once before the servers start: `None` for a
    /// run with no plugins, which sends no frame of its own.
    plugins: OnceLock<Arc<Plugins>>,
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
            stdin: Mutex::new(None),
            held: Mutex::new(None),
            plugins: OnceLock::new(),
            changed,
        }
    }

    /// Takes the plugins of the run: their commands, their panels, their notes
    /// and the messages they send go to every client of the session.
    fn set_plugins(&self, plugins: Arc<Plugins>) {
        let _ = self.plugins.set(plugins);
    }

    /// The plugins of the run, or `None` when the run loaded none.
    fn plugins(&self) -> Option<&Arc<Plugins>> {
        self.plugins.get()
    }

    /// The state of a session served by `args`, with the provider and model
    /// text `model` the run uses, its context window and the reasoning level it
    /// sends.
    ///
    /// A run that serves its session through no socket of its own builds this
    /// itself, so its `set` requests answer with the model of the run.
    pub(crate) fn solo(args: &Args, model: &str, window: Option<u64>, level: Option<&str>) -> Self {
        Self::new(Model::new(
            Arc::from(model),
            window,
            level.map(Arc::from),
            args.provider_name.as_deref().map(Arc::from),
            crate::models::cache_dir(),
        ))
    }

    /// Takes the client that reads the session's requests on stdin, with the
    /// channel its answer lines go on.
    ///
    /// The client gets no frame of the session, only the answers to its own
    /// requests, and it counts as a client until the run counts it gone.
    pub(crate) fn steering(&self, answers: mpsc::UnboundedSender<Vec<u8>>) -> live::ClientId {
        let client = self.joined();
        {
            let mut state = self.state();
            state.stdin_done = false;
        }
        *self
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((client, answers));
        client
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
    ///
    /// The commands and the panels of `plugins` follow them, under the same
    /// lock, so a panel that changes now cannot come before the one it
    /// replaces.
    fn attach(&self, id: live::ClientId, request: Option<u64>) {
        let running = {
            let mut state = self.state();
            if !state.attached.contains(&id) {
                state.attached.push(id);
            }
            state.running
        };
        let mut frames = self.hub();
        frames.set_running(running);
        frames.attach(id, request);
        if let Some(plugins) = self.plugins() {
            plugin_state(&mut frames, id, plugins);
        }
    }

    /// Queues one frame to every client that attached, and keeps it for none of
    /// them: the catch-up of the hub is the frames of the run itself, so what a
    /// plugin publishes never piles up in it. A client that attaches later gets
    /// the state of the plugins instead.
    ///
    /// Says whether a client got it: a frame no client got is one the caller
    /// writes somewhere else, so a note is never lost.
    fn broadcast(&self, frame: &[u8]) -> bool {
        let clients = self.state().attached.clone();
        if clients.is_empty() {
            return false;
        }
        let mut frames = self.hub();
        for id in clients {
            frames.reply(id, frame);
        }
        true
    }

    /// Queues the answer in `out` to the client of `id`: on the channel of the
    /// run's stdin client, or in the queue of the client of the session.
    fn reply(&self, id: live::ClientId, out: &[u8]) {
        let stdin = self
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((client, answers)) = stdin.as_ref()
            && *client == id
        {
            // A channel the run stopped reading drops the answer of a request
            // nobody waits for any more, as a client that is gone does.
            let _ = answers.send(out.to_vec());
            return;
        }
        drop(stdin);
        self.hub().reply(id, out);
    }

    /// Takes the client of `id` out of the hub and out of the clients the
    /// plugins publish to.
    fn left_client(&self, id: live::ClientId) {
        self.state().attached.retain(|client| *client != id);
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
    ///
    /// It says whether the run now waits for a person, and that value is
    /// `Some` only when the wait began or ended.
    fn observe(&self, event: &Event, frame: hub::Frame) -> Option<bool> {
        let waiting = {
            let mut state = self.state();
            let before = state.wait.is_some();
            state.observe(event);
            let after = state.wait.is_some();
            (before != after).then_some(after)
        };
        let mut frames = self.hub();
        if let Event::Committed { record } = event {
            frames.set_committed(*record);
        }
        frames.send(frame, fanout::kind(event));
        drop(frames);
        self.tell();
        waiting
    }

    /// The id of the next message a client sends, one for every `send` of every
    /// client and one for the work the worker queues itself.
    fn message(&self) -> u64 {
        let mut state = self.state();
        let message = state.next_message;
        state.next_message += 1;
        message
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

    /// Whether the open run waits for a person.
    fn waits(&self) -> bool {
        self.state().wait.is_some()
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
        state.wait = None;
        drop(state);
        self.tell();
    }

    /// Notes that the calls a resumed session left open run now: the worker's
    /// own work, which a wait with no client to answer it may stop.
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

    /// Notes that the client of stdin is gone: no request comes from it again.
    fn stdin_gone(&self) {
        self.state().stdin_done = true;
        self.tell();
    }

    /// Whether the run reads no more requests on stdin.
    fn stdin_ended(&self) -> bool {
        self.state().stdin_done
    }

    /// Whether a run is open.
    fn running(&self) -> bool {
        self.state().running
    }

    /// Notes that a signal or the deadline stopped the session.
    fn stopped(&self) {
        self.state().stopped = true;
        self.tell();
    }

    /// Notes that the session is over: its socket takes no client, and the
    /// connections it serves are on their way out.
    fn finish(&self) {
        self.state().done = true;
        self.tell();
    }

    /// Whether the session is over.
    fn done(&self) -> bool {
        self.state().done
    }

    /// Lets every client of the socket go: the queue of each closes, so the
    /// frames it was owed go out and its connection ends.
    fn release(&self) {
        let last = self.state().next;
        let mut frames = self.hub();
        for id in 1..last {
            frames.leave(id);
        }
    }

    /// Why the last run ended.
    fn end(&self) -> TurnEndReason {
        let state = self.state();
        // A session a signal stopped ended cancelled even when no run was open
        // to end with that reason.
        if state.stopped {
            TurnEndReason::Cancelled
        } else {
            state.end
        }
    }
}

/// Ends the acceptor and, with it, every connection it serves.
async fn stop_accepting(acceptor: Option<JoinHandle<()>>) {
    if let Some(acceptor) = acceptor {
        acceptor.abort();
        let _ = acceptor.await;
    }
}

/// Ends the acceptor of a session that is over: it stops taking connections and
/// lets the ones it serves write what they were owed and close. A connection
/// that takes nothing is cut after [`LINGER`].
async fn stop_clients(acceptor: Option<JoinHandle<()>>) {
    let Some(mut acceptor) = acceptor else {
        return;
    };
    // A handle is polled once, so the one that came back within the wait is
    // taken here and the one that did not is cut and taken below.
    if acceptor.is_finished() {
        let _ = acceptor.await;
        return;
    }
    if tokio::time::timeout(LINGER, &mut acceptor).await.is_err() {
        acceptor.abort();
        let _ = acceptor.await;
    }
}

/// Accepts clients until the last one leaves and no run is open, or until the
/// session of a steered run is over.
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
    first: bool,
) {
    let mut changed = shared.changed();
    let mut joins = tokio::task::JoinSet::new();
    // A worker waits a little for its first client, so the one that started it
    // can connect; a steered run takes the clients it gets.
    let deadline = tokio::time::Instant::now() + FIRST_CLIENT;
    let mut waiting = first;
    loop {
        if shared.done() {
            // The session ended: every connection goes once its own frames are
            // written.
            let _ =
                tokio::time::timeout(LINGER, async { while joins.join_next().await.is_some() {} })
                    .await;
            return;
        }
        // The one deadline the wait needs: the linger of a client that left
        // without `detach`, armed only while that is all that keeps the worker
        // up. A steered run ends when its session says so, not when its clients
        // go, so it takes connections until then.
        let linger = if first {
            let state = shared.state();
            if state.may_stop(waiting) {
                return;
            }
            if state.alone() && state.linger.is_some() {
                state.lingering(tokio::time::Instant::now())
            } else {
                None
            }
        } else {
            None
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
            request => {
                handle(
                    &shared,
                    sender.as_ref(),
                    client,
                    &cwd,
                    id,
                    request,
                    &mut out,
                )
                .await
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

/// Queues the state of `plugins` to the client of `id`: the commands they
/// offer, when they offer one, then the panels that show something.
fn plugin_state(frames: &mut hub::Hub, id: live::ClientId, plugins: &Plugins) {
    let commands = plugins.commands();
    if !commands.is_empty() {
        let list = commands
            .iter()
            .map(|command| message::PluginCommand {
                name: Cow::Borrowed(command.name.as_str()),
                description: Cow::Borrowed(command.description.as_str()),
            })
            .collect();
        let mut out = Vec::with_capacity(LINE);
        message::plugin_commands(&mut out, &message::PluginCommands { commands: list });
        frames.reply(id, &out);
    }
    let mut out = Vec::with_capacity(LINE);
    for panel in plugins.panels() {
        if !shown(&panel) {
            continue;
        }
        out.clear();
        panel_frame(&mut out, &panel);
        frames.reply(id, &out);
    }
}

/// Whether `panel` shows anything: a panel whose lines are the empty array is
/// hidden, and one with no lines at all is hidden too.
fn shown(panel: &PanelView) -> bool {
    let lines = panel.lines.trim();
    !lines.is_empty() && lines != "[]"
}

/// Writes one `plugin_panel` frame into `out`.
///
/// The lines the plugin wrote go into the frame as they are: the worker never
/// parses them and never encodes them again.
fn panel_frame(out: &mut Vec<u8>, panel: &PanelView) {
    let mut params =
        Vec::with_capacity(panel.id.len() + panel.title.len() + panel.lines.len() + 32);
    params.extend_from_slice(br#"{"id":"#);
    quote(&mut params, &panel.id);
    params.extend_from_slice(br#","title":"#);
    quote(&mut params, &panel.title);
    params.extend_from_slice(br#","lines":"#);
    params.extend_from_slice(panel.lines.as_bytes());
    params.push(b'}');
    message::notify(out, message::methods::PLUGIN_PANEL, &params);
}

/// Writes `text` as one JSON string, quotes and all.
fn quote(out: &mut Vec<u8>, text: &str) {
    // A string always serializes, and a vector takes every byte it is given.
    let _ = serde_json::to_writer(&mut *out, text);
}

/// What the plugins publish, sent to the clients of the session while it lives.
struct Pump {
    /// Cancelled when the session ends, which frees the sender it holds.
    stop: CancellationToken,
    /// The task that pumps.
    task: JoinHandle<()>,
}

impl Pump {
    /// Starts sending what the plugins of `shared` publish to its clients,
    /// whose own messages the session takes on `sender`.
    ///
    /// A run with no plugins starts no task, holds no sender and reads no
    /// clock: a run that changes nothing pays nothing.
    fn start(shared: &Arc<Shared>, sender: Option<&Sender>) -> Option<Self> {
        let plugins = Arc::clone(shared.plugins()?);
        let sender = sender?.clone();
        let stop = CancellationToken::new();
        let shared = Arc::clone(shared);
        let token = stop.clone();
        let task = tokio::spawn(pump(shared, plugins, sender, token));
        Some(Self { stop, task })
    }
}

/// Ends the pump of a session, if it started one: the sender it holds goes,
/// so the session sees the last of them.
async fn end_pump(pump: &mut Option<Pump>) {
    let Some(pump) = pump.take() else {
        return;
    };
    pump.stop.cancel();
    let _ = pump.task.await;
}

/// Sends what the plugins publish to the clients of the session, and queues the
/// messages they send as the worker's own, until `stop` ends the session.
async fn pump(shared: Arc<Shared>, plugins: Arc<Plugins>, sender: Sender, stop: CancellationToken) {
    let mut out = Vec::with_capacity(LINE);
    // When the last panel frame went out, so a burst of sets waits the rest of
    // the gap instead of one frame per set.
    let mut sent: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            () = plugins.changed() => {}
            () = stop.cancelled() => return,
        }
        for note in plugins.take_notes() {
            out.clear();
            message::note(&mut out, &note);
            // A note no client saw goes to the stderr of the worker at once:
            // the end of the run does not see it, since the pump took it.
            if !shared.broadcast(&out) {
                eprintln!("reagent: {note}");
            }
        }
        // The messages go before the panels wait: a plugin that sets a panel and
        // then sends starts its turn at once.
        for text in plugins.take_sent() {
            shared.queued();
            let message = shared.message();
            let command = live::Command::Send {
                message,
                client: WORKER,
                text: Text::from(text),
                blocks: Vec::new(),
                mode: SendMode::Queue,
            };
            tokio::select! {
                taken = sender.send(command) => {
                    if taken.is_err() {
                        return;
                    }
                }
                () = stop.cancelled() => return,
            }
        }
        let mut panels = plugins.take_changed_panels();
        if panels.is_empty() {
            continue;
        }
        if let Some(at) = sent {
            tokio::select! {
                () = tokio::time::sleep_until(at + PANEL_GAP) => {}
                () = stop.cancelled() => return,
            }
            // The sets that came while this one waited hold the newest lines, so
            // the last lines of a panel are the ones that go out.
            latest(&mut panels, plugins.take_changed_panels());
        }
        for panel in &panels {
            out.clear();
            panel_frame(&mut out, panel);
            shared.broadcast(&out);
        }
        sent = Some(tokio::time::Instant::now());
    }
}

/// Puts the lines of the `newer` panels over the ones the pump already holds,
/// so one panel goes out once, with its newest lines.
fn latest(panels: &mut Vec<PanelView>, newer: Vec<PanelView>) {
    for panel in newer {
        match panels.iter_mut().find(|held| held.id == panel.id) {
            Some(held) => *held = panel,
            None => panels.push(panel),
        }
    }
}

/// Does one request of one client: a message, a cancel, a `set`, a `compact`
/// or an approval goes to the session, and its answer goes to the client that
/// sent it.
///
/// The requests only a client of the socket sends, `attach`, `detach` and
/// `initialize`, are refused here: a run that reads its requests on stdin has
/// no session to attach to, no connection to detach and no handshake.
async fn handle(
    shared: &Arc<Shared>,
    sender: Option<&Sender>,
    client: live::ClientId,
    cwd: &Path,
    id: Option<u64>,
    request: Request<'_>,
    out: &mut Vec<u8>,
) {
    match request {
        Request::Send { text, mode } => {
            // The image files the message names by `@path` go with it. A
            // file that cannot be prepared is told to the client that named
            // it, and the text goes as it was written.
            let blocks = match crate::attach::blocks(cwd, text.as_ref()).await {
                Ok(blocks) => blocks,
                Err(message) => {
                    let message = format!("{message}: the image was not attached");
                    let frame = Encoder::default().frame(&Event::Error(Text::from(message)));
                    shared.reply(client, &frame);
                    Vec::new()
                }
            };
            let message = shared.message();
            let send = live::Command::Send {
                message,
                client,
                text: Text::from(text.as_ref()),
                blocks,
                mode: send_mode(mode),
            };
            // Read before the send, so the answer names the session the
            // message found.
            let disposition = if shared.running() {
                "queued"
            } else {
                "started"
            };
            deliver(
                shared,
                sender,
                client,
                id,
                out,
                send,
                Some((disposition, message)),
            )
            .await;
        }
        Request::Cancel => {
            deliver(
                shared,
                sender,
                client,
                id,
                out,
                live::Command::Cancel { client },
                None,
            )
            .await;
        }
        Request::Set { model, level, mode } => {
            let mode = match mode.map(|text| text.parse::<PermissionMode>()) {
                Some(Ok(mode)) => Some(mode),
                Some(Err(error)) => {
                    out.clear();
                    message::error(out, id, &RpcError::invalid_params(error));
                    shared.reply(client, out);
                    return;
                }
                None => None,
            };
            // The model text is a bare provider model id, which the
            // session's provider takes, or the `provider/model` of another
            // provider, which the worker builds and switches to.
            let (model, provider, config, max_tokens) = match model {
                Some(text) => match switched(shared, text.as_ref()).await {
                    Ok(asked) => (
                        Some(asked.model),
                        asked.swap,
                        asked.config,
                        asked.max_tokens,
                    ),
                    Err(error) => {
                        out.clear();
                        message::error(out, id, &error);
                        shared.reply(client, out);
                        return;
                    }
                },
                None => (None, None, None, None),
            };
            // A model the session does not run brings the limits of its
            // cache entry and the level it takes, so the first request it
            // serves is the one that model takes.
            let limits = match &model {
                Some(id) => limits_of(shared, id, level.as_deref(), max_tokens).await,
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
            deliver(shared, sender, client, id, out, set, None).await;
        }
        Request::Compact { note } => {
            let compact = live::Command::Compact {
                client,
                note: note.map(|text| Text::from(text.as_ref())),
            };
            deliver(shared, sender, client, id, out, compact, None).await;
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
            deliver(shared, sender, client, id, out, approve, None).await;
        }
        Request::Answer { id: ask, answers } => {
            let answer = live::Command::Answer {
                client,
                id: ask,
                answers: answers.map(|per| {
                    per.into_iter()
                        .map(|picks| picks.into_iter().map(|pick| pick.into_owned()).collect())
                        .collect()
                }),
            };
            deliver(shared, sender, client, id, out, answer, None).await;
        }
        // A plugin command answers as soon as it is taken: the client that
        // sent it reads its requests while the plugin runs, and the text the
        // command shows goes to every client of the session as a note.
        Request::Command { name, args } => {
            let Some(plugins) = shared.plugins() else {
                let message = format!("no plugin command is named '/{name}'");
                out.clear();
                message::error(
                    out,
                    id,
                    &RpcError {
                        code: RpcError::METHOD_NOT_FOUND,
                        message: message.into(),
                    },
                );
                shared.reply(client, out);
                return;
            };
            if let Some(id) = id {
                out.clear();
                message::result(out, id, &Empty {});
                shared.reply(client, out);
            }
            let plugins = Arc::clone(plugins);
            let shared = Arc::clone(shared);
            // The request line is gone once this call returns, so the command
            // runs on its own copy of its name and arguments.
            let (name, args) = (name.into_owned(), args.into_owned());
            tokio::spawn(async move {
                if let Some(text) = plugins.command(&name, &args).await {
                    let mut out = Vec::with_capacity(LINE);
                    message::note(&mut out, &text);
                    // The text a command returns is a note the client that asked
                    // gets, and a note with no client to take it is written
                    // where the worker's own stderr goes.
                    if !shared.broadcast(&out) {
                        eprintln!("reagent: {text}");
                    }
                }
            });
        }
        Request::Attach { .. } | Request::Detach | Request::Initialize { .. } => {
            out.clear();
            message::error(out, id, &RpcError::invalid_request());
            shared.reply(client, out);
        }
    }
}

/// Takes the client that reads the session's requests on stdin, and reads them
/// from `input` until it ends.
///
/// The client is one more client of `shared`, with no frame of the session
/// ever sent to it: the answer to each of its requests goes on `answers`, one
/// line each, and the run writes them out among its event lines. The task ends
/// at the end of `input`, which the caller reads as it likes; it never stops
/// the run, and the end it notes is that no request comes from it again.
pub(crate) fn commands<R: AsyncRead + Unpin + Send + 'static>(
    shared: &Arc<Shared>,
    sender: Option<Sender>,
    cwd: &Path,
    input: R,
    answers: mpsc::UnboundedSender<Vec<u8>>,
) -> (live::ClientId, JoinHandle<()>) {
    let client = shared.steering(answers.clone());
    let shared = Arc::clone(shared);
    let cwd = cwd.to_path_buf();
    let task = tokio::spawn(async move {
        stdin::serve(input, answers, &shared, sender.as_ref(), client, &cwd).await;
        shared.stdin_gone();
    });
    (client, task)
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
    sent: Option<(&'static str, u64)>,
) -> bool {
    let delivered = match sender {
        Some(sender) => sender.send(command).await.is_ok(),
        None => false,
    };
    let Some(id) = id else {
        return delivered;
    };
    out.clear();
    if let (true, Some((disposition, message))) = (delivered, sent) {
        message::result(
            out,
            id,
            &Sent {
                disposition,
                message_id: message,
            },
        );
    } else if delivered {
        message::result(out, id, &Empty {});
    } else {
        message::error(
            out,
            Some(id),
            &RpcError::new(RpcError::INVALID_REQUEST, "the session is closed"),
        );
    }
    shared.reply(client, out);
    delivered
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
    mut mark: Option<Mark>,
    committed: u64,
    mut listed: Option<crate::session::Listing>,
    print: Option<mpsc::Sender<Event>>,
) -> TurnEndReason {
    let mut encode = Encoder::default();
    let mut writes = Vec::new();
    while let Some(event) = events.recv().await {
        if let Some(print) = &print {
            // A steered run prints the line of every event too; a writer that
            // is gone costs the line, not the run.
            let _ = print.send(event.clone()).await;
        }
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
        let waiting = shared.observe(&event, frame);
        // The entry follows the frame, so a client of another session marks
        // this one a moment after the clients here saw the change.
        if let (Some(waiting), Some(mark)) = (waiting, mark.as_mut()) {
            mark.waiting(waiting).await;
        }
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
            message: 1,
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
        assert_eq!(state.wait, Some(7));
        state.observe(&Event::ApprovalResolved {
            id: 7,
            client: 1,
            answer: Answer::Once,
        });
        assert_eq!(state.wait, None);
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
        state.wait = Some(3);
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

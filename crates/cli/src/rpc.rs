//! The `rpc` command: the requests of a live session that wait for a person,
//! and the answers to them, one JSON line each.
//!
//! A supervisor that can run programs but cannot open the socket of a session
//! starts this helper. It joins the worker of the session named on the command
//! line, prints the `approval_request` and `question_request` frames that wait,
//! then reads one request per line of stdin, sends it and prints its reply. An
//! `approve` or an `answer` the worker took is followed by the frame that
//! resolves it. At the end of stdin the helper detaches, and the run goes on.

use std::borrow::Cow;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use reagent_protocol::discovery;
use reagent_protocol::discovery::WaitKind;
use reagent_protocol::lines::Lines;
use reagent_protocol::message;
use reagent_protocol::message::MAJOR;
use reagent_protocol::message::methods;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::Instant;

/// The client name the helper gives the worker it joins.
const NAME: &str = "reagent-rpc";

/// How long the helper waits for the frame that resolves an `approve` or an
/// `answer`, once the worker took it.
const RESOLVED: Duration = Duration::from_secs(10);

/// The id of the handshake, and the one of the attach that follows it.
const HELLO: u64 = 1;
const ATTACH: u64 = 2;

/// Runs `reagent rpc <session>`: the entry of the session, its socket, and
/// then the lines of stdin.
pub async fn run(argv: Vec<std::ffi::OsString>) -> ExitCode {
    let words: Vec<String> = argv
        .into_iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    let [session] = words.as_slice() else {
        eprintln!("reagent: rpc needs the id or the prefix of a live session");
        return ExitCode::from(2);
    };
    let Some(state) = crate::session::state_home() else {
        eprintln!("reagent: no state folder; set HOME or XDG_STATE_HOME");
        return ExitCode::from(2);
    };
    let Some(entry) = live_entry(&discovery::folder(&state), session).await else {
        eprintln!("reagent: no live session {session}");
        return ExitCode::from(2);
    };
    let stream = match UnixStream::connect(&entry.socket).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("reagent: {}: {error}", entry.socket.display());
            return ExitCode::from(1);
        }
    };
    match Rpc::new(stream).serve(&entry.token).await {
        Ok(failed) => ExitCode::from(u8::from(failed)),
        Err(reason) => {
            eprintln!("reagent: {reason}");
            ExitCode::from(1)
        }
    }
}

/// The live entry of `session`, or of the one session its prefix names.
async fn live_entry(folder: &Path, session: &str) -> Option<discovery::Entry> {
    let entries = discovery::list(folder).await;
    if let Some(entry) = entries.iter().find(|entry| entry.session == session) {
        return Some(entry.clone());
    }
    let mut named = entries
        .into_iter()
        .filter(|entry| entry.session.starts_with(session));
    let first = named.next()?;
    named.next().is_none().then_some(first)
}

/// One connection to a worker, and the lines of its socket and of stdin that
/// are read but not handled yet.
struct Rpc {
    stream: UnixStream,
    socket: Lines,
    stdin: Lines,
    /// The id of the next request this helper sends.
    next: u64,
    /// The request the worker has not answered yet, if one is out.
    out: Option<Outstanding>,
    /// Whether a reply was an error, or the frame that resolves a request never
    /// came.
    failed: bool,
    /// The bytes of the request being written, and of the line being printed.
    line: Vec<u8>,
    frame: Vec<u8>,
    out_stream: tokio::io::Stdout,
    /// One stdin for the whole run: a read that a socket frame interrupts
    /// keeps its bytes here, where a new handle would lose them.
    input: tokio::io::Stdin,
}

/// A request of the helper the worker has not answered yet.
struct Outstanding {
    /// The id the request carries.
    id: u64,
    /// For an `approve` or an `answer`, the request it answers: the kind of the
    /// frame that resolves it, and its id.
    resolves: Option<(WaitKind, u64)>,
    /// When the wait for that frame runs out, once the reply came.
    deadline: Option<Instant>,
}

/// One line of stdin: the method and the params of the request it names.
#[derive(Deserialize)]
struct Ask<'a> {
    #[serde(borrow)]
    method: Cow<'a, str>,
    #[serde(default)]
    params: Value,
}

/// The params of the handshake.
#[derive(Serialize)]
struct Hello<'a> {
    major: u32,
    client: Named<'a>,
    capabilities: &'a [&'a str],
    token: &'a str,
}

#[derive(Serialize)]
struct Named<'a> {
    name: &'a str,
    version: &'a str,
}

/// The params of an `attach`.
#[derive(Serialize)]
struct From {
    from: u64,
}

/// What a read of stdin and of the socket together gave.
enum Read {
    /// One of the two read bytes.
    More,
    /// stdin ended.
    InputEnd,
    /// The worker closed its half of the connection.
    SessionEnd,
}

impl Rpc {
    fn new(stream: UnixStream) -> Rpc {
        Rpc {
            stream,
            socket: Lines::new(),
            stdin: Lines::new(),
            next: ATTACH,
            out: None,
            failed: false,
            line: Vec::new(),
            frame: Vec::new(),
            out_stream: tokio::io::stdout(),
            input: tokio::io::stdin(),
        }
    }

    /// The handshake, the catch-up, then the requests of stdin. It says whether
    /// a reply was an error or a wait ran out.
    async fn serve(&mut self, token: &str) -> Result<bool, String> {
        let committed = self.hello(token).await?;
        self.caught_up(committed).await?;
        self.requests().await?;
        Ok(self.failed)
    }

    /// Joins the session, and returns the newest record the worker committed.
    async fn hello(&mut self, token: &str) -> Result<u64, String> {
        let params = serde_json::to_vec(&Hello {
            major: MAJOR,
            client: Named {
                name: NAME,
                version: env!("CARGO_PKG_VERSION"),
            },
            capabilities: &[],
            token,
        })
        .map_err(|error| error.to_string())?;
        self.send(HELLO, "initialize", &params).await?;
        loop {
            let Some(line) = self.socket().await else {
                return Err("the session closed the connection".to_string());
            };
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if frame["id"].as_u64() != Some(HELLO) {
                continue;
            }
            if let Some(message) = refused(&frame) {
                return Err(message);
            }
            return match frame["result"]["committed"].as_u64() {
                Some(committed) => Ok(committed),
                None => Err(format!("the worker named no record: {}", shown(&line))),
            };
        }
    }

    /// Attaches from `committed`, and prints every request its catch-up holds.
    async fn caught_up(&mut self, committed: u64) -> Result<(), String> {
        let params =
            serde_json::to_vec(&From { from: committed }).map_err(|error| error.to_string())?;
        self.send(ATTACH, "attach", &params).await?;
        loop {
            let Some(line) = self.socket().await else {
                return Err("the session closed the connection".to_string());
            };
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if frame["id"].as_u64() != Some(ATTACH) {
                if waits(&frame) {
                    self.print(&line).await;
                }
                continue;
            }
            if let Some(message) = refused(&frame) {
                return Err(message);
            }
            return Ok(());
        }
    }

    /// Reads one request per line of stdin and prints each reply, until stdin
    /// ends; then it detaches.
    async fn requests(&mut self) -> Result<(), String> {
        loop {
            // A request whose reply is out keeps stdin unread: the next line of
            // a supervisor comes after the answer to the one before it.
            if self.out.is_some() {
                if !self.answering().await {
                    return Err("the session closed the connection".to_string());
                }
                continue;
            }
            if let Some(line) = buffered(&mut self.stdin) {
                self.ask(&line).await?;
                continue;
            }
            if let Some(line) = buffered(&mut self.socket) {
                self.frame(&line).await;
                continue;
            }
            match self.read().await {
                Read::More => {}
                Read::InputEnd => break,
                Read::SessionEnd => {
                    return Err("the session closed the connection".to_string());
                }
            }
        }
        let params = b"null";
        self.next += 1;
        let id = self.next;
        self.send(id, "detach", params).await
    }

    /// Waits for the reply of the request that is out, and for the frame that
    /// resolves it. It says whether the connection is still open.
    async fn answering(&mut self) -> bool {
        loop {
            let deadline = self.out.as_ref().and_then(|out| out.deadline);
            let line = match deadline {
                Some(at) => match tokio::time::timeout_at(at, self.socket()).await {
                    Ok(line) => line,
                    Err(_) => {
                        let (kind, ask) = match self.out.as_ref() {
                            Some(Outstanding {
                                resolves: Some((kind, ask)),
                                ..
                            }) => (*kind, *ask),
                            _ => {
                                self.out = None;
                                continue;
                            }
                        };
                        eprintln!(
                            "reagent: no frame resolved {} {ask} in {}s",
                            kind_name(kind),
                            RESOLVED.as_secs()
                        );
                        self.failed = true;
                        self.out = None;
                        return true;
                    }
                },
                None => self.socket().await,
            };
            let Some(line) = line else {
                return false;
            };
            self.frame(&line).await;
            if self.out.is_none() {
                return true;
            }
        }
    }

    /// Sends one line of stdin, and remembers what its reply leaves to wait
    /// for. A line that is not a request is told on stderr, and the helper
    /// goes on.
    async fn ask(&mut self, line: &[u8]) -> Result<(), String> {
        let text = std::str::from_utf8(line).unwrap_or_default().trim();
        if text.is_empty() {
            return Ok(());
        }
        let ask: Ask<'_> = match serde_json::from_str(text) {
            Ok(ask) => ask,
            Err(error) => {
                eprintln!("reagent: {error}: {text}");
                return Ok(());
            }
        };
        let params = serde_json::to_vec(&ask.params).map_err(|error| error.to_string())?;
        let ask_id = ask.params["id"].as_u64();
        let resolves = match (ask.method.as_ref(), ask_id) {
            ("approve", Some(id)) => Some((WaitKind::Approval, id)),
            ("answer", Some(id)) => Some((WaitKind::Question, id)),
            _ => None,
        };
        self.next += 1;
        let id = self.next;
        self.out = Some(Outstanding {
            id,
            resolves,
            deadline: None,
        });
        self.send(id, &ask.method, &params).await
    }

    /// Handles one frame of the socket: the reply of the request that is out,
    /// the frame that resolves it, or a request that now waits for a person.
    async fn frame(&mut self, line: &[u8]) {
        let Ok(frame) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let Some(out) = self.out.take() else {
            if waits(&frame) {
                self.print(line).await;
            }
            return;
        };
        if frame["id"].as_u64() == Some(out.id) {
            self.out = None;
            self.print(line).await;
            if frame.get("error").is_some() {
                self.failed = true;
                return;
            }
            if let Some((kind, ask)) = out.resolves {
                self.out = Some(Outstanding {
                    id: out.id,
                    resolves: Some((kind, ask)),
                    deadline: Some(Instant::now() + RESOLVED),
                });
            }
            return;
        }
        let Some((kind, ask)) = out.resolves else {
            self.out = Some(out);
            if waits(&frame) {
                self.print(line).await;
            }
            return;
        };
        if resolved(&frame, kind, ask) {
            self.print(line).await;
        } else if failed_event(&frame) {
            self.failed = true;
            self.print(line).await;
        } else {
            // Any other frame, such as a streamed delta, leaves the request
            // waiting; a new request that waits for a person is shown.
            self.out = Some(out);
            if waits(&frame) {
                self.print(line).await;
            }
        }
    }

    /// Writes the request `id` of `method` with `params`.
    async fn send(&mut self, id: u64, method: &str, params: &[u8]) -> Result<(), String> {
        self.line.clear();
        message::request(&mut self.line, id, method, params);
        self.stream
            .write_all(&self.line)
            .await
            .map_err(|error| error.to_string())
    }

    /// The next line of the socket, or `None` when the worker closed it.
    async fn socket(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.socket.next() {
                Some(Ok(line)) => return Some(line.to_vec()),
                Some(Err(_)) => continue,
                None => {}
            }
            match self.socket.fill(&mut self.stream).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// One line of stdin or of the socket, whatever comes first.
    async fn read(&mut self) -> Read {
        let Self {
            stream,
            socket,
            stdin,
            input,
            ..
        } = self;
        tokio::select! {
            filled = stdin.fill(input) => match filled {
                Ok(0) | Err(_) => Read::InputEnd,
                Ok(_) => Read::More,
            },
            filled = socket.fill(stream) => match filled {
                Ok(0) | Err(_) => Read::SessionEnd,
                Ok(_) => Read::More,
            },
        }
    }

    /// Prints one JSON line, as it came. A reader that stopped reading costs
    /// the lines, not the session.
    async fn print(&mut self, line: &[u8]) {
        self.frame.clear();
        self.frame.extend_from_slice(line);
        self.frame.push(b'\n');
        let written = self.out_stream.write_all(&self.frame);
        if written.await.is_ok() {
            let _ = self.out_stream.flush().await;
        }
    }
}

/// The next whole line of `lines`, if one is read already.
fn buffered(lines: &mut Lines) -> Option<Vec<u8>> {
    loop {
        match lines.next() {
            Some(Ok(line)) => return Some(line.to_vec()),
            Some(Err(_)) => continue,
            None => return None,
        }
    }
}

/// The reason the worker refused `frame`, or `None` when it took it.
fn refused(frame: &Value) -> Option<String> {
    let error = frame.get("error")?;
    Some(format!("the worker refused: {error}"))
}

/// Whether `frame` is a request that waits for a person.
fn waits(frame: &Value) -> bool {
    frame["method"] == methods::APPROVAL_REQUEST || frame["method"] == methods::QUESTION_REQUEST
}

/// Whether `frame` is the notice that resolved the request of `kind` with
/// `ask`.
fn resolved(frame: &Value, kind: WaitKind, ask: u64) -> bool {
    let method = match kind {
        WaitKind::Approval => methods::APPROVAL_RESOLVED,
        WaitKind::Question => methods::QUESTION_RESOLVED,
    };
    frame["method"] == method && frame["params"]["id"].as_u64() == Some(ask)
}

/// Whether `frame` is the error event of the run.
fn failed_event(frame: &Value) -> bool {
    frame["method"] == methods::EVENT && frame["params"]["type"] == "error"
}

/// The word the protocol uses for `kind`.
fn kind_name(kind: WaitKind) -> &'static str {
    match kind {
        WaitKind::Approval => "approval",
        WaitKind::Question => "question",
    }
}

/// One line of a frame, for an error message.
fn shown(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame that is neither the reply nor the one that resolves the request,
    /// such as a streamed delta, leaves the request waiting.
    #[tokio::test]
    async fn another_frame_leaves_the_request_waiting() {
        let (ours, _theirs) = UnixStream::pair().expect("a socket pair");
        let mut rpc = Rpc::new(ours);
        rpc.out = Some(Outstanding {
            id: 3,
            resolves: Some((WaitKind::Approval, 0)),
            deadline: Some(Instant::now() + RESOLVED),
        });
        rpc.frame(
            br#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"hi"}}"#,
        )
        .await;
        assert!(rpc.out.is_some(), "the delta took the request");
        rpc.frame(br#"{"jsonrpc":"2.0","method":"approval_resolved","params":{"id":0}}"#)
            .await;
        assert!(rpc.out.is_none(), "the resolved frame ends the wait");
        assert!(!rpc.failed);
    }
}

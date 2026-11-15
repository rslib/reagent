//! The editor on the other end: the lines this agent writes, and the answers it
//! asks for.
//!
//! One task owns stdout, so no two sessions interleave half a line. A request
//! to the editor takes an id of this agent's own, counted apart from the ids of
//! the editor's requests; the answer arrives on stdin and is routed back to the
//! task that waits for it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use super::message;

/// The answer to a request: its result, or the error object the editor sent.
pub(super) type Reply = Result<Value, Value>;

/// How many lines wait for the writer before their writer waits in turn.
const QUEUE: usize = 64;

/// The editor this agent talks to.
#[derive(Clone)]
pub(super) struct Editor {
    /// The lines to write, in order.
    out: Sender<Vec<u8>>,
    /// The requests this agent sent and still awaits an answer to, by id.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
    /// The id of the next request of this agent.
    next: Arc<AtomicU64>,
}

impl Editor {
    /// Makes an editor, and the lines its writer has to write.
    pub(super) fn new() -> (Editor, Receiver<Vec<u8>>) {
        let (out, lines) = mpsc::channel(QUEUE);
        let editor = Editor {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(0)),
        };
        (editor, lines)
    }

    /// Writes the lines of `lines` to stdout until the editor goes.
    pub(super) async fn write(mut lines: Receiver<Vec<u8>>) {
        let mut out = tokio::io::stdout();
        while let Some(line) = lines.recv().await {
            // A closed stdout ends the writer: a line nobody reads is one the
            // process is leaving without.
            if out.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = out.flush().await;
    }

    /// The id of the next request of this agent.
    pub(super) fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Sends one notification.
    pub(super) async fn note<P: Serialize>(&self, method: &str, params: P) {
        self.line(message::Note {
            jsonrpc: "2.0",
            method,
            params,
        })
        .await;
    }

    /// Answers the request `id`.
    pub(super) async fn respond(&self, id: Value, result: Value) {
        self.line(message::Response {
            jsonrpc: "2.0",
            id,
            result,
        })
        .await;
    }

    /// Answers the request `id` with an error.
    pub(super) async fn fail(&self, id: Value, code: i32, message: &str, data: Option<Value>) {
        self.line(message::Failed {
            jsonrpc: "2.0",
            id,
            error: message::ErrorObject {
                code,
                message: message.to_string(),
                data,
            },
        })
        .await;
    }

    /// Asks the editor the request of `method`, and answers what it says.
    ///
    /// An editor that closed the connection answers no request at all, which the
    /// caller reads as the request it was refused.
    pub(super) async fn request<P: Serialize>(&self, method: &str, params: P) -> Reply {
        let id = self.next_id();
        let (answer, reply) = oneshot::channel();
        if let Ok(mut held) = self.pending.lock() {
            held.insert(id, answer);
        }
        self.line(message::Call {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })
        .await;
        match reply.await {
            Ok(reply) => reply,
            Err(_) => Err(Value::Null),
        }
    }

    /// Hands the answer of the request `id` to the task that waits for it, and
    /// drops it for a request that no task waits for.
    pub(super) fn route(&self, id: u64, reply: Reply) {
        let waiting = self
            .pending
            .lock()
            .ok()
            .and_then(|mut held| held.remove(&id));
        if let Some(waiting) = waiting {
            let _ = waiting.send(reply);
        }
    }

    /// Writes one message as one line.
    async fn line<M: Serialize>(&self, message: M) {
        let Ok(mut line) = serde_json::to_vec(&message) else {
            return;
        };
        line.push(b'\n');
        // A closed channel is an editor that went: its lines have nowhere to go.
        let _ = self.out.send(line).await;
    }
}

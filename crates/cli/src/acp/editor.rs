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

/// One request of the editor, as the map of the open ones holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Open {
    /// A request this agent works on.
    Waiting,
    /// A prompt of the session the id names.
    Prompt(String),
    /// A request a `$/cancel_request` answered, whose task's own answer the
    /// editor must not hear a second time.
    Answered,
}

/// What `$/cancel_request` does with the request it names.
pub(super) enum Cancel {
    /// The request is a prompt of this session, which stops as a
    /// `session/cancel` stops it.
    Prompt(String),
    /// The request has its answer already, and the editor is told so now.
    Answered,
    /// Nothing waits for the id.
    None,
}

/// The JSON text of a request id, which is how the open requests are named.
///
/// A JSON-RPC id is a number or a string, and the editor keeps the two apart,
/// so the number 7 and the string "7" are two requests: the quotes of the
/// string stay in its key.
pub(super) fn key_of(id: &Value) -> Option<String> {
    match id {
        Value::Number(_) | Value::String(_) => Some(id.to_string()),
        _ => None,
    }
}

/// The editor this agent talks to.
#[derive(Clone)]
pub(super) struct Editor {
    /// The lines to write, in order.
    out: Sender<Vec<u8>>,
    /// The requests this agent sent and still awaits an answer to, by id.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>,
    /// The requests of the editor this agent still works on, by the text of
    /// their id.
    open: Arc<Mutex<HashMap<String, Open>>>,
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
            open: Arc::new(Mutex::new(HashMap::new())),
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
    fn next_id(&self) -> u64 {
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

    /// Takes the request the editor sent as one this agent works on.
    pub(super) fn mark(&self, key: &str) {
        if let Ok(mut held) = self.open.lock() {
            held.insert(key.to_string(), Open::Waiting);
        }
    }

    /// Names the request `key` as a prompt of `session`.
    pub(super) fn mark_prompt(&self, key: &str, session: &str) {
        if let Ok(mut held) = self.open.lock() {
            held.insert(key.to_string(), Open::Prompt(session.to_string()));
        }
    }

    /// Takes the request `key` for a `$/cancel_request` of it.
    pub(super) fn cancel(&self, key: &str) -> Cancel {
        let Ok(mut held) = self.open.lock() else {
            return Cancel::None;
        };
        match held.get(key).cloned() {
            Some(Open::Prompt(session)) => Cancel::Prompt(session),
            Some(Open::Waiting) => {
                held.insert(key.to_string(), Open::Answered);
                Cancel::Answered
            }
            // A request a cancel already answered gets no second answer.
            Some(Open::Answered) | None => Cancel::None,
        }
    }

    /// Answers the request `id`.
    pub(super) async fn respond(&self, id: Value, result: Value) {
        if !self.closes(&id) {
            return;
        }
        self.line(message::Response {
            jsonrpc: "2.0",
            id,
            result,
        })
        .await;
    }

    /// Answers the request `id` with an error.
    pub(super) async fn fail(&self, id: Value, code: i32, message: &str, data: Option<Value>) {
        if !self.closes(&id) {
            return;
        }
        self.refuse(id, code, message, data).await;
    }

    /// Answers the request `id` as cancelled, which is the one answer a request
    /// a `$/cancel_request` took gets: the task that works on it answers into
    /// nothing.
    pub(super) async fn answer_cancelled(&self, id: Value) {
        self.refuse(id, -32800, "the request was cancelled", None)
            .await;
    }

    /// Takes the request `id` out of the open ones, and answers whether its own
    /// answer still reaches the editor.
    fn closes(&self, id: &Value) -> bool {
        let Some(key) = key_of(id) else {
            return true;
        };
        !matches!(
            self.open.lock().ok().and_then(|mut held| held.remove(&key)),
            Some(Open::Answered)
        )
    }

    /// Writes the error answer of the request `id`, whatever a cancel did.
    async fn refuse(&self, id: Value, code: i32, message: &str, data: Option<Value>) {
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

    /// Takes an id for a request this agent will send, and the place its answer
    /// arrives, before the task that sends it runs: a request taken back in
    /// between still finds its entry.
    pub(super) fn reserve(&self) -> (u64, oneshot::Receiver<Reply>) {
        let id = self.next_id();
        let (answer, reply) = oneshot::channel();
        if let Ok(mut held) = self.pending.lock() {
            held.insert(id, answer);
        }
        (id, reply)
    }

    /// Asks the editor the request `id` of `method` that [`Editor::reserve`]
    /// named, and answers what it says through `reply`.
    ///
    /// An editor that closed the connection answers no request at all, which the
    /// caller reads as the request it was refused.
    pub(super) async fn request<P: Serialize>(
        &self,
        id: u64,
        reply: oneshot::Receiver<Reply>,
        method: &str,
        params: P,
    ) -> Reply {
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

    /// Takes back the request `id` this agent sent: the editor hears
    /// `$/cancel_request` for it, and the task that waits for its answer stops
    /// waiting.
    pub(super) async fn withdraw(&self, id: u64) {
        let held = self
            .pending
            .lock()
            .ok()
            .and_then(|mut held| held.remove(&id));
        if held.is_some() {
            self.note("$/cancel_request", serde_json::json!({"requestId": id}))
                .await;
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Editor;
    use super::key_of;

    #[tokio::test]
    async fn a_request_taken_back_before_it_is_sent_is_still_cancelled() {
        let (editor, mut lines) = Editor::new();
        let (id, _reply) = editor.reserve();
        editor.withdraw(id).await;
        let line = lines.recv().await.expect("a line");
        let line: serde_json::Value = serde_json::from_slice(&line).expect("JSON");
        assert_eq!(line["method"], "$/cancel_request");
        assert_eq!(line["params"]["requestId"], id);
    }

    #[test]
    fn a_number_id_and_a_string_id_of_the_same_text_are_two_keys() {
        assert_ne!(key_of(&json!(7)), key_of(&json!("7")));
        assert_eq!(key_of(&json!(7)), key_of(&json!(7)));
    }
}

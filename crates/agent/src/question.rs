//! The `ask` tool: the model asks the person a question and waits for the
//! answer.
//!
//! The questions become an [`Event::QuestionRequest`] with an id, and the call
//! waits on one channel for the answer of any client and on the run's cancel
//! token, with no timer. A run with no listener returns at once and the model
//! decides itself: silence is never an answer.
//!
//! A wait is on the journal before its question goes out, under an id of its
//! own, so a run can also be *parked*: [`Questions::park`] ends every wait as
//! parked, the process ends, and a later one answers the question and resumes
//! the session. A parked call leaves no result behind.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use reagent_ai::{Text, ToolSpec};

use crate::agent_loop::emit;
use crate::batch::Note;
use crate::event::{Event, QuestionRequest};
use crate::hook::{EventHook, Seen, WaitEnd, WaitKind};
use crate::live::ClientId;
use crate::tool::{Tool, ToolOutput, parse_args};

/// The `ask` tool name.
pub const NAME: &str = "ask";

/// The text a result carries when no client listens, so nobody can be asked.
pub const NOBODY: &str = "No person is available; decide yourself and state the choice.";

/// The text a result carries when the person gave no answer.
pub const UNANSWERED: &str = "No answer; decide yourself and state the choice.";

/// The text a parked question's result carries.
///
/// The loop leaves the result of a parked call out of the journal and out of
/// the history, so no model reads this text.
pub const PARKED: &str = "The question is parked; a later session answers it.";

/// One question the model asks, as the person reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The question.
    pub q: Text,
    /// The answers offered, in the order shown; empty when the person may
    /// type any answer.
    pub options: Vec<Choice>,
    /// Whether the person may pick several of the options.
    pub multi: bool,
}

/// One answer a question offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The label the person picks, which the answer reads back.
    pub label: Text,
    /// A line under the label, for a cost or a caveat.
    pub note: Option<Text>,
}

/// What a client answered to one question request.
///
/// `None` skips the whole request; otherwise there is one list per question, in
/// the order the model asked them, of the labels picked or the text typed.
pub type Answers = Option<Vec<Vec<String>>>;

/// What ended a question that waits.
#[derive(Debug)]
enum Ended {
    /// A client answered, with its id and its answer.
    Answered(ClientId, Answers),
    /// The run parked the question, so no answer comes in this process.
    Parked,
}

/// The questions of one session that wait for an answer, by id.
///
/// The ids count up for the life of the registry. The loop and the [`Ask`] tool
/// share one registry, so an answer reaches the call that asks whichever clone
/// of the registry the loop holds.
#[derive(Debug, Default)]
pub struct Questions {
    next: AtomicU64,
    /// Whether a park happened, so a result asks the locked set only then.
    parked: AtomicBool,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The waits of the calls that ask, by id.
    pending: HashMap<u64, Wait>,
    /// The events of the live session that listens, when one does.
    events: Option<mpsc::Sender<Event>>,
    /// The notes of the batch that runs, so a wait reaches the journal before
    /// its question goes out.
    notes: Option<mpsc::UnboundedSender<Note>>,
    /// The ask calls of the batch that runs, in the order it starts them.
    calls: Vec<Call>,
    /// The question id a resumed call asks under again, by call id.
    reused: HashMap<String, String>,
    /// Whether the run parked: a wait opened from now on ends parked at once.
    parked: bool,
    /// The calls whose question a park ended, so their results stay out of the
    /// journal and the history.
    stopped: HashSet<String>,
    /// The run's event hook, when it has one, so a wait reaches the plugins.
    hook: Watcher,
}

/// The run's event hook as the registry keeps it: a wait tells it, and a debug
/// line needs nothing of it.
#[derive(Clone, Default)]
struct Watcher(Option<Arc<dyn EventHook>>);

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Some(_) => "Watcher(a hook)",
            None => "Watcher(none)",
        })
    }
}

/// One question that waits for its answer, as the registry holds it.
#[derive(Debug)]
struct Wait {
    /// The call the wait belongs to, when the batch placed the call.
    call: Option<String>,
    /// The id the question carries, when the batch placed the call.
    qid: Option<String>,
    /// Where the answer of a client arrives.
    sender: oneshot::Sender<Ended>,
}

/// One ask call of the batch that runs, before its wait opens.
#[derive(Debug)]
struct Call {
    /// The tool call the question belongs to.
    call: String,
    /// The arguments the model sent, so the call finds its own entry.
    args: String,
}

/// One question that waits for its answer.
struct Pending {
    /// The id the request carries.
    id: u64,
    /// The call the question belongs to, when the batch placed it.
    call: Option<String>,
    /// The id the question carries, when the batch placed it.
    qid: Option<String>,
    /// Whether the record of that id is already on the disk.
    asked: bool,
    /// Where the answer of a client arrives.
    answer: oneshot::Receiver<Ended>,
    /// The events of the session that listens.
    events: mpsc::Sender<Event>,
}

/// What opening a wait for one call found.
enum Opened {
    /// The wait is open and a client may answer it.
    Wait(Pending),
    /// No client listens, so nobody can be asked.
    Nobody,
    /// The run parked, so no client hears this question in this process.
    Parked,
}

impl Questions {
    /// A registry with no listener, so an ask returns at once.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Raises the registry to the clients of a live session: an ask becomes an
    /// [`Event::QuestionRequest`] and the call waits for their answer.
    pub fn listen(&self, events: mpsc::Sender<Event>) {
        self.lock().events = Some(events);
    }

    /// Lowers the registry again, so the next ask returns at once instead of
    /// waiting for an answer no client can send.
    pub fn hush(&self) {
        self.lock().events = None;
    }

    /// Answers question `id` with what `client` picked. Returns whether a call
    /// waited for it: a second answer, or one after a cancel, finds none.
    pub fn answer(&self, client: ClientId, id: u64, answers: Answers) -> bool {
        let Some(wait) = self.lock().pending.remove(&id) else {
            return false;
        };
        let _ = wait.sender.send(Ended::Answered(client, answers));
        true
    }

    /// Parks the questions of the run: every wait ends parked, and every wait
    /// opened after this one ends parked at once.
    ///
    /// A parked call leaves no result in the journal or in the history, so the
    /// run ends once the other calls of the reply are in, and a later process
    /// answers the question and resumes the session.
    pub fn park(&self) {
        let mut inner = self.lock();
        inner.parked = true;
        self.parked.store(true, Ordering::Relaxed);
        for wait in std::mem::take(&mut inner.pending).into_values() {
            if let Some(call) = wait.call {
                inner.stopped.insert(call);
            }
            let _ = wait.sender.send(Ended::Parked);
        }
    }

    /// Says that the call `call` asks again under question `qid`, whose wait a
    /// process that ended already recorded: the ask writes no second record.
    pub fn reuse(&self, call: &str, qid: &str) {
        self.lock().reused.insert(call.to_owned(), qid.to_owned());
    }

    /// Starts a batch: the questions it asks reach the journal through `notes`,
    /// their waits reach the plugins through `hook`, and no call an earlier
    /// batch placed stays.
    pub(crate) fn begin(
        &self,
        notes: mpsc::UnboundedSender<Note>,
        hook: Option<Arc<dyn EventHook>>,
    ) {
        let mut inner = self.lock();
        inner.notes = Some(notes);
        inner.hook = Watcher(hook);
        inner.calls.clear();
    }

    /// Ends the batch, so a later one places its own calls.
    pub(crate) fn end(&self) {
        let mut inner = self.lock();
        inner.notes = None;
        inner.hook = Watcher::default();
        inner.calls.clear();
        inner.reused.clear();
    }

    /// Places the ask call the batch is about to run, so its question knows the
    /// call it belongs to and the id it carries.
    ///
    /// A call the batch places again, because the type coercion repaired its
    /// arguments and it runs a second time, finds its own entry replaced here.
    pub(crate) fn expect(&self, call: &str, args: &str) {
        let mut inner = self.lock();
        inner.calls.retain(|placed| placed.call != call);
        inner.calls.push(Call {
            call: call.to_owned(),
            args: args.to_owned(),
        });
    }

    /// Whether a park now leaves a question a later process answers: a wait
    /// that a batch placed is on the journal under its id.
    ///
    /// A worker reads this before a client leaves, so a wait no journal holds
    /// is cancelled instead.
    pub fn parkable(&self) -> bool {
        self.lock().pending.values().any(|wait| wait.qid.is_some())
    }

    /// Whether a park ended the question of call `call`, so its result stays
    /// out of the journal and out of the history.
    pub fn parked_call(&self, call: &str) -> bool {
        self.parked.load(Ordering::Relaxed) && self.lock().stopped.contains(call)
    }

    /// The id the waiting call `call` asks under, when one waits.
    pub fn qid(&self, call: &str) -> Option<String> {
        let inner = self.lock();
        inner
            .pending
            .values()
            .find(|wait| wait.call.as_deref() == Some(call))
            .and_then(|wait| wait.qid.clone())
    }

    /// Takes the id of the next question and opens the channel its answer
    /// arrives on, for the call whose arguments are `args`.
    fn open(&self, args: &str) -> Opened {
        let mut inner = self.lock();
        let Some(events) = inner.events.clone() else {
            return Opened::Nobody;
        };
        let placed = inner
            .calls
            .iter()
            .position(|call| call.args == args)
            .map(|at| inner.calls.remove(at));
        let (call, qid, asked) = match placed {
            Some(placed) => {
                // The id a resumed call asks under again is used here, where the
                // call asks, so a placed call that never asks burns no id.
                let (qid, asked) = match inner.reused.remove(&placed.call) {
                    Some(qid) => (qid, true),
                    None => (fresh_qid(), false),
                };
                (Some(placed.call), Some(qid), asked)
            }
            None => (None, None, false),
        };
        // The run parked: nobody hears the question in this process, and the
        // call leaves no result behind.
        if inner.parked {
            if let Some(call) = call {
                inner.stopped.insert(call);
            }
            return Opened::Parked;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = oneshot::channel();
        inner.pending.insert(
            id,
            Wait {
                call: call.clone(),
                qid: qid.clone(),
                sender,
            },
        );
        Opened::Wait(Pending {
            id,
            call,
            qid,
            asked,
            answer,
            events,
        })
    }

    /// Tells the plugins that the question of `call` starts to wait for a person.
    fn wait_started(&self, call: Option<&str>) {
        let hook = self.lock().hook.0.clone();
        let Some(hook) = hook else {
            return;
        };
        hook.seen(Seen::Waiting {
            kind: WaitKind::Question,
            call: call.unwrap_or_default(),
            tool: NAME,
        });
    }

    /// Tells the plugins what ended the wait of `call`.
    fn wait_ended(&self, call: Option<&str>, how: WaitEnd) {
        let hook = self.lock().hook.0.clone();
        let Some(hook) = hook else {
            return;
        };
        hook.seen(Seen::Waited {
            kind: WaitKind::Question,
            call: call.unwrap_or_default(),
            how,
        });
    }

    /// Drops a wait that no answer will reach, so a late answer finds no one.
    fn give_up(&self, id: u64) {
        self.lock().pending.remove(&id);
    }

    /// Puts the wait on the journal, when the run keeps one, and returns once it
    /// is there: the question goes out only after the record.
    async fn record(&self, pending: &Pending, questions: &str) {
        let (Some(call), Some(qid)) = (pending.call.as_deref(), pending.qid.as_deref()) else {
            return;
        };
        if pending.asked {
            return;
        }
        let notes = self.lock().notes.clone();
        let Some(notes) = notes else {
            return;
        };
        let (done, written) = oneshot::channel();
        let note = Note::Question {
            call: call.to_owned(),
            qid: qid.to_owned(),
            questions: questions.to_owned(),
            done,
        };
        if notes.send(note).is_ok() {
            let _ = written.await;
        }
    }
}

/// A fresh question id: 32 lowercase hex characters from 16 random bytes.
///
/// The store's random source mixes two hashers a process seeds from the OS with
/// a counter and the clock, which is enough for a name: an id is a name, not a
/// key.
fn fresh_qid() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for word in [
        reagent_store::paths::random_u64(),
        reagent_store::paths::random_u64(),
    ] {
        for byte in word.to_le_bytes() {
            id.push(HEX[usize::from(byte >> 4)] as char);
            id.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    id
}

/// The `ask` tool: the model asks the person a question and waits.
#[derive(Debug)]
pub struct Ask {
    spec: ToolSpec,
    questions: Arc<Questions>,
}

impl Ask {
    /// Builds the tool over the session's question registry.
    pub fn new(questions: Arc<Questions>) -> Self {
        Self {
            spec: spec(),
            questions,
        }
    }

    /// Asks the questions of `args` and answers with one `Q:`/`A:` pair for
    /// each, or with the text that says the person did not answer.
    async fn interview(&self, args: &str, cancel: &CancellationToken) -> ToolOutput {
        let questions = match asked(args) {
            Ok(questions) => questions,
            Err(output) => return output,
        };
        let pending = match self.questions.open(args) {
            Opened::Wait(pending) => pending,
            Opened::Nobody => return ToolOutput::ok(NOBODY),
            Opened::Parked => return ToolOutput::ok(PARKED),
        };
        // The wait is on the disk before the question goes out, so a process
        // that ends here leaves a question a later one answers.
        self.questions.record(&pending, &questions_text(args)).await;
        let request = Event::QuestionRequest(Box::new(QuestionRequest {
            id: pending.id,
            questions: questions.clone(),
        }));
        // Every wait that reaches the clients starts here and ends below, so
        // the plugins hear one `waiting` and one `waited` for each.
        let call = pending.call.as_deref();
        self.questions.wait_started(call);
        if emit(&pending.events, cancel, request).await {
            self.questions.give_up(pending.id);
            self.resolved(&pending.events, pending.id, 0, cancel).await;
            self.questions.wait_ended(call, WaitEnd::Cancelled);
            return ToolOutput::ok(UNANSWERED);
        }
        let answered = tokio::select! {
            answer = pending.answer => answer.ok(),
            () = cancel.cancelled() => None,
        };
        // A cancelled or dropped wait leaves its id to no one.
        let answered = match answered {
            Some(answered) => answered,
            None => {
                self.questions.give_up(pending.id);
                self.resolved(&pending.events, pending.id, 0, cancel).await;
                self.questions.wait_ended(call, WaitEnd::Cancelled);
                return ToolOutput::ok(UNANSWERED);
            }
        };
        let client = match answered {
            Ended::Answered(client, _) => client,
            Ended::Parked => 0,
        };
        let how = match answered {
            Ended::Answered(..) => WaitEnd::Answered,
            Ended::Parked => WaitEnd::Parked,
        };
        let told_parked = pending.call.is_some();
        self.resolved(&pending.events, pending.id, client, cancel)
            .await;
        self.questions.wait_ended(call, how);
        match answered {
            Ended::Answered(_, Some(picks)) => told(&questions, &picks),
            Ended::Answered(_, None) => ToolOutput::ok(UNANSWERED),
            // A wait no batch placed cannot be resumed under its call, so the
            // run leaves no question behind: the model decides itself.
            Ended::Parked if !told_parked => ToolOutput::ok(UNANSWERED),
            Ended::Parked => ToolOutput::ok(PARKED),
        }
    }

    /// Tells every client that question `id` is done and which client answered;
    /// client 0 is no client, since nobody answered.
    async fn resolved(
        &self,
        events: &mpsc::Sender<Event>,
        id: u64,
        client: ClientId,
        cancel: &CancellationToken,
    ) {
        let _ = emit(events, cancel, Event::QuestionResolved { id, client }).await;
    }
}

impl Tool for Ask {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// An ask changes nothing, so a resumed session runs the call again and
    /// asks again rather than reporting it interrupted.
    fn read_only(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(self.interview(args, cancel))
    }
}

/// The spec offered to the provider.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: "Ask the person and wait for the answer. Only when blocked on a choice the person must make; for facts you can check or a usual default, choose, say what you chose, and go on. Ask one choice per question, with 2 to 4 options. The person may also type their own answer.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "q": { "type": "string" },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "note": { "type": "string" }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multi": { "type": "boolean" }
                        },
                        "required": ["q", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    }
}

#[derive(Deserialize)]
struct Arguments<'a> {
    #[serde(default, borrow)]
    questions: Vec<Asked<'a>>,
}

#[derive(Deserialize)]
struct Asked<'a> {
    #[serde(borrow)]
    q: Cow<'a, str>,
    #[serde(default, borrow)]
    options: Vec<Offered<'a>>,
    #[serde(default)]
    multi: bool,
}

#[derive(Deserialize)]
struct Offered<'a> {
    #[serde(borrow)]
    label: Cow<'a, str>,
    #[serde(default, borrow)]
    note: Option<Cow<'a, str>>,
}

/// The questions of `args`, or the tool error that names the problem with their
/// shape.
fn asked(args: &str) -> Result<Vec<Question>, ToolOutput> {
    let Arguments { questions } = parse_args(args)?;
    if questions.is_empty() {
        return Err(ToolOutput::error(
            "invalid arguments: `questions` is empty; ask at least one question",
        ));
    }
    let mut out = Vec::with_capacity(questions.len());
    for (at, asked) in questions.into_iter().enumerate() {
        out.push(built(at, asked)?);
    }
    Ok(out)
}

/// One question as the card reads it, or the tool error that names the problem
/// with its shape. `at` is its place among the questions, from 0.
fn built(at: usize, asked: Asked<'_>) -> Result<Question, ToolOutput> {
    if asked.q.trim().is_empty() {
        return Err(ToolOutput::error(format!(
            "invalid arguments: question {} has no `q` text",
            at + 1
        )));
    }
    // The card offers its own typed answer, so a question needs two options to
    // be a choice. More than the spec's four still draw, so they pass.
    if asked.options.len() < 2 {
        return Err(ToolOutput::error(format!(
            "invalid arguments: question {} has {} options; give 2 to 4, one choice per question",
            at + 1,
            asked.options.len()
        )));
    }
    let mut options = Vec::with_capacity(asked.options.len());
    for (place, offered) in asked.options.into_iter().enumerate() {
        if offered.label.trim().is_empty() {
            return Err(ToolOutput::error(format!(
                "invalid arguments: option {} of question {} has no `label`",
                place + 1,
                at + 1
            )));
        }
        options.push(Choice {
            label: Text::from(offered.label.as_ref()),
            note: offered.note.map(|note| Text::from(note.as_ref())),
        });
    }
    Ok(Question {
        q: Text::from(asked.q.as_ref()),
        options,
        multi: asked.multi,
    })
}

/// The JSON text of the `questions` argument of `args`, as the model sent it,
/// which the journal keeps and a later process reads back.
fn questions_text(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|args| args.get("questions").map(ToString::to_string))
        .unwrap_or_else(|| "[]".to_owned())
}

/// The result an answered question gives in a process that only reads its
/// record: the questions of the `questions` text and the answers of the
/// `answers` text, as the live answer reads them. A `null` answer is no answer.
pub(crate) fn from_record(questions: &str, answers: &str) -> ToolOutput {
    let Some(questions) = listed(questions) else {
        return ToolOutput::ok(UNANSWERED);
    };
    match serde_json::from_str::<Answers>(answers) {
        Ok(Some(picks)) => told(&questions, &picks),
        _ => ToolOutput::ok(UNANSWERED),
    }
}

/// The questions of a `questions` text, when it reads as the argument does.
fn listed(text: &str) -> Option<Vec<Question>> {
    let asked: Vec<Asked<'_>> = serde_json::from_str(text).ok()?;
    let mut out = Vec::with_capacity(asked.len());
    for (at, asked) in asked.into_iter().enumerate() {
        out.push(built(at, asked).ok()?);
    }
    Some(out)
}

/// The result of an answer: one `Q:` line and one `A:` line per question. An
/// answer that does not name one list per question is no answer.
fn told(questions: &[Question], picks: &[Vec<String>]) -> ToolOutput {
    if picks.len() != questions.len() {
        return ToolOutput::ok(UNANSWERED);
    }
    let mut body = String::new();
    for (question, picks) in questions.iter().zip(picks) {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("Q: ");
        body.push_str(&question.q);
        body.push('\n');
        body.push_str("A: ");
        body.push_str(&picks.join(", "));
    }
    ToolOutput::ok(body)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The arguments of one question with two options.
    const ONE: &str = r#"{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres","note":"on the server"}]}]}"#;

    /// A live session: the registry listens on a new events channel.
    fn listening() -> (Arc<Questions>, mpsc::Receiver<Event>) {
        let questions = Questions::new();
        let (events, seen) = mpsc::channel(8);
        questions.listen(events);
        (questions, seen)
    }

    #[tokio::test]
    async fn question_an_answer_reaches_the_waiting_id_once() {
        let (questions, mut seen) = listening();
        let Opened::Wait(first) = questions.open(ONE) else {
            panic!("a client listens");
        };
        let Opened::Wait(second) = questions.open(ONE) else {
            panic!("a client listens");
        };
        assert_eq!((first.id, second.id), (0, 1), "the ids count up");

        assert!(questions.answer(3, first.id, Some(vec![vec!["sqlite".to_owned()]])));
        let Ended::Answered(client, answers) = first.answer.await.expect("the answer arrives")
        else {
            panic!("the answer ends the wait");
        };
        assert_eq!(client, 3);
        assert_eq!(answers, Some(vec![vec!["sqlite".to_owned()]]));
        assert!(
            !questions.answer(4, first.id, None),
            "a second answer finds no one"
        );
        assert!(
            questions.answer(5, second.id, None),
            "the other question still waits"
        );
        assert!(
            seen.try_recv().is_err(),
            "the registry sends no event itself"
        );
    }

    #[tokio::test]
    async fn question_a_cancel_ends_the_wait_and_a_late_answer_finds_no_one() {
        let (questions, mut seen) = listening();
        let cancel = CancellationToken::new();
        let tool = Ask::new(Arc::clone(&questions));
        let asked = tokio::spawn({
            let cancel = cancel.clone();
            async move { tool.run(ONE, &cancel).await }
        });
        let Some(Event::QuestionRequest(request)) = seen.recv().await else {
            panic!("expected a question request");
        };
        assert_eq!(request.questions.len(), 1);
        assert_eq!(request.questions[0].q.as_ref(), "Which database?");
        assert_eq!(request.questions[0].options.len(), 2);
        let id = request.id;

        cancel.cancel();
        let output = asked.await.expect("the tool returns");
        assert_eq!(output.content.as_ref(), UNANSWERED);
        assert!(!output.is_error);
        let Some(Event::QuestionResolved { id: done, client }) = seen.recv().await else {
            panic!("expected a resolved event");
        };
        assert_eq!((done, client), (id, 0), "no client answered");
        assert!(
            !questions.answer(1, id, Some(vec![vec!["sqlite".to_owned()]])),
            "the cancel left no wait behind"
        );
    }

    /// A park ends a wait that is open as parked, and the call that waits gets
    /// a result the loop leaves out of the journal and the history.
    #[tokio::test]
    async fn park_ends_an_open_wait_as_parked() {
        let (questions, mut seen) = listening();
        // The batch places the call, so its question is on the journal and the
        // park leaves it for a later process.
        questions.expect("call_1", ONE);
        let cancel = CancellationToken::new();
        let tool = Ask::new(Arc::clone(&questions));
        let asked = tokio::spawn({
            let cancel = cancel.clone();
            async move { tool.run(ONE, &cancel).await }
        });
        let Some(Event::QuestionRequest(request)) = seen.recv().await else {
            panic!("expected a question request");
        };
        questions.park();
        let output = asked.await.expect("the tool returns");
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), PARKED);
        let Some(Event::QuestionResolved { id, client }) = seen.recv().await else {
            panic!("expected a resolved event");
        };
        assert_eq!((id, client), (request.id, 0), "no client answered");
        assert!(
            !questions.answer(1, request.id, Some(vec![vec!["sqlite".to_owned()]])),
            "the park left no wait behind"
        );
    }

    /// A run that parked asks no client again: the question ends parked at once.
    #[tokio::test]
    async fn park_ends_an_ask_that_comes_after_it_at_once() {
        let (questions, mut seen) = listening();
        questions.park();
        let tool = Ask::new(Arc::clone(&questions));
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            tool.run(ONE, &CancellationToken::new()),
        )
        .await
        .expect("the ask returns at once");
        assert_eq!(output.content.as_ref(), PARKED);
        assert!(
            seen.try_recv().is_err(),
            "no client hears a question of a parked run"
        );
    }

    /// The call the batch placed asks under the question id the batch gave it,
    /// and a call the loop marked as reused keeps its old id.
    #[tokio::test]
    async fn park_a_reused_qid_is_used_and_no_new_one_is_made() {
        let (questions, mut seen) = listening();
        questions.reuse("call_1", "0123456789abcdef0123456789abcdef");
        questions.expect("call_1", ONE);
        let tool = Ask::new(Arc::clone(&questions));
        let asked = tokio::spawn(async move { tool.run(ONE, &CancellationToken::new()).await });
        let Some(Event::QuestionRequest(request)) = seen.recv().await else {
            panic!("expected a question request");
        };
        assert_eq!(
            questions.qid("call_1").as_deref(),
            Some("0123456789abcdef0123456789abcdef"),
            "the reused id is the one the question carries"
        );
        questions.answer(2, request.id, None);
        let output = asked.await.expect("the tool returns");
        assert_eq!(output.content.as_ref(), UNANSWERED);
    }

    /// A call the batch placed without a reuse gets a fresh id, 32 lowercase hex
    /// characters, and a parked call is named so the loop drops its result.
    #[tokio::test]
    async fn park_names_the_call_whose_question_it_ended() {
        let (questions, mut seen) = listening();
        questions.expect("call_1", ONE);
        let tool = Ask::new(Arc::clone(&questions));
        let asked = tokio::spawn(async move { tool.run(ONE, &CancellationToken::new()).await });
        let _request = seen.recv().await.expect("a question request");
        let qid = questions.qid("call_1").expect("the call asks under an id");
        assert_eq!(qid.len(), 32, "{qid}");
        assert!(
            qid.chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()),
            "{qid}"
        );
        assert!(!questions.parked_call("call_1"), "nothing parked yet");
        questions.park();
        let _ = asked.await.expect("the tool returns");
        assert!(
            questions.parked_call("call_1"),
            "the loop leaves this call's result out"
        );
    }

    /// A wait no batch placed cannot be resumed under a call id, so a park must
    /// not give the model the parked text as a normal result: the answer is the
    /// text a process that only reads the journal gives, UNANSWERED.
    #[tokio::test]
    async fn park_a_wait_no_batch_placed_is_unanswered() {
        let (questions, mut seen) = listening();
        let tool = Ask::new(Arc::clone(&questions));
        let asked = tokio::spawn(async move { tool.run(ONE, &CancellationToken::new()).await });
        let _request = seen.recv().await.expect("a question request");
        questions.park();
        let output = asked.await.expect("the tool returns");
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), UNANSWERED);
    }

    /// The loop asks whether a park ended each result's call, so that question
    /// takes no lock until a park happened.
    #[test]
    fn park_parked_call_takes_no_lock_until_a_park() {
        let questions = Arc::new(Questions::new());
        let held = questions.lock();
        let asked = Arc::clone(&questions);
        let (done, waited) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            done.send(asked.parked_call("call_1")).ok();
        });
        let seen = waited.recv_timeout(Duration::from_secs(2));
        drop(held);
        probe.join().expect("the probe returns");
        assert_eq!(
            seen,
            Ok(false),
            "a result takes no lock for a park that never happened"
        );
    }

    /// A record of an answer gives the text a live answer gives.
    #[test]
    fn question_record_of_an_answer_reads_as_the_live_answer() {
        let questions = questions_text(ONE);
        assert_eq!(
            from_record(&questions, r#"[["postgres"]]"#)
                .content
                .as_ref(),
            "Q: Which database?\nA: postgres"
        );
        assert_eq!(from_record(&questions, "null").content.as_ref(), UNANSWERED);
    }

    #[tokio::test]
    async fn question_the_tool_without_a_listener_says_so_at_once() {
        let tool = Ask::new(Questions::new());
        let output = tool.run(ONE, &CancellationToken::new()).await;
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), NOBODY);
    }

    #[tokio::test]
    async fn question_a_question_with_no_questions_is_a_tool_error() {
        let tool = Ask::new(Questions::new());
        let output = tool
            .run(r#"{"questions":[]}"#, &CancellationToken::new())
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("at least one"),
            "{}",
            output.content
        );
    }

    /// A question with fewer than two options is a tool error that names the
    /// rule, so the model asks again with a choice to make.
    #[tokio::test]
    async fn question_needs_two_options() {
        for args in [
            r#"{"questions":[{"q":"Which database?"}]}"#,
            r#"{"questions":[{"q":"Which database?","options":[{"label":"sqlite"}]}]}"#,
        ] {
            let tool = Ask::new(Questions::new());
            let out = tool.run(args, &CancellationToken::new()).await;
            assert!(out.is_error, "{args}");
            assert!(out.content.contains("give 2 to 4"), "{}", out.content);
        }
    }

    /// The spec asks for 2 to 4 options on every question.
    #[test]
    fn question_spec_requires_two_to_four_options() {
        let spec = spec();
        let item = &spec.input_schema["properties"]["questions"]["items"];
        assert_eq!(item["required"], serde_json::json!(["q", "options"]));
        assert_eq!(item["properties"]["options"]["minItems"], 2);
        assert_eq!(item["properties"]["options"]["maxItems"], 4);
    }
}

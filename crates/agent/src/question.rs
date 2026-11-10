//! The `ask` tool: the model asks the person a question and waits for the
//! answer.
//!
//! The questions become an [`Event::QuestionRequest`] with an id, and the call
//! waits on one channel for the answer of any client and on the run's cancel
//! token, with no timer. A run with no listener returns at once and the model
//! decides itself: silence is never an answer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use reagent_ai::{Text, ToolSpec};

use crate::agent_loop::emit;
use crate::event::{Event, QuestionRequest};
use crate::live::ClientId;
use crate::tool::{Tool, ToolOutput, parse_args};

/// The `ask` tool name.
pub const NAME: &str = "ask";

/// The text a result carries when no client listens, so nobody can be asked.
pub const NOBODY: &str = "No person is available; decide yourself and state the choice.";

/// The text a result carries when the person gave no answer.
pub const UNANSWERED: &str = "No answer; decide yourself and state the choice.";

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

/// What arrived for a question that waits: the client that answered and its
/// answer.
type Answered = (ClientId, Answers);

/// The questions of one session that wait for an answer, by id.
///
/// The ids count up for the life of the registry. The loop and the [`Ask`] tool
/// share one registry, so an answer reaches the call that asks whichever clone
/// of the registry the loop holds.
#[derive(Debug, Default)]
pub struct Questions {
    next: AtomicU64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The waits of the calls that ask, by id.
    pending: HashMap<u64, oneshot::Sender<Answered>>,
    /// The events of the live session that listens, when one does.
    events: Option<mpsc::Sender<Event>>,
}

/// One question that waits for its answer.
struct Pending {
    /// The id the request carries.
    id: u64,
    /// Where the answer of a client arrives.
    answer: oneshot::Receiver<Answered>,
    /// The events of the session that listens.
    events: mpsc::Sender<Event>,
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
        let Some(sender) = self.lock().pending.remove(&id) else {
            return false;
        };
        let _ = sender.send((client, answers));
        true
    }

    /// Takes the id of the next question and opens the channel its answer
    /// arrives on; `None` when no client listens.
    fn open(&self) -> Option<Pending> {
        let mut inner = self.lock();
        let events = inner.events.clone()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = oneshot::channel();
        inner.pending.insert(id, sender);
        Some(Pending { id, answer, events })
    }

    /// Drops a wait that no answer will reach, so a late answer finds no one.
    fn give_up(&self, id: u64) {
        self.lock().pending.remove(&id);
    }
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
        let Some(pending) = self.questions.open() else {
            return ToolOutput::ok(NOBODY);
        };
        let request = Event::QuestionRequest(Box::new(QuestionRequest {
            id: pending.id,
            questions: questions.clone(),
        }));
        if emit(&pending.events, cancel, request).await {
            self.questions.give_up(pending.id);
            self.resolved(&pending.events, pending.id, 0, cancel).await;
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
                return ToolOutput::ok(UNANSWERED);
            }
        };
        self.resolved(&pending.events, pending.id, answered.0, cancel)
            .await;
        match answered.1 {
            Some(picks) => told(&questions, &picks),
            None => ToolOutput::ok(UNANSWERED),
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
        if asked.q.trim().is_empty() {
            return Err(ToolOutput::error(format!(
                "invalid arguments: question {} has no `q` text",
                at + 1
            )));
        }
        // The card offers its own typed answer, so a question needs two options
        // to be a choice. More than the spec's four still draw, so they pass.
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
        out.push(Question {
            q: Text::from(asked.q.as_ref()),
            options,
            multi: asked.multi,
        });
    }
    Ok(out)
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
        let first = questions.open().expect("a client listens");
        let second = questions.open().expect("a client listens");
        assert_eq!((first.id, second.id), (0, 1), "the ids count up");

        assert!(questions.answer(3, first.id, Some(vec![vec!["sqlite".to_owned()]])));
        let (client, answers) = first.answer.await.expect("the answer arrives");
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

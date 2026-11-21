//! Asking a live session's clients to approve a tool call, and their answers.
//!
//! The guard says an [`crate::guard::Ask`] for a call whose rule needs a person.
//! The ask becomes an [`Event::ApprovalRequest`] with an id, and the call waits
//! on one channel for the answer of any client and on the run's cancel token,
//! with no timer: a call that nobody answers waits until the run is cancelled.
//! Silence is never a yes, so a run with no listener refuses instead of waiting.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use reagent_ai::Text;
use reagent_config::shell;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::emit;
use crate::event::{ApprovalRequest, Event};
use crate::live::ClientId;

/// The id of one approval request, counting up through a session.
pub type ApprovalId = u64;

/// The answer a client gives to an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Run this call, and ask again for the next one.
    Once,
    /// Run this call, and every later call the shown rules match this session.
    Session,
    /// As `Session`, and write the shown rules to the config.
    Save,
    /// Do not run this call, and tell the model why.
    Refuse {
        /// The reason the model reads; a default when it is `None`.
        reason: Option<Text>,
    },
    /// Do not run this call; the model continues without it.
    Skip,
}

/// The programs a saved rule may not name: a rule for a shell, an interpreter
/// or a wrapper would allow whatever text the call hands it.
const WRAPPERS: [&str; 27] = [
    "sh", "bash", "zsh", "fish", "dash", "python", "python3", "node", "deno", "bun", "ruby",
    "perl", "php", "lua", "npx", "pnpx", "bunx", "env", "sudo", "doas", "xargs", "exec", "nohup",
    "time", "command", "builtin", "eval",
];

/// The most rules one compound command may offer to save.
const MAX_RULES: usize = 5;

/// Whether a program a saved rule names would hand the call to another
/// program, so a rule for it would allow more than the call it was shown for.
pub fn is_wrapper(program: &str) -> bool {
    WRAPPERS.contains(&shell::program_name(program))
}

/// Whether any simple command of `value` runs a shell, an interpreter or a
/// wrapper, as the parser or the raw text shows it.
pub fn names_a_wrapper(value: &str) -> bool {
    starts_with_a_wrapper(value)
        || shell::parse(value)
            .parts
            .iter()
            .any(|part| part.words.first().is_some_and(|word| is_wrapper(word)))
}

/// The rules a save would write for a call: the tool, the program and its
/// first word after the options, one rule per simple command.
///
/// `None` when no rule may be written for the call: it is not a command, it
/// did not parse cleanly, its program is a shell, an interpreter or a wrapper,
/// a rule would name only a program, or it has more than [`MAX_RULES`] parts.
pub fn save_rules(tool: &str, main_arg: Option<&str>, value: Option<&str>) -> Option<Box<[Text]>> {
    if main_arg != Some("command") {
        return None;
    }
    let value = value?;
    let command = shell::parse(value);
    if command.complex
        || command.unknown_program
        || command.parts.is_empty()
        || command.parts.len() > MAX_RULES
        // A wrapper the parser drops (`env`, `nohup`, `time`) is invisible in
        // the parts, so the text is read for it.
        || starts_with_a_wrapper(value)
    {
        return None;
    }
    let name = tool_name(tool);
    let mut rules = Vec::with_capacity(command.parts.len());
    for part in &command.parts {
        let mut words = part.words.iter();
        let program = words.next()?;
        // A rule for a shell or a wrapper would allow the text it is given.
        if !is_plain(program) || is_wrapper(program) {
            return None;
        }
        // A rule matches the leading words as written, so a flag before the
        // word would give a rule that never matches the call it was shown for.
        let word = words.next()?;
        if word.starts_with('-') || !is_plain(word) {
            return None;
        }
        rules.push(Text::from(format!("{name}({program} {word}:*)")));
    }
    Some(rules.into_boxed_slice())
}

/// Whether any simple command of `value` begins with a shell, an interpreter
/// or a wrapper, as far as the raw text can tell.
fn starts_with_a_wrapper(value: &str) -> bool {
    value.split([';', '&', '|']).any(|segment| {
        segment
            .split_whitespace()
            .next()
            .map(|word| word.trim_matches(['(', ')', '{', '}']))
            .is_some_and(is_wrapper)
    })
}

/// Whether a word can be written inside a rule: the rule splits its words on
/// whitespace and reads `)` and `:` in its text.
fn is_plain(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./@+".contains(c))
}

/// The tool name as a rule writes it, such as `Bash` for `bash`.
fn tool_name(tool: &str) -> String {
    let mut chars = tool.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What came of a request for approval.
pub(crate) enum Waited {
    /// A client answered.
    Answered(Answer),
    /// The run was cancelled while the call waited.
    Cancelled,
    /// No answer could come: the event channel or the answer channel is gone.
    Silent,
    /// The run parked the wait: a later process answers it, so the call has
    /// no result in this run.
    Parked,
}

/// The calls of one session that wait for an answer, by id.
///
/// The ids count up for the life of the guard, so a client that answers an id
/// from an earlier run gets the error of an answer nobody waits for.
#[derive(Debug, Default)]
pub struct Approvals {
    next: AtomicU64,
    pending: Mutex<HashMap<ApprovalId, oneshot::Sender<Answer>>>,
    parking: Mutex<Parking>,
}

/// What the registry knows of waits a later process may answer.
#[derive(Debug, Default)]
struct Parking {
    /// The call of each open wait whose request the journal holds with an id.
    parkable: HashMap<ApprovalId, String>,
    /// Whether the run parked: a wait that can park ends parked at once.
    parked: bool,
    /// The calls a park ended, so their results stay out of the journal.
    stopped: HashSet<String>,
    /// The id a resumed call waits under again, by call id.
    reused: HashMap<String, String>,
    /// The answer a later process gave, by call id: the call takes it and
    /// asks no client.
    given: HashMap<String, Answer>,
}

impl Approvals {
    /// Takes the id of the next request and the channel its answer arrives on.
    fn open(&self) -> (ApprovalId, oneshot::Receiver<Answer>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, sender);
        }
        (id, receiver)
    }

    /// Takes the sender of `id`: `None` when no call waits for it any more.
    fn take(&self, id: ApprovalId) -> Option<oneshot::Sender<Answer>> {
        if let Ok(mut parking) = self.parking.lock() {
            parking.parkable.remove(&id);
        }
        self.pending.lock().ok()?.remove(&id)
    }

    fn parking(&self) -> std::sync::MutexGuard<'_, Parking> {
        self.parking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Says that the resumed call `call` waits again under `qid`, whose wait a
    /// process that ended already recorded: the wait writes no second record.
    pub fn reuse(&self, call: &str, qid: &str) {
        self.parking()
            .reused
            .insert(call.to_owned(), qid.to_owned());
    }

    /// Gives the resumed call `call` the answer a later process wrote, so the
    /// call takes it and asks no client.
    pub fn give(&self, call: &str, answer: Answer) {
        self.parking().given.insert(call.to_owned(), answer);
    }

    /// The id the wait of `call` carries, and whether the journal already
    /// holds it: a resumed call keeps its old id once, any other call gets a
    /// new one.
    pub(crate) fn qid(&self, call: &str) -> (String, bool) {
        match self.parking().reused.remove(call) {
            Some(qid) => (qid, true),
            None => (crate::question::fresh_qid(), false),
        }
    }

    /// Takes the answer a later process gave the wait of `call`.
    pub(crate) fn take_given(&self, call: &str) -> Option<Answer> {
        self.parking().given.remove(call)
    }

    /// Drops what a resumed batch left: an id or an answer no call took,
    /// because the check of its call no longer asked.
    pub(crate) fn settle(&self) {
        let mut parking = self.parking();
        parking.reused.clear();
        parking.given.clear();
    }

    /// Parks the waits the journal holds with an id: each ends parked, and a
    /// wait that opens after this ends parked at once.
    ///
    /// A parked call leaves no result, so the run ends once the other calls
    /// of the reply are in, and a later process answers the wait and resumes
    /// the session.
    pub fn park(&self) {
        let ids: Vec<ApprovalId> = {
            let mut parking = self.parking();
            parking.parked = true;
            let parkable = std::mem::take(&mut parking.parkable);
            let ids = parkable.keys().copied().collect();
            parking.stopped.extend(parkable.into_values());
            ids
        };
        // Dropping a sender ends its wait; the wait reads the park.
        if let Ok(mut pending) = self.pending.lock() {
            for id in ids {
                pending.remove(&id);
            }
        }
    }

    /// Whether a park now leaves a wait a later process answers.
    pub fn parkable(&self) -> bool {
        !self.parking().parkable.is_empty()
    }

    /// Whether a park ended the wait of `call`, so its result stays out of
    /// the journal and the history.
    pub fn parked_call(&self, call: &str) -> bool {
        self.parking().stopped.contains(call)
    }
}

/// The clients of a live session an ask reaches.
#[derive(Clone, Debug)]
pub struct Approver {
    events: mpsc::Sender<Event>,
    approvals: Arc<Approvals>,
}

impl Approver {
    /// An approver that asks on `events` and waits in `approvals`.
    pub fn new(events: mpsc::Sender<Event>, approvals: Arc<Approvals>) -> Self {
        Self { events, approvals }
    }

    /// Asks a client to approve the call an [`crate::guard::Ask`] describes.
    ///
    /// The call waits on the answer and on the token only, so a cancel reaches
    /// it at once and an unanswered request waits until the run ends.
    pub(crate) async fn ask(
        &self,
        ask: &crate::guard::Ask,
        call: Option<&str>,
        token: &CancellationToken,
    ) -> Waited {
        // A call whose wait the journal holds with an id may park: after a park
        // it parks at once, and until then a park ends it.
        let (id, receiver) = {
            let mut parking = self.approvals.parking();
            if let Some(call) = call
                && parking.parked
            {
                parking.stopped.insert(call.to_owned());
                return Waited::Parked;
            }
            let (id, receiver) = self.approvals.open();
            if let Some(call) = call {
                parking.parkable.insert(id, call.to_owned());
            }
            (id, receiver)
        };
        let request = Event::ApprovalRequest(Box::new(ApprovalRequest {
            id,
            tool: ask.tool.clone(),
            call: ask.call.clone(),
            rule: ask.rule.clone(),
            save: ask.save.clone(),
        }));
        if emit(&self.events, token, request).await {
            self.approvals.take(id);
            return if token.is_cancelled() {
                Waited::Cancelled
            } else {
                Waited::Silent
            };
        }
        let answer = tokio::select! {
            answer = receiver => answer,
            () = token.cancelled() => {
                // The call does not run, and a late answer finds no one to take it.
                self.approvals.take(id);
                return Waited::Cancelled;
            }
        };
        match answer {
            Ok(answer) => Waited::Answered(answer),
            Err(_) => {
                self.approvals.take(id);
                if call.is_some_and(|call| self.approvals.parked_call(call)) {
                    Waited::Parked
                } else {
                    Waited::Silent
                }
            }
        }
    }

    /// Sends `answer` to the call that waits for approval `id`, and tells every
    /// client who answered. Returns whether a call waited: a second answer to
    /// the same id finds none.
    pub(crate) async fn answer(
        &self,
        client: ClientId,
        id: ApprovalId,
        answer: Answer,
        token: &CancellationToken,
    ) -> bool {
        let Some(sender) = self.approvals.take(id) else {
            return false;
        };
        let resolved = Event::ApprovalResolved {
            id,
            client,
            answer: answer.clone(),
        };
        // The call is answered even when no event reaches a client.
        let _ = sender.send(answer);
        emit(&self.events, token, resolved).await;
        true
    }

    /// Tells every client which rule a save writes.
    pub(crate) async fn saving(&self, rule: &Text, token: &CancellationToken) {
        let event = Event::RuleToSave { rule: rule.clone() };
        emit(&self.events, token, event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(tool: &str, command: &str) -> Option<Vec<String>> {
        save_rules(tool, Some("command"), Some(command))
            .map(|rules| rules.iter().map(ToString::to_string).collect())
    }

    fn one(tool: &str, command: &str) -> Option<String> {
        rules(tool, command).map(|rules| rules.join(", "))
    }

    #[test]
    fn save_rule_names_the_program_and_its_first_word() {
        assert_eq!(
            one("bash", "cargo test -p a").as_deref(),
            Some("Bash(cargo test:*)")
        );
        assert_eq!(one("bash", "ls").as_deref(), None);
        assert_eq!(one("bash", "ls -la").as_deref(), None);
        // A flag before the word: the rule would never match the call.
        assert_eq!(one("bash", "git -C . push origin main").as_deref(), None);
        assert_eq!(one("bash", "rm -rf x").as_deref(), None);
        assert_eq!(
            one("bash", "./target/debug/app serve").as_deref(),
            Some("Bash(./target/debug/app serve:*)")
        );
    }

    #[test]
    fn save_rule_refuses_a_shell_an_interpreter_or_a_wrapper() {
        for command in [
            "python3 -c 'print(1)'",
            "bash -c x",
            "sudo ls /",
            "env FOO=1 cargo test",
            "nohup cargo test",
            "xargs rm",
            "eval 'rm -rf x'",
            "sh script.sh",
        ] {
            assert_eq!(one("bash", command), None, "{command}");
        }
    }

    #[test]
    fn save_rule_refuses_a_command_that_did_not_parse() {
        assert_eq!(one("bash", "echo $(cat secret)"), None);
        assert_eq!(one("bash", "$PROGRAM test"), None);
        // Six simple commands are more rules than one save may offer.
        assert_eq!(one("bash", "a b && c d && e f && g h && i j && k l"), None);
    }

    #[test]
    fn save_rule_gives_one_rule_per_simple_command() {
        assert_eq!(
            rules("bash", "cargo build && cargo test").as_deref(),
            Some(
                [
                    "Bash(cargo build:*)".to_string(),
                    "Bash(cargo test:*)".to_string()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn save_rule_is_for_a_command_and_no_other_argument() {
        assert!(save_rules("write", Some("path"), Some("src/a.rs")).is_none());
        assert!(save_rules("bash", Some("command"), None).is_none());
    }

    fn ask() -> crate::guard::Ask {
        crate::guard::Ask {
            tool: Text::from("bash"),
            call: Text::from("bash: cargo test"),
            rule: Text::from("default"),
            file: None,
            save: None,
            rules: Box::default(),
        }
    }

    #[tokio::test]
    async fn an_answer_reaches_the_call_that_waits_once() {
        let approvals = Arc::new(Approvals::default());
        let (events, mut seen) = mpsc::channel(8);
        let approver = Approver::new(events, Arc::clone(&approvals));
        let token = CancellationToken::new();
        let responder = approver.clone();
        let waited = token.clone();
        let wait = tokio::spawn(async move { responder.ask(&ask(), None, &waited).await });
        let Some(Event::ApprovalRequest(request)) = seen.recv().await else {
            panic!("expected an approval request");
        };
        let id = request.id;
        assert!(approver.answer(1, id, Answer::Once, &token).await);
        let Some(Event::ApprovalResolved { id: done, .. }) = seen.recv().await else {
            panic!("expected a resolved event");
        };
        assert_eq!(done, id);
        assert!(matches!(
            wait.await.unwrap(),
            Waited::Answered(Answer::Once)
        ));
        // A second answer to the same id has no call to reach.
        assert!(!approver.answer(2, id, Answer::Skip, &token).await);
    }

    #[tokio::test]
    async fn a_cancel_ends_the_wait_and_a_late_answer_finds_no_one() {
        let approvals = Arc::new(Approvals::default());
        let (events, mut seen) = mpsc::channel(8);
        let approver = Approver::new(events, Arc::clone(&approvals));
        let token = CancellationToken::new();
        let responder = approver.clone();
        let cancel = token.clone();
        let wait = tokio::spawn(async move { responder.ask(&ask(), None, &cancel).await });
        let Some(Event::ApprovalRequest(request)) = seen.recv().await else {
            panic!("expected an approval request");
        };
        let id = request.id;
        token.cancel();
        assert!(matches!(wait.await.unwrap(), Waited::Cancelled));
        assert!(!approver.answer(1, id, Answer::Once, &token).await);
    }
}

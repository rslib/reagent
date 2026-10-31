//! Rebuilding a run from its journal.
//!
//! The read starts at the last base record, so a long run does not replay the
//! records before it. A tool call that was already sent and has no result may
//! have changed the world: it is not rerun, and its result says so instead. A
//! read, and a call that never started, is safe to run again and comes back in
//! [`Resumed::pending`].

use std::collections::{HashMap, HashSet};
use std::io;

use reagent_ai::{Content, Message, Text};
use reagent_config::permissions::PermissionMode;
use reagent_store::Session;

use crate::journal::Journal;
use crate::journal::record::{self, ApprovalKind, Aside, Record};
use crate::journal::state::State;
use crate::tool::EffectClass;

/// The result a call that may have run gets after a crash.
pub const INTERRUPTED: &str = "interrupted: check whether it ran";

/// A run rebuilt from its journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resumed {
    /// The messages the run continues from.
    pub history: Vec<Message>,
    /// The agent state at the last base, with the changes after it applied.
    pub state: State,
    /// The provider the run was using.
    pub provider: String,
    /// The model the run was using.
    pub model: String,
    /// The number of the last turn that ended, 0 when none did.
    pub turns: u64,
    /// How many messages of `history` the part chain already holds.
    ///
    /// The messages after it come from this read: a result it wrote for a
    /// call that may have run. A resumed part writes them itself, so the
    /// history keeps its order when it is read again.
    pub recorded: usize,
    /// The calls to run again, in the order the model asked for them.
    pub pending: Vec<PendingCall>,
    /// Whether the last turn has no `TURN_END`.
    pub open_turn: bool,
    /// The reasoning level the run last moved to, when a settings record says.
    pub level: Option<Text>,
    /// The permission mode the run last moved to, when a settings record says.
    pub mode: Option<PermissionMode>,
}

/// A tool call a resumed run has to run again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCall {
    /// The call id the result will answer.
    pub id: String,
    /// The tool the model named.
    pub name: String,
    /// The class the journal recorded, when the call started.
    pub class: Option<EffectClass>,
}

/// A turn the journal does not have.
const NO_TURN: &str = "the session has no turn";

/// Reads the journal from its last base and rebuilds the run.
///
/// Reading blocks, so a caller runs this in `spawn_blocking`.
pub fn load(session: &Session) -> io::Result<Resumed> {
    Ok(read(session, None)?.0)
}

/// Reads the run and the sequence number of the last record it shows.
///
/// A resumed part forks at that record, so the read reports it too.
pub fn load_tail(session: &Session) -> io::Result<(Resumed, u64)> {
    read(session, None)
}

/// Reads the run up to the end of `turn`, for a fork.
///
/// Returns the run and the sequence number of that turn end, which the new
/// part forks at. A turn the journal does not have is an error.
pub fn load_to(session: &Session, turn: u64) -> io::Result<(Resumed, u64)> {
    read(session, Some(turn))
}

/// Marks the first `len` messages of the history as recorded by a parent part.
///
/// A resumed run that answers pending calls writes their results itself, so
/// the next write of the run starts after them.
pub fn mark_recorded(journal: &mut Journal, len: usize) {
    journal.recorded = Some(len);
}

/// Reads the journal up to `until`, or to its end when that is `None`.
fn read(session: &Session, until: Option<u64>) -> io::Result<(Resumed, u64)> {
    let from = session
        .last_base()
        .unwrap_or(reagent_store::part::FIRST_SEQ);
    let mut last = 0;
    let mut stopped = false;
    let mut resumed = Resumed {
        history: Vec::new(),
        state: State::default(),
        provider: String::new(),
        model: String::new(),
        turns: 0,
        recorded: 0,
        pending: Vec::new(),
        open_turn: false,
        level: None,
        mode: None,
    };
    // The class of each call that started, in the order it started.
    let mut started: Vec<(String, String, EffectClass)> = Vec::new();
    let mut answered: HashSet<Text> = HashSet::new();
    // The kind of the last approval record of each call.
    let mut approvals: HashMap<String, ApprovalKind> = HashMap::new();
    // The tool calls of the last assistant message, in the order sent.
    let mut last_calls: Vec<(Text, Text)> = Vec::new();
    // The results that follow the last assistant message. A batch commits each
    // result as it returns, so they may be out of call order.
    let mut trailing: Vec<Message> = Vec::new();
    // The provider and model of the last assistant reply, for a log with no base.
    let mut last_provider: Option<Text> = None;
    let mut last_model: Option<Text> = None;
    for record in session.records_from(from) {
        let record = record?;
        last = record.seq;
        // An approval or a settings change is not part of the replayed history;
        // it decides what the history means.
        if let Some(aside) = record::decode_aside(record.kind, &record.payload)? {
            match aside {
                Aside::Approval(approval) => {
                    approvals.insert(approval.id, approval.kind);
                }
                Aside::Settings(change) => {
                    if let Some(level) = change.level {
                        resumed.level = Some(Text::from(level));
                    }
                    if let Some(mode) = change.mode {
                        resumed.mode = mode.parse().ok();
                    }
                }
            }
            continue;
        }
        let decoded = record::decode(record.kind, &record.payload)?;
        // The turn a fork stops at ends the read, and the last record read is
        // the turn end the new part forks at.
        let ends = match (&decoded, until) {
            (Record::TurnEnd(end), Some(turn)) => end.turn == turn,
            _ => false,
        };
        // A record that is not a tool result ends the results of the reply
        // before it, which are in call order from here on.
        if !matches!(decoded, Record::Message(Message::ToolResult(_))) {
            ordered(&mut resumed.history, &mut trailing, &last_calls);
        }
        match decoded {
            Record::Message(message) => {
                match &message {
                    Message::Assistant(reply) => {
                        last_calls = tool_calls(&reply.content);
                        last_provider = Some(reply.provider.clone());
                        last_model = Some(reply.model.clone());
                    }
                    Message::ToolResult(result) => {
                        answered.insert(result.call_id.clone());
                        trailing.push(message);
                        resumed.open_turn = true;
                        continue;
                    }
                    Message::User { .. } => {}
                }
                resumed.history.push(message);
                resumed.open_turn = true;
            }
            Record::CallStart(call) => {
                started.push((call.id, call.name, call.class));
                resumed.open_turn = true;
            }
            Record::TurnEnd(end) => {
                resumed.turns = end.turn;
                resumed.open_turn = false;
            }
            // A base replaces the history before it: the messages after it
            // are the history the run continued with.
            Record::Base(state) => {
                resumed.state = state;
                resumed.history.clear();
                started.clear();
                answered.clear();
                approvals.clear();
                last_calls.clear();
                resumed.level = None;
                resumed.mode = None;
            }
            Record::Model(change) => {
                resumed.state.provider = change.provider;
                resumed.state.model = change.model;
            }
            Record::Unknown(_) => {}
        }
        if ends {
            stopped = true;
            break;
        }
    }
    ordered(&mut resumed.history, &mut trailing, &last_calls);
    if let Some(turn) = until
        && !stopped
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{NO_TURN} {turn}"),
        ));
    }
    // Everything read so far is on the disk; what `settle` adds is not.
    resumed.recorded = resumed.history.len();
    settle(&mut resumed, &started, &answered, &approvals, &last_calls);
    if resumed.state.provider.is_empty()
        && let Some(provider) = last_provider
    {
        resumed.state.provider = provider.to_string();
    }
    if resumed.state.model.is_empty()
        && let Some(model) = last_model
    {
        resumed.state.model = model.to_string();
    }
    resumed.provider = resumed.state.provider.clone();
    resumed.model = resumed.state.model.clone();
    Ok((resumed, last))
}

/// Puts the tool results of the last assistant message in the order of its
/// calls and appends them to the history.
///
/// The log may hold the results of one reply in the order they finished; the
/// history the model saw is in the order the reply asked for the calls.
fn ordered(history: &mut Vec<Message>, trailing: &mut Vec<Message>, last_calls: &[(Text, Text)]) {
    if trailing.is_empty() {
        return;
    }
    let mut results = std::mem::take(trailing);
    for (id, _) in last_calls {
        let Some(at) = results.iter().position(
            |message| matches!(message, Message::ToolResult(result) if result.call_id == *id),
        ) else {
            continue;
        };
        history.push(results.remove(at));
    }
    // A result no call of the reply names, which a log cut in an odd place
    // leaves behind, keeps its place after the ones that do.
    history.append(&mut results);
}

/// Applies the crash rule to the calls without a result.
fn settle(
    resumed: &mut Resumed,
    started: &[(String, String, EffectClass)],
    answered: &HashSet<Text>,
    approvals: &HashMap<String, ApprovalKind>,
    last_calls: &[(Text, Text)],
) {
    let classes: HashMap<&str, EffectClass> = started
        .iter()
        .map(|(id, _, class)| (id.as_str(), *class))
        .collect();
    let mut seen: HashSet<Text> = HashSet::new();
    // The calls of the last assistant message first, in the order it asked,
    // then any other started call a log cut in an odd place left behind.
    for (id, name) in last_calls.iter().chain(unasked(started, last_calls).iter()) {
        if answered.contains(id) || !seen.insert(id.clone()) {
            continue;
        }
        settle_one(
            resumed,
            id,
            name,
            classes.get(&**id).copied(),
            approvals.get(&**id).copied(),
        );
    }
}

/// The calls that started but are not in the last assistant message.
fn unasked(
    started: &[(String, String, EffectClass)],
    last_calls: &[(Text, Text)],
) -> Vec<(Text, Text)> {
    let known: HashSet<&str> = last_calls.iter().map(|(id, _)| &**id).collect();
    started
        .iter()
        .filter(|(id, _, _)| !known.contains(id.as_str()))
        .map(|(id, name, _)| (Text::from(id.as_str()), Text::from(name.as_str())))
        .collect()
}

/// Applies the crash rule to one call without a result.
fn settle_one(
    resumed: &mut Resumed,
    id: &str,
    name: &str,
    class: Option<EffectClass>,
    approval: Option<ApprovalKind>,
) {
    // A call whose last approval record is a wait or a refusal never ran: the
    // call is pending, so the guard asks again after the resume.
    let rerun = match (class, approval) {
        (_, Some(ApprovalKind::Wait | ApprovalKind::Refused)) | (None, _) => true,
        // A read changes nothing, so it runs again.
        (Some(EffectClass::Read), _) => true,
        // A side effect may have happened: report it instead of repeating it.
        (Some(_), _) => false,
    };
    if rerun {
        resumed.pending.push(PendingCall {
            id: id.to_owned(),
            name: name.to_owned(),
            class,
        });
    } else {
        resumed
            .history
            .push(Message::tool_result(id, INTERRUPTED, true));
    }
}

/// The tool calls of one assistant message, in the order sent.
fn tool_calls(content: &[Content]) -> Vec<(Text, Text)> {
    content
        .iter()
        .filter_map(|block| match block {
            Content::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bytes::Bytes;
    use reagent_ai::{
        AssistantMessage, Content, Image, ImageType, StopReason, Text, ToolCall, Usage,
    };
    use reagent_store::Header;

    use crate::journal::Journal;
    use crate::journal::state::State;

    use super::*;
    /// Opens a journal over `dir`, as a run does.
    fn journal(dir: &Path) -> Journal {
        let header = Header::new(
            "session".into(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        Journal::create(dir, header).expect("create the journal")
    }

    /// Rebuilds the run `dir` holds.
    fn load_now(dir: &Path) -> Resumed {
        let session = Session::open(dir).expect("open the session");
        load(&session).expect("load the run")
    }

    /// One image, with base64 text the journal stores as it stands.
    fn image(data: &str) -> Image {
        Image {
            media_type: ImageType::Png,
            width: 2,
            height: 3,
            data: data.into(),
        }
    }

    /// An assistant message that calls `calls`, in order.
    fn call_reply(calls: &[(&str, &str)]) -> Message {
        let content = calls
            .iter()
            .map(|(id, name)| {
                Content::ToolCall(ToolCall {
                    id: Text::from(*id),
                    name: Text::from(*name),
                    arguments: Text::from("{}"),
                })
            })
            .collect();
        let mut reply = AssistantMessage::new("p", "m", content);
        reply.stop_reason = Some(StopReason::ToolUse);
        Message::assistant(reply)
    }

    #[tokio::test]
    async fn a_clean_session_gives_the_history_it_wrote() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let base = State::new("p", "m", 0);
        journal.base(&base).expect("base");
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "read")]);
        let result = Message::tool_result("c1", "text", false);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal.message(&result).expect("append");
        journal.turn_end(1, Usage::default()).expect("turn end");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [user, reply, result]);
        assert_eq!(resumed.state, base);
        assert_eq!(resumed.provider, "p");
        assert_eq!(resumed.model, "m");
        assert_eq!(resumed.turns, 1);
        assert!(!resumed.open_turn, "the turn end closed the turn");
        assert_eq!(resumed.recorded, 3, "every message is on the disk");
        assert!(resumed.pending.is_empty());
    }

    #[tokio::test]
    async fn a_model_record_changes_the_state_after_the_base() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        journal.base(&State::new("p", "m1", 2)).expect("base");
        journal.model("q", "m2").expect("model");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.state.provider, "q");
        assert_eq!(resumed.state.model, "m2");
        assert_eq!(resumed.state.compaction, 2, "the rest of the base stays");
        assert_eq!(resumed.provider, "q");
        assert_eq!(resumed.model, "m2");
    }

    #[tokio::test]
    async fn an_interrupted_side_effect_call_gets_the_interrupted_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "bash")]);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal
            .call_start("c1", "bash", EffectClass::LocalReversible)
            .expect("call start");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [user, reply, Message::tool_result("c1", INTERRUPTED, true),]
        );
        // The part holds the call, not the result this read made up, so the
        // resumed part writes the result itself.
        assert_eq!(resumed.recorded, 2);
        assert!(resumed.pending.is_empty(), "never run again");
        assert!(resumed.open_turn, "the turn has no end");
    }

    #[tokio::test]
    async fn an_interrupted_read_is_pending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let reply = call_reply(&[("c1", "read")]);
        journal.message(&Message::user("hello")).expect("append");
        journal.message(&reply).expect("append");
        journal
            .call_start("c1", "read", EffectClass::Read)
            .expect("call start");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [Message::user("hello"), reply]);
        assert_eq!(
            resumed.pending,
            [PendingCall {
                id: "c1".into(),
                name: "read".into(),
                class: Some(EffectClass::Read),
            }]
        );
    }

    #[tokio::test]
    async fn a_never_started_call_is_pending() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let reply = call_reply(&[("c1", "bash"), ("c2", "read")]);
        journal.message(&Message::user("hello")).expect("append");
        journal.message(&reply).expect("append");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [Message::user("hello"), reply]);
        assert_eq!(
            resumed.pending,
            [
                PendingCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    class: None,
                },
                PendingCall {
                    id: "c2".into(),
                    name: "read".into(),
                    class: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn tool_results_resume_in_call_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "read"), ("c2", "read"), ("c3", "read")]);
        let first = Message::tool_result("c1", "one", false);
        let second = Message::tool_result("c2", "two", false);
        let third = Message::tool_result("c3", "three", false);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        // The batch of one reply commits each result as it comes back, so the
        // log holds them in the order the calls finished.
        journal.message(&third).expect("append");
        journal.message(&first).expect("append");
        journal.message(&second).expect("append");
        journal.turn_end(1, Usage::default()).expect("turn end");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [user, reply, first, second, third],
            "a resume gives the order the model saw"
        );
    }

    #[tokio::test]
    async fn a_call_that_waited_for_an_answer_never_ran() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "bash")]);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal
            .call_start("c1", "bash", EffectClass::LocalReversible)
            .expect("call start");
        // The call waits for a client, and no answer came before the crash.
        journal
            .approval("c1", ApprovalKind::Wait)
            .expect("approval wait");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [user, reply], "no interrupted result");
        assert_eq!(
            resumed.pending,
            [PendingCall {
                id: "c1".into(),
                name: "bash".into(),
                class: Some(EffectClass::LocalReversible),
            }],
            "the rerun asks the same question again"
        );
    }

    #[tokio::test]
    async fn a_call_refused_before_its_result_never_ran() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "bash")]);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal
            .call_start("c1", "bash", EffectClass::LocalReversible)
            .expect("call start");
        journal
            .approval("c1", ApprovalKind::Wait)
            .expect("approval wait");
        // The client refused, and the process died before the refusal result.
        journal
            .approval("c1", ApprovalKind::Refused)
            .expect("approval answer");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [user, reply], "no interrupted result");
        assert_eq!(resumed.pending.len(), 1, "the guard asks again");
    }

    #[tokio::test]
    async fn a_call_answered_to_run_is_a_started_call() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user = Message::user("hello");
        let reply = call_reply(&[("c1", "bash")]);
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal
            .call_start("c1", "bash", EffectClass::LocalReversible)
            .expect("call start");
        journal
            .approval("c1", ApprovalKind::Wait)
            .expect("approval wait");
        journal
            .approval("c1", ApprovalKind::Run)
            .expect("approval answer");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [user, reply, Message::tool_result("c1", INTERRUPTED, true)],
            "an answer to run and no result: it may have run"
        );
        assert!(resumed.pending.is_empty());
    }

    #[tokio::test]
    async fn settings_records_give_the_last_level_and_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        journal.base(&State::new("p", "m", 0)).expect("base");
        journal
            .settings(Some("high"), Some("accept-edits"))
            .expect("settings");
        journal.settings(Some("low"), None).expect("settings");
        journal.model("p", "m2").expect("model");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.level.as_deref(), Some("low"), "the last level wins");
        assert_eq!(
            resumed.mode,
            Some(PermissionMode::AcceptEdits),
            "a change that sets no mode keeps the mode"
        );
        assert_eq!(resumed.model, "m2", "the model has its own record");
    }

    #[tokio::test]
    async fn reading_starts_at_the_last_base() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        for n in 0..500u32 {
            let text = format!("{n:08x}{n:08x}{n:08x}{n:08x}{n:08x}{n:08x}{n:08x}");
            journal.message(&Message::user(text)).expect("append");
        }
        journal.commit().expect("commit");
        let base = State::new("p", "m", 1);
        journal.base(&base).expect("base");
        let after = Message::user("after the base");
        journal.message(&after).expect("append");
        journal.commit().expect("commit");
        let part = reagent_store::paths::part_path(dir.path(), journal.part_id());
        journal.close().expect("close");

        let len = tokio::fs::metadata(&part)
            .await
            .expect("part metadata")
            .len();
        let session = Session::open(dir.path()).expect("open the session");
        let resumed = load(&session).expect("load the run");
        let read = session.bytes_read();
        assert_eq!(resumed.history, [after]);
        assert_eq!(resumed.state, base);
        assert!(
            read * 2 < len,
            "read {read} of {len} bytes: the read did not start at the base"
        );
    }

    #[tokio::test]
    async fn a_fork_gives_the_parents_history_then_its_own() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut parent = journal(dir.path());
        let a1 = Message::user("a1");
        let a2 = Message::assistant(AssistantMessage::new("p", "m", vec![Content::text("a2")]));
        parent.message(&a1).expect("append");
        parent.message(&a2).expect("append");
        parent.commit().expect("commit");
        parent.close().expect("close");

        let session = Session::open(dir.path()).expect("open the parent");
        let last = session
            .records()
            .filter_map(Result::ok)
            .map(|record| record.seq)
            .max()
            .expect("a record");
        let fork = session.fork_point(last + 1);
        let created = session.header().created_ms + 1;
        let session_id = session.header().session_id.clone();
        drop(session);

        let header = Header::new(session_id, created, Some(fork), Bytes::new());
        let mut child = Journal::create(dir.path(), header).expect("create the child");
        let b1 = Message::user("b1");
        child.message(&b1).expect("append");
        child.commit().expect("commit");
        child.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(resumed.history, [a1, a2, b1]);
    }

    #[tokio::test]
    async fn a_resume_gives_back_the_images_it_wrote() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut journal = journal(dir.path());
        let user =
            Message::user_content(vec![Content::text("look"), Content::Image(image("aGk="))]);
        let reply = call_reply(&[("c1", "read")]);
        let result = Message::tool_result_with_images(
            "c1",
            "two frames",
            false,
            vec![image("aGk="), image("bG8=")],
        );
        journal.message(&user).expect("append");
        journal.message(&reply).expect("append");
        journal.message(&result).expect("append");
        journal.turn_end(1, Usage::default()).expect("turn end");
        journal.commit().expect("commit");
        journal.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [user, reply, result],
            "the images come back byte for byte"
        );
    }

    #[tokio::test]
    async fn a_fork_keeps_the_images_of_the_parent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut parent = journal(dir.path());
        let a1 = Message::user_content(vec![Content::text("a1"), Content::Image(image("aGk="))]);
        let a2 = Message::tool_result_with_images("c1", "a2", false, vec![image("bG8=")]);
        parent.message(&a1).expect("append");
        parent.message(&a2).expect("append");
        parent.commit().expect("commit");
        parent.close().expect("close");

        let session = Session::open(dir.path()).expect("open the parent");
        let last = session
            .records()
            .filter_map(Result::ok)
            .map(|record| record.seq)
            .max()
            .expect("a record");
        let fork = session.fork_point(last + 1);
        let created = session.header().created_ms + 1;
        let session_id = session.header().session_id.clone();
        drop(session);

        let header = Header::new(session_id, created, Some(fork), Bytes::new());
        let mut child = Journal::create(dir.path(), header).expect("create the child");
        let b1 = Message::user("b1");
        child.message(&b1).expect("append");
        child.commit().expect("commit");
        child.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [a1, a2, b1],
            "the images of the parent reach the fork"
        );
    }

    #[tokio::test]
    async fn a_base_in_the_parent_part_is_not_read_twice() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut parent = journal(dir.path());
        let old = Message::user("before the compaction");
        let kept = Message::user("the compacted history");
        parent.message(&old).expect("append");
        parent.commit().expect("commit");
        parent.base(&State::new("p", "m", 1)).expect("base");
        parent.message(&kept).expect("append");
        parent.commit().expect("commit");
        parent.close().expect("close");

        let session = Session::open(dir.path()).expect("open the parent");
        let last = session
            .records()
            .filter_map(Result::ok)
            .map(|record| record.seq)
            .max();
        let fork = session.fork_point(last.expect("a record"));
        let header = Header::new(
            session.header().session_id.clone(),
            session.header().created_ms + 1,
            Some(fork),
            Bytes::new(),
        );
        drop(session);
        let mut child = Journal::resumed(dir.path(), header, 1).expect("resumed child");
        let next = Message::user("next");
        child.message(&next).expect("append");
        child.commit().expect("commit");
        child.close().expect("close");

        let resumed = load_now(dir.path());
        assert_eq!(
            resumed.history,
            [kept, next],
            "the history from the base on, once"
        );
        assert_eq!(resumed.state.compaction, 1);
    }
}

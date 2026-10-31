//! The permission check before each tool call.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reagent_ai::Text;
use reagent_config::permissions::{Call, Kind, PermissionMode, Reason, Rules};
use reagent_util::json::string_arg;
use reagent_util::path::normalize;
use reagent_util::text::head;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::Event;
use crate::live::ClientId;
use crate::tool::{Effects, Tool, ToolOutput};

#[path = "approval.rs"]
pub mod approval;

pub use approval::{Answer, ApprovalId, Approver};

use approval::{Approvals, Waited, save_rules};

/// The longest call summary an ask shows, in bytes.
const CALL_BYTES: usize = 200;

/// Decides each tool call by the permission rules before it runs.
///
/// A live session's guard asks its clients about a call the rules send to a
/// person; a headless run has no one to ask, so it refuses the call.
#[derive(Debug, Clone)]
pub struct Guard {
    rules: Rules,
    /// How a call the rules send to a person is treated.
    mode: PermissionMode,
    /// Whether the session may switch to [`PermissionMode::Bypass`].
    bypass_enabled: bool,
    /// The events of the live session that listens, when one does.
    events: Option<mpsc::Sender<Event>>,
    /// The calls that wait for an answer. Every clone shares the list and its
    /// ids, so an answer reaches the call whichever clone asked.
    approvals: Arc<Approvals>,
}

impl Guard {
    /// A guard over `rules`, for a run with no one to ask.
    pub fn new(rules: Rules) -> Self {
        Self {
            rules,
            mode: PermissionMode::default(),
            bypass_enabled: false,
            events: None,
            approvals: Arc::default(),
        }
    }

    /// The rules the guard decides with.
    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// The mode the guard decides with.
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Whether the session may switch to [`PermissionMode::Bypass`].
    pub fn bypass_enabled(&self) -> bool {
        self.bypass_enabled
    }

    /// Lets the session switch to [`PermissionMode::Bypass`], which the start
    /// of a run decides and no client can turn on.
    #[must_use]
    pub fn with_bypass(mut self, enabled: bool) -> Self {
        self.bypass_enabled = enabled;
        self
    }

    /// Switches the mode. [`PermissionMode::Bypass`] is refused when the run
    /// did not enable it, and the mode stays as it was.
    pub fn set_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
        if mode == PermissionMode::Bypass && !self.bypass_enabled {
            return Err(format!(
                "the mode `bypass` is not enabled for this run; the mode stays {}",
                self.mode
            ));
        }
        self.mode = mode;
        Ok(())
    }

    /// Raises the guard to the clients of a live session: an ask becomes an
    /// [`Event::ApprovalRequest`] they answer.
    pub fn listen(&mut self, events: mpsc::Sender<Event>) {
        self.events = Some(events);
    }

    /// Lowers the guard again, so the next run refuses instead of waiting for
    /// an answer no client can send.
    pub fn hush(&mut self) {
        self.events = None;
    }

    /// The approver of a listening session, if one listens.
    fn approver(&self) -> Option<Approver> {
        let events = self.events.clone()?;
        Some(Approver::new(events, Arc::clone(&self.approvals)))
    }

    /// What the rules say about a call: run it, refuse it with the result the
    /// model reads, run it contained, or ask a client.
    pub fn check(&self, tool: &dyn Tool, name: &str, args: &str) -> Verdict {
        if self.rules.is_open() {
            return Verdict::Run;
        }
        let main_arg = tool.main_arg();
        let value = main_arg.and_then(|key| string_arg(args, key));
        let edits = self.mode == PermissionMode::AcceptEdits && is_editor(name);
        // Only Read and Edit rules look at paths, and an edit of an edit mode.
        let effects = if self.rules.needs_effects() || edits {
            tool.effects(args)
        } else {
            Effects::default()
        };
        let decision = self.rules.decide(&Call {
            tool: name,
            main_arg,
            value: value.as_deref(),
            reads: &effects.reads,
            writes: &effects.writes,
        });
        // A containable call is one the sandbox can hold on its own: a command,
        // not an edit of a file of this machine.
        let contains = main_arg == Some("command") && tool.can_contain();
        match (decision.kind, decision.reason) {
            // A rule for a shell, an interpreter or a wrapper names a program
            // that runs other text, so in auto a call with any such part runs
            // contained instead of freely. A bare tool rule trusts every call.
            (Kind::Allow, reason @ Reason::Rule { .. })
                if self.mode == PermissionMode::Auto
                    && contains
                    && reason_rule(&reason).is_some_and(|rule| rule.contains('('))
                    && value.as_deref().is_some_and(approval::names_a_wrapper) =>
            {
                Verdict::Contain(Ask::new(name, main_arg, value.as_deref(), args, reason))
            }
            (Kind::Allow, _) => Verdict::Run,
            (Kind::Deny, Reason::Rule { rule, file }) => Verdict::Refuse(ToolOutput::error(
                format!("denied by rule {rule} in {file}; the call did not run"),
            )),
            (Kind::Deny, Reason::Default) => Verdict::Refuse(ToolOutput::error(
                "denied: no rule allows this call (default deny); the call did not run",
            )),
            (Kind::Ask, _) if self.mode == PermissionMode::Bypass => Verdict::Run,
            (Kind::Ask, _) if edits && inside(self.rules.cwd(), &effects.writes) => Verdict::Run,
            (Kind::Ask, reason) => {
                let ask = Ask::new(name, main_arg, value.as_deref(), args, reason);
                if self.mode == PermissionMode::Auto && contains {
                    Verdict::Contain(ask)
                } else {
                    Verdict::Ask(ask)
                }
            }
        }
    }

    /// Waits for a client's answer to `ask`.
    ///
    /// `None` when the run was cancelled while the call waited: the call does
    /// not run and the turn ends as cancelled. With no listening session the
    /// call is refused at once, as a headless run has always refused it.
    pub async fn approve(&self, ask: &Ask, token: &CancellationToken) -> Option<Grant> {
        let Some(approver) = self.approver() else {
            return Some(Grant::Refuse(ask.refusal()));
        };
        match approver.ask(ask, token).await {
            Waited::Answered(answer) => Some(self.apply(ask, answer, token).await),
            Waited::Cancelled => None,
            Waited::Silent => Some(Grant::Refuse(ToolOutput::error(
                "the approval request reached no client; the call did not run",
            ))),
        }
    }

    /// What one answer means for the call it was given for.
    async fn apply(&self, ask: &Ask, answer: Answer, token: &CancellationToken) -> Grant {
        match answer {
            Answer::Once => Grant::Run,
            Answer::Session => {
                self.allow(ask);
                Grant::Run
            }
            Answer::Save => {
                self.allow(ask);
                if let Some(approver) = self.approver() {
                    for rule in ask.rules() {
                        approver.saving(rule, token).await;
                    }
                }
                Grant::Run
            }
            Answer::Refuse { reason } => Grant::Refuse(ToolOutput::error(
                reason.unwrap_or_else(|| Text::from("the user refused this call")),
            )),
            Answer::Skip => Grant::Refuse(ToolOutput::error(
                "the user skipped this call; continue without it",
            )),
        }
    }

    /// Adds the rules the client approved to the session's own allow list.
    fn allow(&self, ask: &Ask) {
        for rule in ask.rules() {
            self.rules.add_session_allow(rule);
        }
    }

    /// Sends a client's answer to the call that waits for `id`. Returns
    /// whether a call waited: a second answer to the same id finds none.
    pub(crate) async fn answer(
        &self,
        client: ClientId,
        id: ApprovalId,
        answer: Answer,
        token: &CancellationToken,
    ) -> bool {
        match self.approver() {
            Some(approver) => approver.answer(client, id, answer, token).await,
            None => false,
        }
    }
}

/// What the guard says about one call.
#[derive(Debug)]
pub enum Verdict {
    /// The call runs as it does without the guard.
    Run,
    /// The call does not run; the model gets this result instead.
    Refuse(ToolOutput),
    /// A client decides: the call runs on its answer.
    Ask(Ask),
    /// The call runs contained in the sandbox, and a client decides when the
    /// sandbox cannot start. `Ask` holds what such a request shows.
    Contain(Ask),
}

/// What the answer to an ask came to.
#[derive(Debug)]
pub enum Grant {
    /// The call runs as it does without the guard.
    Run,
    /// The call does not run; the model gets this result instead.
    Refuse(ToolOutput),
}

/// What one ask shows a client about the call it would run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The tool the call names.
    pub tool: Text,
    /// One line that shows the call.
    pub call: Text,
    /// The rule that asked, or `default` when no rule matched.
    pub rule: Text,
    /// The file of the rule that asked; `None` for the default.
    pub file: Option<Text>,
    /// The rule a save would write, when a save is possible.
    pub save: Option<Text>,
    /// The rules a session or save answer adds to the session's allow list.
    rules: Box<[Text]>,
}

impl Ask {
    fn new(
        name: &str,
        main_arg: Option<&str>,
        value: Option<&str>,
        args: &str,
        reason: Reason,
    ) -> Self {
        let (rule, file) = match reason {
            Reason::Rule { rule, file } => (rule, Some(file)),
            Reason::Default => (Text::from("default"), None),
        };
        let rules = save_rules(name, main_arg, value).unwrap_or_default();
        Self {
            tool: Text::from(name),
            call: summary(name, value.unwrap_or(args)),
            rule,
            file,
            save: save_text(&rules),
            rules,
        }
    }

    /// The rules a session answer adds, and a save answer writes.
    pub fn rules(&self) -> &[Text] {
        &self.rules
    }

    /// The result of the call when no one can answer: today's refusal.
    fn refusal(&self) -> ToolOutput {
        ToolOutput::error(match &self.file {
            Some(file) => format!(
                "needs approval by rule {} in {file}, and this run cannot ask; the call did not run",
                self.rule
            ),
            None => "denied: no rule allows this call (default ask); the call did not run".into(),
        })
    }
}

/// The rule text of a reason, when a rule decided.
fn reason_rule(reason: &Reason) -> Option<&str> {
    match reason {
        Reason::Rule { rule, .. } => Some(rule),
        Reason::Default => None,
    }
}

/// Whether a tool name is one that edits files.
fn is_editor(name: &str) -> bool {
    name.eq_ignore_ascii_case("write") || name.eq_ignore_ascii_case("edit")
}

/// Whether every path of `writes` is inside `cwd`. A call with no known write
/// is not inside it, so such a call still asks.
fn inside(cwd: &Path, writes: &[PathBuf]) -> bool {
    !writes.is_empty()
        && writes
            .iter()
            .all(|write| normalize(&cwd.join(write)).starts_with(cwd))
}

/// One line that shows a call: the tool and the argument that decides it.
fn summary(name: &str, detail: &str) -> Text {
    let line = detail.lines().next().unwrap_or_default().trim();
    Text::from(format!("{name}: {}", head(line, CALL_BYTES)))
}

/// The rules of a save as one text, for a client that shows them.
fn save_text(rules: &[Text]) -> Option<Text> {
    match rules {
        [] => None,
        [one] => Some(one.clone()),
        many => {
            let mut joined = String::new();
            for (at, rule) in many.iter().enumerate() {
                if at > 0 {
                    joined.push_str(", ");
                }
                joined.push_str(rule);
            }
            Some(Text::from(joined))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use futures_util::future::BoxFuture;
    use reagent_ai::{Text, ToolCall, ToolSpec};
    use reagent_config::permissions::Lists;
    use reagent_config::{Layer, Settings};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tool::Registry;

    /// A bash-like tool that records each command it really runs.
    struct Shell {
        spec: ToolSpec,
        ran: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl Tool for Shell {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("command")
        }

        fn can_contain(&self) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            self.ran.lock().unwrap().push(args.to_owned());
            Box::pin(async { ToolOutput::ok("ran") })
        }
    }

    /// A write-like tool with a path effect.
    struct Writer {
        spec: ToolSpec,
    }

    impl Tool for Writer {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("path")
        }

        fn effects(&self, args: &str) -> Effects {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            let writes = value["path"]
                .as_str()
                .map(|path| vec![Path::new("/work").join(path)])
                .unwrap_or_default();
            Effects {
                reads: Vec::new(),
                writes,
            }
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("wrote") })
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "test".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn guard(allow: &[&str], ask: &[&str], deny: &[&str], default: Option<&str>) -> Guard {
        let lists = Lists {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            default: default.map(str::to_owned),
        };
        let layer = Layer {
            path: PathBuf::from("/c/config.toml"),
            settings: Settings {
                permissions: lists,
                ..Settings::default()
            },
        };
        Guard::new(Rules::from_layers(&[layer], Path::new("/work"), None).unwrap())
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: Text::from(id),
            name: Text::from(name),
            arguments: Text::from(args),
        }
    }

    fn result_text(message: &reagent_ai::Message) -> (String, bool) {
        match message {
            reagent_ai::Message::ToolResult(result) => {
                (result.content.to_string(), result.is_error)
            }
            _ => panic!("expected a tool result"),
        }
    }

    #[tokio::test]
    async fn a_denied_call_does_not_run_and_names_the_rule() {
        let ran = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.add(Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::clone(&ran),
        });
        let guard = guard(&["Bash(git:*)"], &[], &["Bash(git push:*)"], None);
        let calls = [
            call("1", "bash", r#"{"command":"cd app && git push"}"#),
            call("2", "bash", r#"{"command":"git status"}"#),
        ];
        let results = crate::batch::run_all_guarded(
            &calls,
            &registry,
            Some(&guard),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result_text(&results[0]),
            (
                "denied by rule Bash(git push:*) in /c/config.toml; the call did not run".into(),
                true
            )
        );
        assert_eq!(result_text(&results[1]), ("ran".into(), false));
        assert_eq!(ran.lock().unwrap().len(), 1, "only git status ran");
    }

    #[tokio::test]
    async fn an_ask_is_refused_in_a_headless_run() {
        let ran = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.add(Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::clone(&ran),
        });
        let guard = guard(&[], &["Bash(rm:*)"], &[], None);
        let results = crate::batch::run_all_guarded(
            &[call("1", "bash", r#"{"command":"rm -rf build"}"#)],
            &registry,
            Some(&guard),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result_text(&results[0]).0,
            "needs approval by rule Bash(rm:*) in /c/config.toml, and this run cannot ask; the call did not run"
        );
        assert!(ran.lock().unwrap().is_empty());
    }

    /// A guard in `mode`, over the given lists, bypass enabled or not.
    fn mode_guard(mode: PermissionMode, bypass: bool) -> Guard {
        let mut guard = guard(&[], &[], &[], Some("ask")).with_bypass(bypass);
        guard.set_mode(mode).expect("the mode is allowed here");
        guard
    }

    #[test]
    fn accept_edits_mode_runs_an_edit_inside_the_folder() {
        let guard = mode_guard(PermissionMode::AcceptEdits, false);
        let tool = Writer { spec: spec("edit") };
        assert!(matches!(
            guard.check(&tool, "edit", r#"{"path":"src/a.rs"}"#),
            Verdict::Run
        ));
        assert!(matches!(
            guard.check(&tool, "edit", r#"{"path":"./src/deep/b.rs"}"#),
            Verdict::Run
        ));
        // One path outside the folder asks, as an edit with no known path does.
        assert!(matches!(
            guard.check(&tool, "edit", r#"{"path":"../elsewhere/b.rs"}"#),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            guard.check(&tool, "edit", r#"{"path":"/etc/passwd"}"#),
            Verdict::Ask(_)
        ));
    }

    #[test]
    fn accept_edits_mode_still_asks_for_a_command() {
        let guard = mode_guard(PermissionMode::AcceptEdits, false);
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let Verdict::Ask(ask) = guard.check(&tool, "bash", r#"{"command":"cargo test"}"#) else {
            panic!("expected an ask");
        };
        assert_eq!(ask.rule.as_ref(), "default");
    }

    #[test]
    fn accept_edits_mode_keeps_a_deny_rule() {
        let mut guard = guard(&[], &[], &["Edit(secrets/**)"], Some("ask"));
        guard
            .set_mode(PermissionMode::AcceptEdits)
            .expect("accept-edits needs no flag");
        let tool = Writer { spec: spec("edit") };
        assert!(matches!(
            guard.check(&tool, "edit", r#"{"path":"secrets/key"}"#),
            Verdict::Refuse(_)
        ));
    }

    #[test]
    fn bypass_mode_runs_what_a_rule_sent_to_a_person_but_keeps_a_deny() {
        let mut guard = guard(
            &[],
            &["Bash(git push:*)"],
            &["Bash(git push --force:*)"],
            Some("ask"),
        )
        .with_bypass(true);
        guard
            .set_mode(PermissionMode::Bypass)
            .expect("bypass is enabled");
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        assert!(matches!(
            guard.check(&tool, "bash", r#"{"command":"cargo test"}"#),
            Verdict::Run
        ));
        assert!(matches!(
            guard.check(&tool, "bash", r#"{"command":"git push origin main"}"#),
            Verdict::Run
        ));
        let Verdict::Refuse(output) = guard.check(
            &tool,
            "bash",
            r#"{"command":"git push --force origin main"}"#,
        ) else {
            panic!("expected a refusal");
        };
        assert!(
            output
                .content
                .starts_with("denied by rule Bash(git push --force:*)"),
            "{}",
            output.content
        );
    }

    #[test]
    fn bypass_mode_is_refused_unless_it_is_enabled() {
        let mut guard = guard(&[], &[], &[], Some("ask"));
        assert!(!guard.bypass_enabled());
        assert_eq!(guard.mode(), PermissionMode::Default);
        let refused = guard
            .set_mode(PermissionMode::Bypass)
            .expect_err("not enabled");
        assert!(refused.contains("not enabled"), "{refused}");
        assert_eq!(guard.mode(), PermissionMode::Default, "the mode stays");
        // Every other mode needs no flag.
        for mode in [
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Default,
        ] {
            assert!(guard.set_mode(mode).is_ok(), "{mode}");
            assert_eq!(guard.mode(), mode);
        }
    }

    #[test]
    fn auto_mode_contains_a_command_an_ask_rule_asked_about() {
        let guard = mode_guard(PermissionMode::Auto, false);
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let Verdict::Contain(ask) =
            guard.check(&tool, "bash", r#"{"command":"rm -rf target/debug"}"#)
        else {
            panic!("expected a contained run");
        };
        assert_eq!(ask.call.as_ref(), "bash: rm -rf target/debug");
        // A tool that cannot be contained asks, as it does in the default mode.
        let writer = Writer {
            spec: spec("write"),
        };
        assert!(matches!(
            guard.check(&writer, "write", r#"{"path":"src/a.rs"}"#),
            Verdict::Ask(_)
        ));
    }

    #[test]
    fn auto_mode_contains_a_command_a_wrapper_rule_allows() {
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        // A rule for an interpreter or a wrapper runs the call contained,
        // rather than freely, because the rule allows its arguments too.
        let rules = Rules::from_layers(
            &[Layer {
                path: PathBuf::from("/c/config.toml"),
                settings: Settings {
                    permissions: Lists {
                        allow: vec!["Bash(python3:*)".into(), "Bash(cargo:*)".into()],
                        ..Lists::default()
                    },
                    ..Settings::default()
                },
            }],
            Path::new("/work"),
            None,
        )
        .unwrap();
        let mut guard = Guard::new(rules);
        // Outside auto the rule allows the call as it always has.
        assert!(matches!(
            guard.check(&tool, "bash", r#"{"command":"python3 x.py"}"#),
            Verdict::Run
        ));
        guard.set_mode(PermissionMode::Auto).unwrap();
        assert!(matches!(
            guard.check(&tool, "bash", r#"{"command":"python3 x.py"}"#),
            Verdict::Contain(_)
        ));
        // The wrapper part is contained wherever it stands in the command.
        for command in ["cargo build && python3 x.py", "python3 x.py && cargo build"] {
            let args = serde_json::json!({ "command": command }).to_string();
            assert!(
                matches!(guard.check(&tool, "bash", &args), Verdict::Contain(_)),
                "{command}"
            );
        }
        // A rule for an ordinary program still runs the call freely.
        assert!(matches!(
            guard.check(&tool, "bash", r#"{"command":"cargo test"}"#),
            Verdict::Run
        ));
    }

    #[test]
    fn an_edit_rule_stops_a_write_by_its_effect() {
        let guard = guard(&[], &[], &["Edit(secrets/**)"], None);
        let tool = Writer {
            spec: spec("write"),
        };
        let Verdict::Refuse(output) =
            guard.check(&tool, "write", r#"{"path":"secrets/key","content":"x"}"#)
        else {
            panic!("expected a refusal");
        };
        assert!(
            output
                .content
                .starts_with("denied by rule Edit(secrets/**)")
        );
        assert!(matches!(
            guard.check(&tool, "write", r#"{"path":"src/a.rs"}"#),
            Verdict::Run
        ));
    }

    #[test]
    fn the_default_names_itself() {
        let guard = guard(&[], &[], &[], Some("deny"));
        let tool = Writer {
            spec: spec("write"),
        };
        let Verdict::Refuse(output) = guard.check(&tool, "write", r#"{"path":"a"}"#) else {
            panic!("expected a refusal");
        };
        assert_eq!(
            output.content.as_ref(),
            "denied: no rule allows this call (default deny); the call did not run"
        );
    }

    #[test]
    fn open_rules_check_nothing() {
        let guard = Guard::new(Rules::none());
        let tool = Writer {
            spec: spec("write"),
        };
        assert!(matches!(
            guard.check(&tool, "write", "not json"),
            Verdict::Run
        ));
    }

    #[test]
    fn an_ask_shows_the_call_the_rule_and_the_rule_a_save_writes() {
        let guard = guard(&[], &["Bash(cargo test:*)"], &[], Some("deny"));
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let Verdict::Ask(ask) = guard.check(
            &tool,
            "bash",
            r#"{"command":"cargo test -p a\ncargo build --release"}"#,
        ) else {
            panic!("expected an ask");
        };
        assert_eq!(ask.tool.as_ref(), "bash");
        assert_eq!(ask.rule.as_ref(), "Bash(cargo test:*)");
        assert_eq!(ask.file.as_deref(), Some("/c/config.toml"));
        // The summary is the first line of the command, and a save offers one
        // rule for each of the two simple commands.
        assert_eq!(ask.call.as_ref(), "bash: cargo test -p a");
        assert_eq!(
            ask.save.as_deref(),
            Some("Bash(cargo test:*), Bash(cargo build:*)")
        );
    }

    #[test]
    fn an_ask_of_the_default_names_the_default() {
        let guard = guard(&[], &[], &[], Some("ask"));
        let tool = Shell {
            spec: spec("bash"),
            ran: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let Verdict::Ask(ask) = guard.check(&tool, "bash", r#"{"command":"ls"}"#) else {
            panic!("expected an ask");
        };
        assert_eq!(ask.rule.as_ref(), "default");
        assert_eq!(ask.file, None);
        assert_eq!(ask.save, None, "a bare program is not a rule");
    }
}

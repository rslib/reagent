//! The permission check before each tool call.

use reagent_config::permissions::{Call, Kind, Reason, Rules};
use reagent_util::json::string_arg;

use crate::tool::{Effects, Tool, ToolOutput};

/// Decides each tool call by the permission rules before it runs.
///
/// A headless run has no one to ask, so a call that needs approval is
/// refused.
#[derive(Debug, Clone)]
pub struct Guard {
    rules: Rules,
}

impl Guard {
    /// A guard over `rules`.
    pub fn new(rules: Rules) -> Self {
        Self { rules }
    }

    /// `None` when the call may run, else the error the model gets instead.
    pub fn check(&self, tool: &dyn Tool, name: &str, args: &str) -> Option<ToolOutput> {
        if self.rules.is_open() {
            return None;
        }
        let main_arg = tool.main_arg();
        let value = main_arg.and_then(|key| string_arg(args, key));
        // Only Read and Edit rules look at paths; skip the second parse otherwise.
        let effects = if self.rules.needs_effects() {
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
        let text = match (decision.kind, decision.reason) {
            (Kind::Allow, _) => return None,
            (Kind::Deny, Reason::Rule { rule, file }) => {
                format!("denied by rule {rule} in {file}; the call did not run")
            }
            (Kind::Ask, Reason::Rule { rule, file }) => format!(
                "needs approval by rule {rule} in {file}, and this run cannot ask; the call did not run"
            ),
            (Kind::Deny, Reason::Default) => {
                "denied: no rule allows this call (default deny); the call did not run".into()
            }
            (Kind::Ask, Reason::Default) => {
                "denied: no rule allows this call (default ask); the call did not run".into()
            }
        };
        Some(ToolOutput::error(text))
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

    #[test]
    fn an_edit_rule_stops_a_write_by_its_effect() {
        let guard = guard(&[], &[], &["Edit(secrets/**)"], None);
        let tool = Writer {
            spec: spec("write"),
        };
        let output = guard
            .check(&tool, "write", r#"{"path":"secrets/key","content":"x"}"#)
            .expect("denied");
        assert!(
            output
                .content
                .starts_with("denied by rule Edit(secrets/**)")
        );
        assert!(
            guard
                .check(&tool, "write", r#"{"path":"src/a.rs"}"#)
                .is_none()
        );
    }

    #[test]
    fn the_default_names_itself() {
        let guard = guard(&[], &[], &[], Some("deny"));
        let tool = Writer {
            spec: spec("write"),
        };
        let output = guard
            .check(&tool, "write", r#"{"path":"a"}"#)
            .expect("denied");
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
        assert!(guard.check(&tool, "write", "not json").is_none());
    }
}

//! The `Tool` trait and a named, sorted registry of tools.

use std::collections::BTreeMap;
use std::fmt;

use futures_util::future::BoxFuture;

use reagent_ai::{Text, ToolSpec};
use tokio_util::sync::CancellationToken;

/// The outcome of one tool call, sent back to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// The text the tool produced.
    pub content: Text,
    /// Whether the tool failed.
    pub is_error: bool,
}

impl ToolOutput {
    /// Builds a successful result.
    pub fn ok(content: impl Into<Text>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// Builds a failed result.
    pub fn error(content: impl Into<Text>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Parses a tool's JSON arguments with borrowed serde types.
///
/// `T` may borrow from `args` (for example a field of type `Cow<'a, str>`),
/// so the tool copies only the text it keeps. A bad argument becomes an
/// error [`ToolOutput`] for the model, not a panic.
pub fn parse_args<'a, T>(args: &'a str) -> Result<T, ToolOutput>
where
    T: serde::Deserialize<'a>,
{
    serde_json::from_str(args)
        .map_err(|error| ToolOutput::error(format!("invalid arguments: {error}")))
}

/// A named tool the model can call.
pub trait Tool: Send + Sync {
    /// The spec offered to the provider.
    fn spec(&self) -> &ToolSpec;

    /// Whether the tool only reads state, so a batch of calls may run together.
    fn read_only(&self) -> bool {
        false
    }

    /// Runs one call. `cancel` fires when the caller asks the loop to stop.
    fn run<'a>(&'a self, args: &'a str, cancel: &'a CancellationToken)
    -> BoxFuture<'a, ToolOutput>;
}

/// A set of tools offered to the model, ordered by name.
///
/// The tool list built from a registry is stable for a given set of tools
/// because the order comes from a sorted map, so the prompt-cache prefix of
/// a session is not disturbed by registry state.
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// Adds a tool, replacing any tool with the same name.
    pub fn add(&mut self, tool: impl Tool + 'static) {
        let name = tool.spec().name.clone();
        self.tools.insert(name, Box::new(tool));
    }

    /// The tool names in stable sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        self.tools.keys().map(String::as_str)
    }

    /// The tool specs in stable sorted order, for a provider request.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> + '_ {
        self.tools.values().map(|tool| tool.spec())
    }

    /// Looks up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    /// The number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registry")
            .field("tools", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    use serde::Deserialize;
    use serde_json::json;

    fn spec(name: &'static str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "A test tool.".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    /// Echoes its `message` argument.
    struct Echo {
        spec: ToolSpec,
    }

    #[derive(Deserialize)]
    struct EchoArgs<'a> {
        #[serde(borrow)]
        message: Option<Cow<'a, str>>,
    }

    impl Tool for Echo {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                match parse_args::<EchoArgs<'a>>(args) {
                    Ok(arguments) => {
                        ToolOutput::ok(arguments.message.map(Cow::into_owned).unwrap_or_default())
                    }
                    Err(output) => output,
                }
            })
        }
    }

    fn echo() -> Echo {
        Echo { spec: spec("echo") }
    }

    /// A named tool that never blocks; used to poke the registry.
    struct Named {
        spec: ToolSpec,
        read_only: bool,
    }

    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            self.read_only
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("") })
        }
    }

    fn registry_of(names: &[&'static str]) -> Registry {
        let mut registry = Registry::new();
        for name in names {
            registry.add(Named {
                spec: spec(name),
                read_only: false,
            });
        }
        registry
    }

    #[test]
    fn names_are_sorted_regardless_of_insertion_order() {
        let registry = registry_of(&["zeta", "alpha", "mid"]);
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            vec!["alpha", "mid", "zeta"]
        );
    }

    #[test]
    fn specs_follow_the_same_sorted_order() {
        let registry = registry_of(&["b", "a"]);
        assert_eq!(
            registry
                .specs()
                .map(|spec: &ToolSpec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn adding_a_duplicate_name_replaces() {
        let mut registry = registry_of(&["a", "b"]);
        registry.add(Named {
            spec: spec("a"),
            read_only: true,
        });
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get("a").map(|tool| tool.read_only()), Some(true));
    }

    #[test]
    fn get_returns_the_named_tool_and_none_otherwise() {
        let registry = registry_of(&["read", "write"]);
        assert!(registry.get("read").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn read_only_defaults_to_false() {
        struct WriteOnly {
            spec: ToolSpec,
        }

        impl Tool for WriteOnly {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }

            fn run<'a>(
                &'a self,
                _args: &'a str,
                _cancel: &'a CancellationToken,
            ) -> BoxFuture<'a, ToolOutput> {
                Box::pin(async { ToolOutput::ok("") })
            }
        }

        let tool = WriteOnly {
            spec: spec("write"),
        };
        assert!(!tool.read_only());
        assert!(echo().read_only());
    }

    #[tokio::test]
    async fn run_echoes_borrowed_arguments() {
        let output = echo()
            .run(r#"{"message":"hello"}"#, &CancellationToken::new())
            .await;
        assert_eq!(output.content.as_ref(), "hello");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn run_without_arguments_returns_empty() {
        let output = echo().run("{}", &CancellationToken::new()).await;
        assert_eq!(output.content.as_ref(), "");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn bad_arguments_are_a_result_not_a_panic() {
        let output = echo().run("not json", &CancellationToken::new()).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("invalid arguments:"));
    }

    #[tokio::test]
    async fn a_tool_run_writes_once() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct WriteOnce {
            spec: ToolSpec,
            wrote: AtomicBool,
        }

        impl Tool for WriteOnce {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }

            fn run<'a>(
                &'a self,
                _args: &'a str,
                _cancel: &'a CancellationToken,
            ) -> BoxFuture<'a, ToolOutput> {
                Box::pin(async move {
                    let first = !self.wrote.swap(true, Ordering::SeqCst);
                    if first {
                        ToolOutput::ok("first")
                    } else {
                        ToolOutput::ok("again")
                    }
                })
            }
        }

        let tool = WriteOnce {
            spec: spec("w"),
            wrote: AtomicBool::new(false),
        };
        assert_eq!(
            tool.run("{}", &CancellationToken::new())
                .await
                .content
                .as_ref(),
            "first"
        );
    }
}

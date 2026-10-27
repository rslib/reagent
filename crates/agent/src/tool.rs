//! The `Tool` trait and a named, sorted registry of tools.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use futures_util::future::BoxFuture;

use reagent_ai::{Text, ToolSpec};
use tokio_util::sync::CancellationToken;

/// What one file is, so a read is matched with a later read of the same file
/// without looking at the path again.
///
/// On unix it is the device and inode out of the metadata of the handle a read
/// already opened: two integers, so no allocation and no extra syscall, the
/// same answer for every spelling of the path including a symlink, and a new
/// pair for a file that a write replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The device and inode of a file, on a platform that has them.
    Inode {
        /// The device the file system is mounted from.
        dev: u64,
        /// The file's inode number on that device.
        ino: u64,
    },
    /// The canonical path, where the metadata carries no inode.
    #[cfg(not(unix))]
    Canonical(PathBuf),
}

impl Identity {
    /// The identity of the file `metadata` describes, which `path` named.
    ///
    /// On unix this reads two integers out of the metadata, so it costs no
    /// syscall and no allocation. Elsewhere it canonicalizes the path.
    pub async fn new(metadata: &std::fs::Metadata, path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let _ = path;
            Self::Inode {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            match tokio::fs::canonicalize(path).await {
                Ok(canonical) => Self::Canonical(canonical),
                Err(_) => Self::Canonical(path.to_path_buf()),
            }
        }
    }
}

/// The lines of one file a read showed the model, with the file's state when
/// it ran, so the loop can tell a re-read from a fresh one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The path as the model wrote it, for the note.
    pub path: Text,
    /// The file's identity, for matching; see [`Identity`], which costs no
    /// allocation and no path lookup.
    pub key: Identity,
    /// The first line shown, 1-based.
    pub start: u64,
    /// The last line shown, 1-based, after the byte cap and the line limit.
    pub end: u64,
    /// The file's size when it was read.
    pub size: u64,
    /// The file's modified time when it was read.
    pub modified: Option<SystemTime>,
    /// Whether a shown line was cut at the line cap.
    pub cut: bool,
}

/// The outcome of one tool call, sent back to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// The text the tool produced.
    pub content: Text,
    /// Whether the tool failed.
    pub is_error: bool,
    /// The file lines a `read` showed, for the re-read note; `None` otherwise.
    pub seen: Option<Box<Seen>>,
    /// The object a script gets in place of the text; `None` for a text result.
    pub object: Option<Box<Object>>,
}

/// What a script gets in place of a result's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// The fields of the object.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// When set, the object also gets `output`: the content without its last this many bytes.
    pub tail: Option<usize>,
    /// When set, `output` keeps only the lines that start with a line number
    /// and a tab, without that prefix; any other line is a note and is left out.
    pub numbered: bool,
}

impl Object {
    /// An object with `fields` and no `output`.
    pub fn new(fields: serde_json::Map<String, serde_json::Value>) -> Self {
        Self {
            fields,
            tail: None,
            numbered: false,
        }
    }

    /// Adds `output`: the content without its last `tail` bytes.
    #[must_use]
    pub fn with_tail(mut self, tail: usize) -> Self {
        self.tail = Some(tail);
        self
    }

    /// Makes `output` the numbered lines of the content, without the numbers.
    #[must_use]
    pub fn numbered(mut self) -> Self {
        self.numbered = true;
        self
    }
}

impl ToolOutput {
    /// Builds a successful result.
    pub fn ok(content: impl Into<Text>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            seen: None,
            object: None,
        }
    }

    /// Builds a failed result.
    pub fn error(content: impl Into<Text>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            seen: None,
            object: None,
        }
    }

    /// Records the lines this result showed, so the loop can note a re-read.
    #[must_use]
    pub fn with_seen(mut self, seen: Seen) -> Self {
        // Boxed, so a result without one stays small as it moves through the
        // batch and the loop; only a read pays the one allocation.
        self.seen = Some(Box::new(seen));
        self
    }

    /// Makes a script read this result as `object` instead of as its text.
    #[must_use]
    pub fn with_object(mut self, object: Object) -> Self {
        // Boxed like `seen`, so the result grows by one pointer.
        self.object = Some(Box::new(object));
        self
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
    serde_json::from_str(args).map_err(|error| {
        // Arguments that end early are a reply cut short, most often a long
        // file write; saying so gets a smaller call instead of the same one.
        if error.is_eof() && !args.trim().is_empty() {
            return ToolOutput::error(format!(
                "invalid arguments: they were cut off at byte {} before the JSON ended; send a shorter call, and split a long file write into parts",
                args.len()
            ));
        }
        ToolOutput::error(format!("invalid arguments: {error}"))
    })
}

/// The files a tool call reads and writes.
///
/// Paths are joined to the tool's working folder and not yet normalized.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Effects {
    /// The files the call reads.
    pub reads: Vec<PathBuf>,
    /// The files the call writes.
    pub writes: Vec<PathBuf>,
}

/// How far a tool call's effect reaches, from a read to the outside world.
///
/// The journal writes a class before a call that is not [`EffectClass::Read`]
/// runs, so a resumed run reruns a read and reports anything else as
/// interrupted. Ordering is from the least to the most effect, so `max` of a
/// group of tools is the strictest class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectClass {
    /// Reads state and changes nothing.
    Read,
    /// Changes this machine inside a snapshot's reach: an edit, a write, a
    /// sandboxed shell.
    LocalReversible,
    /// Changes this machine outside a snapshot's reach.
    LocalIrreversible,
    /// Changes something outside this machine: the network, an MCP server.
    External,
}

/// A named tool the model can call.
pub trait Tool: Send + Sync {
    /// The spec offered to the provider.
    fn spec(&self) -> &ToolSpec;

    /// Whether the tool only reads state, so a batch of calls may run together.
    fn read_only(&self) -> bool {
        false
    }

    /// How far this tool's call reaches, for the journal's crash rule.
    ///
    /// Default: [`EffectClass::Read`] for a read-only tool, else
    /// [`EffectClass::LocalReversible`].
    fn effect(&self) -> EffectClass {
        if self.read_only() {
            EffectClass::Read
        } else {
            EffectClass::LocalReversible
        }
    }

    /// The return shape a script sees, as its signature line shows it.
    fn returns(&self) -> &'static str {
        "string"
    }

    /// The JSON schema of what the tool returns, when it names one.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    /// The argument a permission rule's pattern matches, such as `command` or `path`; `None` when no pattern applies.
    fn main_arg(&self) -> Option<&'static str> {
        None
    }

    /// The files a call with these arguments reads and writes, as paths joined to the tool's working folder (not yet normalized). Default: none.
    fn effects(&self, _args: &str) -> Effects {
        Effects::default()
    }

    /// Arguments to run in place of a call's own when the call names its file
    /// in a way the tool can fix, such as an alias or a mistyped path. The
    /// batch checks the rules against what this returns. `None`, the default
    /// and the answer for a well-formed call, costs nothing.
    fn repair<'a>(&'a self, _args: &'a str) -> Option<BoxFuture<'a, Repaired>> {
        None
    }

    /// Another tool's call that does the same work, such as a `read` for a
    /// bash `cat`: the batch runs it instead when the rules allow both calls,
    /// and runs this call when it fails. `None`, the default, costs nothing.
    fn reroute<'a>(&'a self, _args: &'a str) -> Option<BoxFuture<'a, Option<Reroute>>> {
        None
    }

    /// Runs one call. `cancel` fires when the caller asks the loop to stop.
    fn run<'a>(&'a self, args: &'a str, cancel: &'a CancellationToken)
    -> BoxFuture<'a, ToolOutput>;
}

/// What [`Tool::repair`] made of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repaired {
    /// The call runs as sent.
    Keep,
    /// The call runs with `args`. With a note its result starts with
    /// `[repaired: note]`; a change of shape alone is silent.
    Args { args: String, note: Option<String> },
    /// The call fails with this text and does not run.
    Refuse(String),
}

/// A call to run in place of another; see [`Tool::reroute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reroute {
    /// The tool to call.
    pub tool: &'static str,
    /// Its JSON arguments.
    pub args: String,
    /// The words the result starts with, as `[label: lines A-B of PATH]`.
    pub label: &'static str,
}

/// A set of tools offered to the model, ordered by name.
///
/// The tool list built from a registry is stable for a given set of tools
/// because the order comes from a sorted map, so the prompt-cache prefix of
/// a session is not disturbed by registry state. The specs are kept in one
/// sorted slice beside the map, so a request hands the provider a borrow
/// instead of rebuilding a list.
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    specs: Vec<ToolSpec>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
            specs: Vec::new(),
        }
    }

    /// Adds a tool, replacing any tool with the same name.
    pub fn add(&mut self, tool: impl Tool + 'static) {
        let name = tool.spec().name.clone();
        self.tools.insert(name, Box::new(tool));
        self.rebuild_specs();
    }

    /// Rebuilds the sorted spec slice after a tool was added or replaced.
    fn rebuild_specs(&mut self) {
        self.specs.clear();
        self.specs
            .extend(self.tools.values().map(|tool| tool.spec().clone()));
    }

    /// The tool names in stable sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        self.tools.keys().map(String::as_str)
    }

    /// The tool specs in stable sorted order, for a provider request.
    ///
    /// The slice is rebuilt by [`Registry::add`], so a caller borrows it
    /// without copying a schema.
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Looks up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    /// Looks up the tool a model's call means: the exact name, else the one
    /// name equal after cutting a leaked tag such as `</arg_key>`, trimming,
    /// dropping a leading `functions.` and ignoring ASCII case, else the same
    /// cleanup applied to the text after the last `>`, which is where a
    /// leaked `<tool_call>` tag ends. Open models and their hosts send such
    /// names.
    pub fn resolve(&self, name: &str) -> Option<&dyn Tool> {
        if let Some(tool) = self.get(name) {
            return Some(tool);
        }
        let trimmed = name.split('<').next().unwrap_or(name).trim();
        self.resolve_cleaned(trimmed)
            .or_else(|| self.resolve_after_tag(name))
    }

    /// Looks up the one tool whose name equals `wanted` after dropping a
    /// leading `functions.`, ignoring ASCII case. Two names are no answer.
    fn resolve_cleaned(&self, wanted: &str) -> Option<&dyn Tool> {
        let wanted = wanted.strip_prefix("functions.").unwrap_or(wanted);
        let mut found = self
            .tools
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(wanted));
        match (found.next(), found.next()) {
            (Some((_, tool)), None) => Some(tool.as_ref()),
            _ => None,
        }
    }

    /// Looks up the tool a call ends with: the cleanup of
    /// [`Registry::resolve`] applied to the text after the last `>`, when the
    /// text before it holds a `<` and so ends a tag such as `<tool_call>`. An
    /// empty text is no answer, and neither is a shell redirect, as in
    /// `echo hi > edit`, so a command that leaked into the name stays
    /// unresolved. Builds no `String`.
    fn resolve_after_tag(&self, name: &str) -> Option<&dyn Tool> {
        let (before, after) = name.rsplit_once('>')?;
        if !before.contains('<') {
            return None;
        }
        let after = after.trim();
        if after.is_empty() {
            return None;
        }
        self.resolve_cleaned(after)
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
    fn specs_are_one_sorted_slice() {
        let registry = registry_of(&["b", "a"]);
        let specs: &[ToolSpec] = registry.specs();
        assert_eq!(
            specs
                .iter()
                .map(|spec| spec.name.as_str())
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

    #[test]
    fn default_main_arg_and_effects_are_empty() {
        let tool = Named {
            spec: spec("n"),
            read_only: false,
        };
        assert_eq!(tool.main_arg(), None);
        assert_eq!(tool.effects("{}"), Effects::default());
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

    #[test]
    fn broken_call_a_tag_after_a_real_name_resolves() {
        let registry = registry_of(&["bash", "read"]);
        assert!(registry.resolve("read</arg_key>").is_some());
        assert!(registry.resolve("found_missing_output</arg_key>").is_none());
        assert!(registry.resolve("grep -n x f.py</arg_value>").is_none());
    }

    #[test]
    fn resolve_junk_a_name_after_a_tag_resolves() {
        let registry = registry_of(&["edit", "read"]);
        let tool = registry
            .resolve("读出 And fix the parse_line function entirely:<tool_call>edit")
            .unwrap();
        assert_eq!(tool.spec().name, "edit");
    }

    #[test]
    fn resolve_junk_a_shell_command_after_a_tag_stays_unresolved() {
        let registry = registry_of(&["bash", "grep"]);
        assert!(registry.resolve("grep -n x f.py</arg_value>").is_none());
    }

    #[test]
    fn resolve_junk_a_shell_redirect_before_a_name_stays_unresolved() {
        let registry = registry_of(&["bash", "edit"]);
        assert!(registry.resolve("echo hi > edit").is_none());
    }

    #[test]
    fn resolve_junk_two_tool_words_are_no_answer() {
        let registry = registry_of(&["edit", "read"]);
        assert!(registry.resolve("read then edit").is_none());
    }

    #[test]
    fn resolve_junk_a_name_after_a_tag_in_other_case_resolves() {
        let registry = registry_of(&["edit"]);
        let tool = registry.resolve("x:<tool_call>functions.EDIT ").unwrap();
        assert_eq!(tool.spec().name, "edit");
    }

    #[test]
    fn resolve_junk_whole_words_only() {
        let registry = registry_of(&["edit"]);
        assert!(registry.resolve("<tool_call>editor").is_none());
    }

    #[test]
    fn broken_call_cut_off_arguments_say_so() {
        let error = parse_args::<serde_json::Value>("{\"command\":\"cat << EOF > a.py\\nprint(1)")
            .unwrap_err();
        assert!(error.is_error);
        assert!(error.content.contains("cut off"), "{}", error.content);
        assert!(error.content.contains("shorter call"), "{}", error.content);
        let empty = parse_args::<serde_json::Value>("").unwrap_err();
        assert!(!empty.content.contains("cut off"), "{}", empty.content);
    }
}

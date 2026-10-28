//! Runs a reply's tool calls, in parallel when every call is read-only.

use std::borrow::Cow;

use futures_util::future::BoxFuture;

use reagent_ai::{Message, ToolCall};
use reagent_util::text;

use crate::coerce;
use crate::guard::Guard;
use crate::tool::{Registry, Repaired, Reroute, Seen, Tool, ToolOutput};
use tokio_util::sync::CancellationToken;

/// One call's result: the message the reply records, and the lines a read
/// showed for the loop's re-read note.
pub type Outcome = (Message, Option<Box<Seen>>);

/// Runs every tool call in `calls` and returns its result in call order.
///
/// When every call names a read-only tool, the calls run at the same time;
/// when any call writes, they run in reply order. Results line up with `calls`
/// either way. On cancel, each call that already finished keeps its real
/// output; only the calls that had not finished are answered with the
/// cancellation message, so a reply never leaves a call without a result.
pub async fn run_all<'a>(
    calls: &'a [ToolCall],
    registry: &'a Registry,
    token: &'a CancellationToken,
) -> Vec<Message> {
    run_all_guarded(calls, registry, None, token).await
}

/// [`run_all`] with each call first checked by `guard`; a refused call gets
/// the guard's error as its result and does not run.
pub async fn run_all_guarded<'a>(
    calls: &'a [ToolCall],
    registry: &'a Registry,
    guard: Option<&'a Guard>,
    token: &'a CancellationToken,
) -> Vec<Message> {
    let outcomes = run_all_guarded_seen(calls, registry, guard, token).await;
    outcomes.into_iter().map(|(message, _)| message).collect()
}

/// [`run_all_guarded`], keeping each result's `seen` next to its message so
/// the loop can note a re-read before the message is recorded.
pub async fn run_all_guarded_seen<'a>(
    calls: &'a [ToolCall],
    registry: &'a Registry,
    guard: Option<&'a Guard>,
    token: &'a CancellationToken,
) -> Vec<Outcome> {
    if token.is_cancelled() {
        return calls.iter().map(cancelled_message).collect();
    }
    // An unknown tool is not read-only, so it forces reply order.
    let parallel = calls
        .iter()
        .all(|call| registry.resolve(&call.name).is_some_and(Tool::read_only));

    let outputs: Vec<Option<ToolOutput>> = if let [call] = calls {
        vec![run_one(registry, guard, call, token).await]
    } else if parallel {
        let futures: Vec<BoxFuture<'a, Option<ToolOutput>>> = calls
            .iter()
            .map(|call| run_one(registry, guard, call, token))
            .collect();
        futures_util::future::join_all(futures).await
    } else {
        let mut outputs = Vec::with_capacity(calls.len());
        for call in calls {
            if token.is_cancelled() {
                break;
            }
            outputs.push(run_one(registry, guard, call, token).await);
        }
        while outputs.len() < calls.len() {
            outputs.push(None);
        }
        outputs
    };

    calls
        .iter()
        .zip(outputs)
        .map(|(call, output)| match output {
            Some(output) => result_message(call, output),
            None => cancelled_message(call),
        })
        .collect()
}

/// Runs one call as a direct call does: the name and argument repair, the
/// rules, the reroute, the type coercion and the run. `None` when the token
/// fires.
pub fn run_one<'a>(
    registry: &'a Registry,
    guard: Option<&'a Guard>,
    call: &'a ToolCall,
    token: &'a CancellationToken,
) -> BoxFuture<'a, Option<ToolOutput>> {
    match registry.resolve(&call.name) {
        Some(tool) => Box::pin(async move {
            // The note of the name the model sent, when it is not the tool's
            // own and the registry had to read it as that tool's.
            let name = tool.spec().name.as_str();
            let read_as = (&*call.name != name)
                .then(|| format!("name `{}` read as `{name}`", text::head(&call.name, 60)));
            let mut repaired = None;
            if let Some(pending) = tool.repair(call.arguments.as_ref()) {
                let outcome = tokio::select! {
                    outcome = pending => outcome,
                    () = token.cancelled() => return None,
                };
                match outcome {
                    Repaired::Keep => {}
                    Repaired::Args { args, note } => repaired = Some((args, note)),
                    Repaired::Refuse(message) => return Some(ToolOutput::error(message)),
                }
            }
            // The rules, the reroute and the run all see the repaired call.
            let args = repaired
                .as_ref()
                .map_or(call.arguments.as_ref(), |(args, _)| args);
            // The note of the name, then the note of the tool's own repair.
            let note = joined_notes(
                read_as.as_deref(),
                repaired.as_ref().and_then(|(_, note)| note.as_deref()),
            );
            // The rules see the tool's own name, not the model's spelling of
            // it: `bash</arg_key>` must meet the `Bash(...)` rules.
            if let Some(refused) =
                guard.and_then(|guard| guard.check(tool, tool.spec().name.as_str(), args))
            {
                return Some(refused);
            }
            if let Some(pending) = tool.reroute(args) {
                let reroute = tokio::select! {
                    reroute = pending => reroute,
                    () = token.cancelled() => return None,
                };
                if let Some(reroute) = reroute
                    && let Rerouted::Done(output) =
                        run_reroute(registry, guard, &reroute, token).await
                {
                    return output.map(|output| with_note(output, note.as_deref()));
                }
            }
            let output = tokio::select! {
                output = tool.run(args, token) => output,
                () = token.cancelled() => return None,
            };
            // A call whose values are of another type than its arguments
            // declare is what a tool reports as `invalid arguments`: run it
            // once more with the types its schema declares.
            if output.is_error
                && output.content.starts_with("invalid arguments")
                && let Some(converted) = coerce::by_schema(args, &tool.spec().input_schema)
            {
                // The rules see the arguments the second run would use.
                if let Some(refused) =
                    guard.and_then(|guard| guard.check(tool, tool.spec().name.as_str(), &converted))
                {
                    return Some(refused);
                }
                let retried = tokio::select! {
                    retried = tool.run(&converted, token) => retried,
                    () = token.cancelled() => return None,
                };
                let note = converted_note(note.as_deref());
                return Some(with_note(retried, Some(&note)));
            }
            Some(with_note(output, note.as_deref()))
        }),
        None => {
            let error = unknown_tool(registry, &call.name);
            Box::pin(async move { Some(ToolOutput::error(error)) })
        }
    }
}

/// The error for a name no tool has. Shell text in the name, which open
/// models leak from their own tool format, gets the bash call it meant.
fn unknown_tool(registry: &Registry, name: &str) -> String {
    let text = name.split('<').next().unwrap_or(name).trim();
    if text.contains(char::is_whitespace) && registry.get("bash").is_some() {
        let call = serde_json::json!({ "command": text });
        return format!(
            "`{name}` is not a tool name; to run it as a shell command, call `bash` with {call}"
        );
    }
    let names = registry.names().collect::<Vec<_>>().join(", ");
    format!("unknown tool `{name}`; the tools are: {names}")
}

/// The note of a name read as another tool's, then the note of the tool's own
/// repair: the result carries both in one line.
fn joined_notes<'a>(name: Option<&'a str>, note: Option<&'a str>) -> Option<Cow<'a, str>> {
    match (name, note) {
        (Some(name), Some(note)) => Some(Cow::Owned(format!("{name}; {note}"))),
        (Some(name), None) => Some(Cow::Borrowed(name)),
        (None, Some(note)) => Some(Cow::Borrowed(note)),
        (None, None) => None,
    }
}

/// `output` after a first line that says how the call was repaired, when it
/// was repaired with a note.
fn with_note(mut output: ToolOutput, note: Option<&str>) -> ToolOutput {
    let Some(note) = note else {
        return output;
    };
    const FRAME: usize = "[repaired: ]\n".len();
    let mut content = String::with_capacity(FRAME + note.len() + output.content.len());
    content.push_str("[repaired: ");
    content.push_str(note);
    content.push_str("]\n");
    content.push_str(&output.content);
    output.content = content.into();
    output
}

/// The note of a call that ran again with its values converted to the types
/// of its schema, after any note its arguments were repaired with.
fn converted_note(note: Option<&str>) -> String {
    const CONVERTED: &str = "values converted to the types in the tool schema";
    match note {
        Some(note) => format!("{note}; {CONVERTED}"),
        None => CONVERTED.to_owned(),
    }
}

/// What a rerouted call came to.
enum Rerouted {
    /// Its output, or `None` when the run was cancelled.
    Done(Option<ToolOutput>),
    /// It could not serve the call, so the original tool runs.
    Fallback,
}

/// Runs `reroute` when its tool exists and the rules allow it. A result
/// that is not an error, names the lines it showed and cut none of them is
/// kept, after a line that says which call served it. Anything else falls
/// back, a refusal too: the original call was allowed, and routing must not
/// take away what ran before.
async fn run_reroute(
    registry: &Registry,
    guard: Option<&Guard>,
    reroute: &Reroute,
    token: &CancellationToken,
) -> Rerouted {
    let Some(target) = registry.resolve(reroute.tool) else {
        return Rerouted::Fallback;
    };
    if guard.is_some_and(|guard| guard.check(target, reroute.tool, &reroute.args).is_some()) {
        return Rerouted::Fallback;
    }
    let mut output = tokio::select! {
        output = target.run(&reroute.args, token) => output,
        () = token.cancelled() => return Rerouted::Done(None),
    };
    let Some(seen) = output
        .seen
        .as_deref()
        .filter(|seen| !output.is_error && !seen.cut)
    else {
        return Rerouted::Fallback;
    };
    let mut content = format!(
        "[{}: lines {}-{} of {}]\n",
        reroute.label, seen.start, seen.end, seen.path
    );
    content.push_str(&output.content);
    output.content = content.into();
    Rerouted::Done(Some(output))
}

fn result_message(call: &ToolCall, output: ToolOutput) -> Outcome {
    // The message takes the content and the images; `seen` moves out next to it.
    let message = Message::tool_result_with_images(
        call.id.clone(),
        output.content,
        output.is_error,
        output.images.map(|images| *images).unwrap_or_default(),
    );
    (message, output.seen)
}

fn cancelled_message(call: &ToolCall) -> Outcome {
    (
        Message::tool_result(call.id.clone(), "cancelled", true),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use reagent_ai::{Text, ToolSpec};

    use crate::tool::Tool;

    /// A tool that optionally sleeps, records its completion to `log` and
    /// writes its own name back.
    struct TestTool {
        spec: ToolSpec,
        name: String,
        read_only: bool,
        sleep_ms: u64,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Tool for TestTool {
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
            let name = self.name.clone();
            let log = Arc::clone(&self.log);
            let sleep_ms = self.sleep_ms;
            Box::pin(async move {
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                }
                log.lock().expect("log lock").push(name.clone());
                ToolOutput::ok(name)
            })
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: Text::from("{}"),
        }
    }

    fn add_tool(
        registry: &mut Registry,
        name: &str,
        read_only: bool,
        sleep_ms: u64,
        log: &Arc<Mutex<Vec<String>>>,
    ) {
        registry.add(TestTool {
            spec: ToolSpec {
                name: name.into(),
                description: "A batch test tool.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            name: name.into(),
            read_only,
            sleep_ms,
            log: Arc::clone(log),
        });
    }

    fn content_of(message: &Message) -> String {
        let Message::ToolResult(result) = message else {
            panic!("expected a tool result");
        };
        result.content.to_string()
    }

    #[tokio::test]
    async fn a_near_miss_name_runs_the_tool() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "bash", false, 0, &log);
        add_tool(&mut registry, "read", true, 0, &log);
        let calls = vec![call("c1", "Bash"), call("c2", " functions.read ")];

        let results = run_all(&calls, &registry, &CancellationToken::new()).await;

        assert_eq!(
            content_of(&results[0]),
            "[repaired: name `Bash` read as `bash`]\nbash"
        );
        assert_eq!(
            content_of(&results[1]),
            "[repaired: name ` functions.read ` read as `read`]\nread"
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_error_lists_the_tools() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "bash", false, 0, &log);
        add_tool(&mut registry, "read", true, 0, &log);

        let results = run_all(&[call("c1", "shell")], &registry, &CancellationToken::new()).await;

        let Message::ToolResult(result) = &results[0] else {
            panic!("expected a tool result");
        };
        assert!(result.is_error);
        assert_eq!(
            result.content.as_ref(),
            "unknown tool `shell`; the tools are: bash, read"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_only_calls_finish_under_the_sum_of_sleeps() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 100, &log);
        add_tool(&mut registry, "ro_b", true, 100, &log);
        let calls = vec![call("c1", "ro_a"), call("c2", "ro_b")];

        let started = tokio::time::Instant::now();
        let results = run_all(&calls, &registry, &CancellationToken::new()).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(180),
            "two 100ms read-only calls should overlap, took {elapsed:?}"
        );
        assert_eq!(results.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn results_keep_call_order_when_parallel() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 100, &log);
        add_tool(&mut registry, "ro_b", true, 0, &log);
        let calls = vec![call("c1", "ro_a"), call("c2", "ro_b")];

        let results = run_all(&calls, &registry, &CancellationToken::new()).await;

        // ro_b completes first, but results still line up with call order.
        assert_eq!(content_of(&results[0]), "ro_a");
        assert_eq!(content_of(&results[1]), "ro_b");
        // Both ran at the same time: the slow one finished while the log still
        // shows only two entries in parallel rather than a longer chain.
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_call_runs_every_call_in_reply_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 0, &log);
        add_tool(&mut registry, "write_b", false, 50, &log);
        let calls = vec![call("c1", "ro_a"), call("c2", "write_b")];

        run_all(&calls, &registry, &CancellationToken::new()).await;

        // The writer forces reply order: the fast read-only call still waits.
        assert_eq!(
            log.lock().unwrap().clone(),
            vec!["ro_a".to_string(), "write_b".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_call_runs_first_when_first_in_the_reply() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "write_b", false, 50, &log);
        add_tool(&mut registry, "ro_a", true, 0, &log);
        let calls = vec![call("c1", "write_b"), call("c2", "ro_a")];

        run_all(&calls, &registry, &CancellationToken::new()).await;

        assert_eq!(
            log.lock().unwrap().clone(),
            vec!["write_b".to_string(), "ro_a".to_string()]
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_still_returns_in_call_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 0, &log);
        let calls = vec![call("c1", "ro_a"), call("c2", "nope")];

        let results = run_all(&calls, &registry, &CancellationToken::new()).await;

        assert_eq!(content_of(&results[0]), "ro_a");
        assert_eq!(results.len(), 2);
        let Message::ToolResult(second) = &results[1] else {
            panic!("expected a tool result");
        };
        assert!(second.is_error);
    }

    /// A read tool that reports the line range it showed.
    struct SeenTool {
        spec: ToolSpec,
        seen: Seen,
    }

    impl Tool for SeenTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("1\ta\n").with_seen(self.seen.clone()) })
        }
    }

    #[tokio::test]
    async fn a_read_result_keeps_its_seen_lines_beside_the_message() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 0, &log);
        registry.add(SeenTool {
            spec: ToolSpec {
                name: "read".into(),
                description: "A batch test read.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            seen: Seen {
                path: Text::from("f.txt"),
                key: crate::tool::Identity::Inode { dev: 1, ino: 2 },
                start: 1,
                end: 1,
                size: 4,
                modified: None,
                cut: false,
            },
        });
        let seen_call = ToolCall {
            id: "c2".into(),
            name: "read".into(),
            arguments: Text::from("{}"),
        };
        let calls = vec![call("c1", "ro_a"), seen_call, call("c3", "shell")];

        let outcomes =
            run_all_guarded_seen(&calls, &registry, None, &CancellationToken::new()).await;

        assert_eq!(outcomes.len(), 3);
        assert_eq!(content_of(&outcomes[0].0), "ro_a");
        assert_eq!(outcomes[0].1, None, "a call with no seen keeps none");
        assert_eq!(content_of(&outcomes[1].0), "1\ta\n");
        let seen = outcomes[1].1.as_ref().expect("the read keeps its seen");
        assert_eq!((seen.start, seen.end), (1, 1));
        assert_eq!(seen.path.as_ref(), "f.txt");
        assert_eq!(outcomes[2].1, None, "an unknown tool has no seen");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_stops_running_tools_within_time() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "ro_a", true, 10_000, &log);
        add_tool(&mut registry, "ro_b", true, 10_000, &log);
        let calls = vec![call("c1", "ro_a"), call("c2", "ro_b")];
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move { run_all(&calls, &registry, &token).await });
        // Let the calls start sleeping before cancelling.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        cancellation.cancel();
        let started = tokio::time::Instant::now();
        let results = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the batch stops when cancelled")
            .expect("runner finishes");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "cancel took {elapsed:?}"
        );
        assert_eq!(results.len(), 2);
        for message in &results {
            let Message::ToolResult(result) = message else {
                panic!("expected a tool result");
            };
            assert!(result.is_error);
            assert_eq!(result.content.as_ref(), "cancelled");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_answers_every_pending_call_in_a_writing_batch() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "write_a", false, 10_000, &log);
        add_tool(&mut registry, "write_b", false, 10_000, &log);
        let calls = vec![call("c1", "write_a"), call("c2", "write_b")];
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move { run_all(&calls, &registry, &token).await });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        cancellation.cancel();
        let results = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the batch stops when cancelled")
            .expect("runner finishes");

        assert_eq!(results.len(), 2);
        for message in &results {
            let Message::ToolResult(result) = message else {
                panic!("expected a tool result");
            };
            assert!(result.is_error);
            assert_eq!(result.content.as_ref(), "cancelled");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_keeps_a_finished_call_output() {
        // A writing batch runs in reply order: write_a first, then write_b.
        // write_a is fast; write_b is still sleeping when the token fires, so
        // write_b is "cancelled" while write_a keeps its real output.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "write_a", false, 10, &log);
        add_tool(&mut registry, "write_b", false, 10_000, &log);
        let calls = vec![call("c1", "write_a"), call("c2", "write_b")];
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move { run_all(&calls, &registry, &token).await });
        // Let write_a finish, then write_b start sleeping.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        cancellation.cancel();
        let results = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the batch stops when cancelled")
            .expect("runner finishes");

        assert_eq!(results.len(), 2);
        assert_eq!(
            content_of(&results[0]),
            "write_a",
            "write_a keeps its output"
        );
        assert_eq!(
            content_of(&results[1]),
            "cancelled",
            "write_b was cancelled"
        );
        let Message::ToolResult(first) = &results[0] else {
            panic!("expected a tool result");
        };
        assert!(!first.is_error, "write_a finished before the cancel");
        let Message::ToolResult(second) = &results[1] else {
            panic!("expected a tool result");
        };
        assert!(second.is_error, "write_b was cancelled");
    }

    /// A bash stand-in whose `cat X` calls run as a read of `X`.
    struct Shell {
        spec: ToolSpec,
        ran: Arc<Mutex<Vec<String>>>,
    }

    impl Tool for Shell {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("command")
        }

        fn reroute<'a>(&'a self, args: &'a str) -> Option<BoxFuture<'a, Option<Reroute>>> {
            let command: serde_json::Value = serde_json::from_str(args).ok()?;
            let path = command["command"]
                .as_str()?
                .strip_prefix("cat ")?
                .to_string();
            Some(Box::pin(async move {
                Some(Reroute {
                    tool: "read",
                    args: serde_json::json!({ "path": path }).to_string(),
                    label: "bash ran this as read",
                })
            }))
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            self.ran.lock().unwrap().push(args.to_owned());
            Box::pin(async { ToolOutput::ok("from bash") })
        }
    }

    /// A read stand-in: `missing.txt` fails, `empty.txt` shows no lines,
    /// `long.txt` cuts a line, and any other path shows lines 3-4.
    struct Reader {
        spec: ToolSpec,
    }

    impl Tool for Reader {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("path")
        }

        fn effects(&self, args: &str) -> crate::tool::Effects {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            crate::tool::Effects {
                reads: value["path"]
                    .as_str()
                    .map(|path| vec![std::path::Path::new("/work").join(path)])
                    .unwrap_or_default(),
                ..crate::tool::Effects::default()
            }
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            let path = value["path"].as_str().unwrap_or_default().to_string();
            Box::pin(async move {
                match path.as_str() {
                    "missing.txt" => return ToolOutput::error("cannot open missing.txt"),
                    "empty.txt" => return ToolOutput::ok("empty.txt is empty"),
                    _ => {}
                }
                ToolOutput::ok("3\tc\n4\td\n").with_seen(Seen {
                    path: Text::from(path.as_str()),
                    key: crate::tool::Identity::Inode { dev: 1, ino: 2 },
                    start: 3,
                    end: 4,
                    size: 8,
                    modified: None,
                    cut: path == "long.txt",
                })
            })
        }
    }

    fn spec_named(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "A reroute test tool.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn shell_and_reader() -> (Registry, Arc<Mutex<Vec<String>>>) {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.add(Shell {
            spec: spec_named("bash"),
            ran: Arc::clone(&ran),
        });
        registry.add(Reader {
            spec: spec_named("read"),
        });
        (registry, ran)
    }

    fn bash_call(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: Text::from(serde_json::json!({ "command": command }).to_string()),
        }
    }

    #[tokio::test]
    async fn reroute_runs_the_read_with_the_note_and_keeps_seen() {
        let (registry, ran) = shell_and_reader();
        let results = run_all_guarded_seen(
            &[bash_call("cat a.rs")],
            &registry,
            None,
            &CancellationToken::new(),
        )
        .await;
        let (message, seen) = &results[0];
        assert_eq!(
            content_of(message),
            "[bash ran this as read: lines 3-4 of a.rs]\n3\tc\n4\td\n"
        );
        assert_eq!(seen.as_ref().map(|seen| seen.start), Some(3));
        assert!(ran.lock().unwrap().is_empty(), "bash did not run");
    }

    #[tokio::test]
    async fn reroute_falls_back_to_the_tool_when_the_read_fails() {
        let (registry, ran) = shell_and_reader();
        let results = run_all_guarded_seen(
            &[bash_call("cat missing.txt")],
            &registry,
            None,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(content_of(&results[0].0), "from bash");
        assert!(results[0].1.is_none());
        assert_eq!(ran.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reroute_a_refused_read_runs_bash_as_before() {
        use reagent_config::permissions::{Lists, Rules};
        let (registry, ran) = shell_and_reader();
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: Lists {
                    allow: vec!["Bash(cat:*)".into()],
                    default: Some("deny".into()),
                    ..Lists::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/work"), None).unwrap();
        let guard = Guard::new(rules);
        let results = run_all_guarded_seen(
            &[bash_call("cat a.rs")],
            &registry,
            Some(&guard),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(content_of(&results[0].0), "from bash");
        assert_eq!(ran.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reroute_a_read_with_no_lines_or_a_cut_line_runs_bash() {
        for path in ["empty.txt", "long.txt"] {
            let (registry, ran) = shell_and_reader();
            let call = bash_call(&format!("cat {path}"));
            let results =
                run_all_guarded_seen(&[call], &registry, None, &CancellationToken::new()).await;
            assert_eq!(content_of(&results[0].0), "from bash", "{path}");
            assert!(results[0].1.is_none(), "{path}");
            assert_eq!(ran.lock().unwrap().len(), 1, "{path}");
        }
    }

    #[tokio::test]
    async fn reroute_stops_when_the_run_is_cancelled() {
        let (registry, ran) = shell_and_reader();
        let token = CancellationToken::new();
        token.cancel();
        let results = run_all_guarded_seen(&[bash_call("cat a.rs")], &registry, None, &token).await;
        assert_eq!(content_of(&results[0].0), "cancelled");
        assert!(ran.lock().unwrap().is_empty());
    }

    /// An edit stand-in that repairs `file_path` to `path`, refuses a call
    /// with neither, and writes back the path it ran on.
    struct Fixer {
        spec: ToolSpec,
    }

    impl Tool for Fixer {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("path")
        }

        fn effects(&self, args: &str) -> crate::tool::Effects {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            crate::tool::Effects {
                writes: value["path"]
                    .as_str()
                    .map(|path| vec![std::path::Path::new("/work").join(path)])
                    .unwrap_or_default(),
                ..crate::tool::Effects::default()
            }
        }

        fn repair<'a>(&'a self, args: &'a str) -> Option<BoxFuture<'a, Repaired>> {
            let value: serde_json::Value = serde_json::from_str(args).ok()?;
            if value.get("path").is_some() {
                return None;
            }
            Some(Box::pin(async move {
                match value["file_path"].as_str() {
                    Some(path) => Repaired::Args {
                        args: serde_json::json!({ "path": path }).to_string(),
                        note: Some(format!("used {path}")),
                    },
                    None => Repaired::Refuse("path is missing".into()),
                }
            }))
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            let path = value["path"].as_str().unwrap_or("none").to_string();
            Box::pin(async move { ToolOutput::ok(format!("edited {path}")) })
        }
    }

    fn fixer_call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            arguments: Text::from(args.to_string()),
        }
    }

    fn fixer() -> Registry {
        let mut registry = Registry::new();
        registry.add(Fixer {
            spec: spec_named("edit"),
        });
        registry
    }

    #[tokio::test]
    async fn repair_runs_the_repaired_call_with_the_note() {
        let call = fixer_call(serde_json::json!({ "file_path": "a.rs" }));
        let results =
            run_all_guarded_seen(&[call], &fixer(), None, &CancellationToken::new()).await;
        assert_eq!(
            content_of(&results[0].0),
            "[repaired: used a.rs]\nedited a.rs"
        );
    }

    #[tokio::test]
    async fn repair_refusal_is_the_result() {
        let call = fixer_call(serde_json::json!({}));
        let results =
            run_all_guarded_seen(&[call], &fixer(), None, &CancellationToken::new()).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(result.is_error);
        assert_eq!(result.content.as_ref(), "path is missing");
    }

    #[tokio::test]
    async fn repair_the_rules_see_the_repaired_arguments() {
        use reagent_config::permissions::{Lists, Rules};
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: Lists {
                    deny: vec!["Edit(secrets/**)".into()],
                    default: Some("allow".into()),
                    ..Lists::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/work"), None).unwrap();
        let guard = Guard::new(rules);
        let call = fixer_call(serde_json::json!({ "file_path": "secrets/k.txt" }));
        let results =
            run_all_guarded_seen(&[call], &fixer(), Some(&guard), &CancellationToken::new()).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(result.is_error);
        assert!(
            result.content.contains("Edit(secrets/**)"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn repair_note_a_name_read_as_another_tool_says_so() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "bash", false, 0, &log);

        let results = run_all(
            &[call("c1", "functions.bash")],
            &registry,
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            content_of(&results[0]),
            "[repaired: name `functions.bash` read as `bash`]\nbash"
        );
    }

    #[tokio::test]
    async fn repair_note_an_exact_name_has_none() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "bash", false, 0, &log);

        let results = run_all(&[call("c1", "bash")], &registry, &CancellationToken::new()).await;

        assert_eq!(content_of(&results[0]), "bash");
    }

    #[tokio::test]
    async fn repair_note_a_name_note_joins_the_tools_own_note() {
        let mut call = fixer_call(serde_json::json!({ "file_path": "a.rs" }));
        call.name = Text::from("functions.edit");

        let results =
            run_all_guarded_seen(&[call], &fixer(), None, &CancellationToken::new()).await;

        assert_eq!(
            content_of(&results[0].0),
            "[repaired: name `functions.edit` read as `edit`; used a.rs]\nedited a.rs"
        );
    }

    /// A tool that reads `limit` through [`crate::tool::parse_args`] and
    /// records every run's arguments.
    struct Limited {
        spec: ToolSpec,
        ran: Arc<Mutex<Vec<String>>>,
    }

    #[derive(serde::Deserialize)]
    struct LimitArgs {
        limit: u32,
    }

    impl Tool for Limited {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            self.ran.lock().expect("ran lock").push(args.to_owned());
            Box::pin(async move {
                match crate::tool::parse_args::<LimitArgs>(args) {
                    Ok(arguments) => ToolOutput::ok(format!("limit {}", arguments.limit)),
                    Err(output) => output,
                }
            })
        }
    }

    fn limited() -> (Registry, Arc<Mutex<Vec<String>>>) {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.add(Limited {
            spec: ToolSpec {
                name: "limited".into(),
                description: "A batch test tool with a typed argument.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "limit": { "type": "integer" } },
                    "required": ["limit"]
                }),
            },
            ran: Arc::clone(&ran),
        });
        (registry, ran)
    }

    fn limited_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "limited".into(),
            arguments: Text::from(arguments.to_string()),
        }
    }

    #[tokio::test]
    async fn coerce_a_string_integer_runs_the_call_again() {
        let (registry, ran) = limited();
        let call = limited_call(serde_json::json!({ "limit": "5" }));

        let results =
            run_all_guarded_seen(&[call], &registry, None, &CancellationToken::new()).await;

        let content = content_of(&results[0].0);
        assert!(
            content.starts_with("[repaired: values converted"),
            "{content}"
        );
        assert_eq!(
            content,
            "[repaired: values converted to the types in the tool schema]\nlimit 5"
        );
        assert_eq!(ran.lock().expect("ran lock").len(), 2, "the tool ran twice");
    }

    #[tokio::test]
    async fn coerce_a_call_with_no_value_to_convert_keeps_its_error() {
        let (registry, ran) = limited();
        let call = limited_call(serde_json::json!({}));

        let results =
            run_all_guarded_seen(&[call], &registry, None, &CancellationToken::new()).await;

        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(result.is_error);
        assert!(
            result.content.starts_with("invalid arguments"),
            "{}",
            result.content
        );
        assert_eq!(ran.lock().expect("ran lock").len(), 1, "the tool ran once");
    }

    #[tokio::test]
    async fn broken_call_shell_text_as_a_name_names_the_bash_call() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        add_tool(&mut registry, "bash", false, 0, &log);
        add_tool(&mut registry, "read", true, 0, &log);
        let results = run_all(
            &[call("c1", "grep -n x f.py</arg_value>")],
            &registry,
            &CancellationToken::new(),
        )
        .await;
        let text = content_of(&results[0]);
        assert!(
            text.contains(r#"call `bash` with {"command":"grep -n x f.py"}"#),
            "{text}"
        );
        let other = run_all(
            &[call("c1", "found_missing_output</arg_key>")],
            &registry,
            &CancellationToken::new(),
        )
        .await;
        let text = content_of(&other[0]);
        assert!(text.contains("the tools are: bash, read"), "{text}");
    }

    #[tokio::test]
    async fn broken_call_a_tagged_name_meets_the_rules_of_its_tool() {
        use reagent_config::permissions::{Lists, Rules};
        let (registry, ran) = shell_and_reader();
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: Lists {
                    deny: vec!["Bash(rm:*)".into()],
                    default: Some("allow".into()),
                    ..Lists::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/work"), None).unwrap();
        let guard = Guard::new(rules);
        let call = ToolCall {
            id: "c1".into(),
            name: "bash</arg_key>".into(),
            arguments: Text::from(r#"{"command":"rm -rf x"}"#),
        };
        let results =
            run_all_guarded_seen(&[call], &registry, Some(&guard), &CancellationToken::new()).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(result.is_error, "{}", result.content);
        assert!(ran.lock().unwrap().is_empty(), "bash did not run");
    }

    /// A tool that reads `name` as a string through [`crate::tool::parse_args`]
    /// and records every run's arguments.
    struct Named {
        spec: ToolSpec,
        ran: Arc<Mutex<Vec<String>>>,
    }

    #[derive(serde::Deserialize)]
    struct NameArg<'a> {
        #[serde(borrow)]
        name: Cow<'a, str>,
    }

    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("name")
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            self.ran.lock().expect("ran lock").push(args.to_owned());
            Box::pin(async move {
                match crate::tool::parse_args::<NameArg>(args) {
                    Ok(arguments) => ToolOutput::ok(format!("named {}", arguments.name)),
                    Err(output) => output,
                }
            })
        }
    }

    fn named() -> (Registry, Arc<Mutex<Vec<String>>>) {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        registry.add(Named {
            spec: ToolSpec {
                name: "named".into(),
                description: "A batch test tool with a string argument.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
            },
            ran: Arc::clone(&ran),
        });
        (registry, ran)
    }

    #[tokio::test]
    async fn coerce_a_guard_that_refuses_the_converted_call_stops_it() {
        use reagent_config::permissions::{Lists, Rules};
        let (registry, ran) = named();
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: Lists {
                    deny: vec!["Named(5)".into()],
                    default: Some("allow".into()),
                    ..Lists::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/work"), None).unwrap();
        let guard = Guard::new(rules);
        // The rule reads the `name` value, which only the converted arguments
        // hold as a string: the call it was sent with passes the rules.
        let call = ToolCall {
            id: "c1".into(),
            name: "named".into(),
            arguments: Text::from(serde_json::json!({ "name": 5 }).to_string()),
        };

        let results =
            run_all_guarded_seen(&[call], &registry, Some(&guard), &CancellationToken::new()).await;

        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("denied by rule Named(5)"),
            "{}",
            result.content
        );
        assert_eq!(
            ran.lock().expect("ran lock").len(),
            1,
            "the tool ran once, before the conversion"
        );
    }
}

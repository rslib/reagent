//! Runs a reply's tool calls, in parallel when every call is read-only.

use futures_util::future::BoxFuture;

use reagent_ai::{Message, ToolCall};

use crate::guard::Guard;
use crate::tool::{Registry, Tool, ToolOutput};
use tokio_util::sync::CancellationToken;

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

/// Runs one call, giving up with `None` when the token fires.
fn run_one<'a>(
    registry: &'a Registry,
    guard: Option<&'a Guard>,
    call: &'a ToolCall,
    token: &'a CancellationToken,
) -> BoxFuture<'a, Option<ToolOutput>> {
    match registry.resolve(&call.name) {
        Some(tool) => Box::pin(async move {
            if let Some(refused) =
                guard.and_then(|guard| guard.check(tool, &call.name, call.arguments.as_ref()))
            {
                return Some(refused);
            }
            tokio::select! {
                output = tool.run(call.arguments.as_ref(), token) => Some(output),
                () = token.cancelled() => None,
            }
        }),
        None => {
            let names = registry.names().collect::<Vec<_>>().join(", ");
            let error = format!("unknown tool `{}`; the tools are: {names}", call.name);
            Box::pin(async move { Some(ToolOutput::error(error)) })
        }
    }
}

fn result_message(call: &ToolCall, output: ToolOutput) -> Message {
    Message::tool_result(call.id.clone(), output.content, output.is_error)
}

fn cancelled_message(call: &ToolCall) -> Message {
    Message::tool_result(call.id.clone(), "cancelled", true)
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

        assert_eq!(content_of(&results[0]), "bash");
        assert_eq!(content_of(&results[1]), "read");
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
}

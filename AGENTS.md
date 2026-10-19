# reagent

A minimal coding agent in Rust, in the style of pi. Cargo workspace, toolchain pinned in `rust-toolchain.toml`.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo nextest run -p reagent-agent -E 'test(/^session::/)'   # one module; fails when no test matches
cargo nextest run -p reagent --features testing   # cli end-to-end tests; skipped without the feature
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --profile min -p reagent             # size build for footprint checks
cargo run -q -p reagent -- --version
```

The build output goes to the global target folder `~/.cache/cargo-target`, not the repo folder.

## Priorities

1. Pass rate on Terminal-Bench 2.1.
2. Tokens per solved task.
3. Footprint: start, RSS, binary size.
4. Small API.

A feature that does not move 1 or 2 in a measured run does not ship as a default.

## Harness changes

- Ship a harness change only when it fixes a mechanism (a crash, lost work, a wrong final state, wasted tokens) or follows a practice that two or more outside sources measured. Never add prompt or tool text written for one benchmark task: tuning on failed tasks did not carry to held-out Terminal-Bench tasks.
- The system prompt and tool specs stay under `PROMPT_BUDGET` in `crates/cli/src/main.rs`. A new note in `prompts/` must pay for its tokens in a measured run.
- Measure on the dev set with 3 trials (`bench/harbor/run.sh --dev`), then report from `bench/harbor`: `uv run python -m reagent_harbor.report <job> --against <old job> --tasks dev-tasks.txt`. Give the pass rate with its 95% interval, tokens per solved task and the failure types.
- A dev gap under 10 points, or a difference interval that includes 0, is noise. Do not ship on it.
- Before you call a default change a gain, confirm it on the Terminal-Bench tasks not in `bench/harbor/dev-tasks.txt` and on a second model.

## Where things go

| Area | Crate | Contents |
|---|---|---|
| Message types, providers, SSE, `FakeProvider` | `crates/ai` (`reagent-ai`) | no other reagent crate |
| Loop, events, session, context policy, `Tool` trait | `crates/agent` (`reagent-agent`) | depends on ai |
| Native tools | `crates/tools` (`reagent-tools`) | depends on agent |
| QuickJS plugins and `execute(code)` | `crates/script` (`reagent-script`) | depends on agent |
| MCP client | `crates/mcp` (`reagent-mcp`) | depends on agent |
| Terminal UI and markdown | `crates/tui` (`reagent-tui`) | depends on agent |
| Binary and flags | `crates/cli` (`reagent`) | depends on all |
| Model-facing prompt templates | none: `prompts/` at the root | baked into `crates/agent` and `crates/cli` with `include_str!` |
| Harbor adapter | `bench/harbor` | Python, run with `uv` |

Links point down this table only. Do not add a crate, and do not add a `common` or `utils` crate. Shared types go in `crates/ai`. Update this section in the change that moves an area.

Design decisions and their reasons: `docs/plans/architecture.md` (local only, ignored by git).

## Performance rules

- Do not clone message text. Finished text is `Arc<str>`. Stream chunks are `bytes::Bytes`.
- Deserialize tool arguments and SSE events with borrowed serde (`&str`, `Cow<str>`). Do not go through `serde_json::Value` on a hot path.
- Serialize each finished message once and reuse the bytes in every later request.
- Never rewrite an earlier message in a request. It breaks the prompt cache.
- Never hold a whole session file or a whole tool output in RAM. Cap and spill to a file.
- All I/O is async: `tokio::fs`, `tokio::process`, `tokio::net`, `tokio::time`. `clippy.toml` bans the `std` versions, in tests too. Heavy CPU work goes in `tokio::task::spawn_blocking`. The runtime is `current_thread`.
- A new dependency needs `default-features = false` and only the features it uses. Add it with `cargo add -p <package> <dep> --no-default-features -F <features>`.
- Claim a speed or size gain only with a measured number and the command that produced it.

## Unsafe

`unsafe_code` is denied in the workspace. A crate that needs it allows it in one module, and each block has a `// SAFETY:` line.

## Tests

- Loop and provider tests use `FakeProvider` (feature `testing` of `reagent-ai`). No test calls a real API.
- A bug fix lands with a test that fails before the fix.
- Task checks use `cargo nextest run`, not `cargo test <filter>`. `cargo test` passes when the filter matches no test.
- Tools are tested on temp folders (`tempfile`), never on the repo.

## Boundaries

- Always: run clippy with `-D warnings` and the tests of each crate you touched.
- Ask first: a new crate, a new dependency over 500 KB in the `min` build, any change to the system prompt or tool descriptions (they are prompt-cache and pass-rate sensitive).
- Never: call a real LLM API from a test, print secrets from the environment, commit `docs/plans/`.

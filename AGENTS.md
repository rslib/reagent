# reagent

A minimal coding agent in Rust, in the style of pi. Cargo workspace, toolchain pinned in `rust-toolchain.toml`.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo nextest run -p reagent-agent -E 'test(/^session::/)'   # one module; fails when no test matches
cargo nextest run -p reagent --features testing --test e2e   # cli end-to-end tests; skipped without the feature
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --profile min -p reagent             # size build for footprint checks
cargo nextest run --release -p reagent-agent --test budget                    # loop heap and CPU budgets
cargo nextest run --release -p reagent --features testing --test footprint -E 'test(/^process::/)'   # process RSS and CPU budgets
cargo nextest run -p reagent-ai --features testing --test allocs   # heap blocks per stream event and body build
cargo nextest run -p reagent-tools --test allocs                   # heap blocks for edit, read, walk and effects
cargo nextest run -p reagent-agent --test allocs                   # heap blocks per journal record, stuck turn and delta
cargo nextest run -p reagent-tui --test allocs                     # TUI heap blocks per frame, streamed delta and worker line
cargo nextest run --release -p reagent --features testing --test footprint -E 'test(/^tui::/)'   # TUI start, idle RSS, key latency, idle wakeups, streaming CPU
bash bench/instructions.sh                                         # Linux: Callgrind instruction counts against the merge base
bash bench/syscalls.sh   # Linux: syscalls of fixed scenarios against bench/syscall-budget.txt
bash bench/size.sh   # size build against bench/size-budget.txt
bash bench/tmp-leftovers.sh   # the suites in an empty TMPDIR; fails on anything left but reagent-spill
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
| Small helpers with no domain types: text cuts, number writing, path normalizing, borrowed JSON reading | `crates/util` (`reagent-util`) | no other reagent crate |
| OS sandbox for one process and its children: Seatbelt on macOS, Landlock on Linux | `crates/sandbox` (`reagent-sandbox`) | no other reagent crate |
| Durable append-only session log: frames, fields, CRC, part writer and reader, recovery, fork pointers, zstd stream, session list. No message types | `crates/store` (`reagent-store`) | no other reagent crate |
| Client protocol: JSON-RPC message types, the line codec, the discovery folder and the join token. No agent types | `crates/protocol` (`reagent-protocol`) | no other reagent crate |
| Images for models: format sniffing, decoders, EXIF orientation, resize, PNG and JPEG encode, limits. No file system; AVIF only on macOS (ImageIO in `src/imageio.rs`, the crate's one `unsafe` module) | `crates/image` (`reagent-image`) | no other reagent crate |
| Config files, providers and models, permission rules and their shell lexer and glob | `crates/config` (`reagent-config`) | depends on util |
| Shared OAuth (PKCE, device flow, JWT, loopback callback) and the HTTP client | `crates/oauth` (`reagent-oauth`) | depends on config |
| Message types, providers, SSE, `FakeProvider`, OAuth logins, the message record codec of the journal (`record.rs`) | `crates/ai` (`reagent-ai`) | `reagent-config` (the auth file), oauth and store |
| Loop, events, session journal, context policy, `Tool` trait, the permission guard and its modes, live sessions (`live.rs`: steer, queue, cancel, settings) and client approvals (`approval.rs`) | `crates/agent` (`reagent-agent`) | depends on ai, config, store and util |
| Native tools; `read` returns images through `reagent-image` | `crates/tools` (`reagent-tools`) | depends on agent, ai, config, image and util |
| The `codemode` tool: a QuickJS script that calls the other tools | `crates/script` (`reagent-script`) | depends on agent, ai and tools |
| MCP client and its OAuth login | `crates/mcp` (`reagent-mcp`) | depends on agent, ai, oauth and config |
| Terminal UI: screen buffer and diff, full-screen transcript ring, streaming markdown, editor, layout, mouse, the slot API and the built-in panels, async tty input (`src/app/input`, the crate's one `unsafe` module), worker client. Never the agent loop | `crates/tui` (`reagent-tui`) | depends on protocol, store and ai |
| Binary and flags | `crates/cli` (`reagent`) | depends on all |
| Model-facing prompt templates | none: `prompts/` at the root | baked into `crates/agent` and `crates/cli` with `include_str!` |
| Harbor adapter | `bench/harbor` | Python, run with `uv` |

Links point down this table only. Do not add a crate without updating this table. `crates/util` holds only small helpers that two or more crates use and that carry no domain types; anything with a domain meaning goes in the crate that owns that domain, and shared message types go in `crates/ai`. Update this section in the change that moves an area.

Design decisions and their reasons: `docs/plans/architecture.md` (local only, ignored by git).

## Performance rules

- Do not clone message text. Finished text is `Arc<str>`. Stream chunks are `bytes::Bytes`.
- Deserialize tool arguments and SSE events with borrowed serde (`&str`, `Cow<str>`). Do not go through `serde_json::Value` on a hot path.
- Serialize each finished message once and reuse the bytes in every later request.
- Never rewrite an earlier message in a request. It breaks the prompt cache.
- Never hold a whole session file or a whole tool output in RAM. Cap and spill to a file.
- All I/O is async: `tokio::fs`, `tokio::process`, `tokio::net`, `tokio::time`. `clippy.toml` bans the `std` versions, in tests too. Heavy CPU work goes in `tokio::task::spawn_blocking`. The runtime is `current_thread`. The file work of one tool call runs in one `spawn_blocking` job (`crates/tools/src/blocking.rs`), and `std::fs` is allowed only inside such a job, with an `#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]` (or `disallowed_types` for `std::fs::File`). The store (`crates/store`) is the other exception: its reads run inside `spawn_blocking` and its writer runs on its own thread, so it uses `std::fs` with the same `#[expect]` and a reason.
- A new dependency needs `default-features = false` and only the features it uses. Add it with `cargo add -p <package> <dep> --no-default-features -F <features>`.
- Claim a speed or size gain only with a measured number and the command that produced it.
- A hot path runs per stream event, per request, per tool call or per line or file a tool touches. A change to one keeps its `allocs` test, `bench/instructions.sh` and `bench/syscalls.sh` passing (2% on instructions and estimated cycles, 12% for `find`, `grep` and `capture`, which touch the file system or a child process). A new hot path adds a dhat case that fails on the old code, with the bound set near the measured value. Raise a bound only with the old and new number in the commit body.
- Every test in a dhat program takes the program's shared lock (`SERIAL` in its `main.rs`) as its first line. dhat counts every thread, and `cargo test` runs tests in parallel threads, so another test's setup lands in the count.
- No per-event syscall: batch output writes, and read the clock only when the value is used.
- No stack array over 16 KB in an async function; put the buffer on the heap. Clippy's `large_futures`, `large_stack_arrays` and `large_enum_variant` fire on x86_64 Linux with sizes macOS does not show, so a change to an async function, a buffer or an enum runs clippy on Linux too.

## Unsafe

`unsafe_code` is denied in the workspace. A crate that needs it allows it in one module, and each block has a `// SAFETY:` line.

## Tests

- Loop and provider tests use `FakeProvider` (feature `testing` of `reagent-ai`). No test calls a real API.
- A bug fix lands with a test that fails before the fix.
- A new integration test goes in a module of the crate's existing test program (`tests/<program>/main.rs`), not in a new file at `tests/`: each file there links its own program, and macOS checks every new program before it runs, which slowed a full run by minutes. dhat tests go in the crate's `allocs` program.
- Task checks use `cargo nextest run`, not `cargo test <filter>`. `cargo test` passes when the filter matches no test.
- Tools are tested on temp folders (`tempfile`), never on the repo.
- A test that runs `reagent models` sets `REAGENT_MODELS_DEV_URL` to a closed port, or it fetches the real models.dev.
- CPU budgets are tight only in release builds, so a change to the loop, a provider or the output path runs the two budget commands with `--release`. Raise a budget only with the new measured number in the commit body.

## Boundaries

- Always: run clippy with `-D warnings` and the tests of each crate you touched.
- Ask first: a new crate, a new dependency over 500 KB in the `min` build, any change to the system prompt or tool descriptions (they are prompt-cache and pass-rate sensitive).
- Never: call a real LLM API from a test, print secrets from the environment, commit `docs/plans/`.

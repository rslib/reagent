# reagent Harbor adapter

This folder runs the `reagent` binary on Terminal-Bench datasets through Harbor.
The adapter is a Harbor installed agent named `reagent`.

It copies the statically linked `dist/reagent` binary into each task container
and runs it headless with `--json`.
Harbor installs it from `bench/harbor/reagent_harbor/agent.py:ReagentAgent`.

## What it does

- Runs the binary headless with your API key.
- Sends the task text to the binary on stdin, not on the command line.
  The text is never in the argv of any process.
- Reports token and turn counts back to Harbor.
  The final summary line decides the totals.
  Without it, the adapter sums the per-turn usage lines and counts the turn
  start lines.

## Setup

Build the binary first.

```bash
./bench/harbor/build.sh
```

The binary is written to `bench/harbor/dist/reagent`.
You need `cargo-zigbuild` and `zig`.
See the top of `build.sh`.

Run a dataset with `run.sh`.

```bash
./bench/harbor/run.sh anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3
```

Reach the same run directly with `run.sh --dry-run` to see the harbor command.
The API key comes from the environment.
Harbor reads `ANTHROPIC_API_KEY` and, when set, `ANTHROPIC_BASE_URL`.

## Gateways

The model prefix picks the wire, the endpoint and the API-key variable.

| Model prefix | Wire | Endpoint | Key variable |
|---|---|---|---|
| `anthropic/<model>` | `anthropic` | `ANTHROPIC_BASE_URL`, else `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `opencode-go/<model>` | `openai-chat` | `https://opencode.ai/inference/go/openai/v1` | `OPENCODE_API_KEY` |
| `commandcode/<model>` | `openai-chat` | `https://api.commandcode.ai/provider/v1` | `CMD_API_KEY` |

The model id is everything after the first slash, so `commandcode/deepseek/deepseek-v4-flash` sends `deepseek/deepseek-v4-flash` to reagent.
The endpoint follows the wire, so `--ak wire=anthropic` with `opencode-go/` uses `https://opencode.ai/inference/go/anthropic`.
An `opencode-go` trial sends one `x-opencode-session` header.

Export the key before you run.

```bash
export OPENCODE_API_KEY=...
./bench/harbor/run.sh opencode-go/kimi-k2.6 terminal-bench/terminal-bench-2-1 3

export CMD_API_KEY=...
./bench/harbor/run.sh commandcode/deepseek/deepseek-v4-flash terminal-bench/terminal-bench-2-1 3

export ANTHROPIC_API_KEY=...
./bench/harbor/run.sh anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3
```

Harbor has an `opencode` provider but no `opencode-go` and no `commandcode` entry.
`resolve_model_connection` takes the first path segment as the provider, finds no entry and resolves no API key, so `ModelConnectionSpec(passthrough=True)` does nothing for these prefixes.
Harbor does not reject the prefix and a local run has no credential check.
The adapter reads the key from the host environment itself, so the user passes no extra Harbor flag.

## Dev set

A full Terminal-Bench 2.1 run takes about 3 hours at 4 trials at a time.
For each harness change, run the dev set instead: the 26 tasks in `dev-tasks.txt`, 8 at a time, with a 10-minute agent cap.

```bash
./bench/harbor/run.sh --dev anthropic/claude-haiku-5-5 terminal-bench/terminal-bench-2-1 1
```

The tasks were picked from the 2026-10-18 Sonnet 5.5 and Haiku 5.5 runs: the tasks where the two models differ, a few both pass and a few both fail.
Extra harbor arguments come after the dev flags, so `--n-concurrent 4` overrides the 8.
Full runs stay the reference for a result you report.

## Report

`reagent_harbor.report` prints the pass rate with a 95% interval, tokens per solved task and the failure types of a job.
The interval resamples tasks, not trials, because trials of one task are correlated.
`--against` gives the paired difference with an older job on the shared tasks.

```bash
cd bench/harbor
uv run python -m reagent_harbor.report jobs/<new> --against jobs/<old> --tasks dev-tasks.txt
```

## Agent options

Harbor passes options to an installed agent as `--ak name=value`.

- `agent_timeout_sec`: an integer, at least 1.
  The task timeout in seconds.
  Harbor does not pass the timeout to the agent, so the adapter sets the
  binary's `--deadline` to this value minus a 30 second margin.
  Without it, Harbor hard-kills the binary and the token counts come from the
  per-turn usage lines.

  ```bash
  ./run.sh anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3 \
    --ak agent_timeout_sec=600
  ```

- `policy`: a string.
  The loop policies to enable, passed to the binary as `--policy <value>`.
  One value, for example `completion,budget`.
  Without it, the binary runs only `budget`. `none` turns every policy off.
  `mask` replaces old large tool outputs with a short stub; it needs the `openai-chat` wire.

  ```bash
  ./run.sh anthropic/claude-sonnet-5-5 terminal-bench/terminal-bench-2-1 3 \
    --ak policy=completion,budget
  ```

- `max_tokens`: an integer.
  The reply token cap, passed to the binary as `--max-tokens <value>`.
  The binary's default is 64000. Set a lower value for a gateway model whose output limit is lower.

  ```bash
  ./run.sh opencode-go/kimi-k2.6 terminal-bench/terminal-bench-2-1 3 \
    --ak max_tokens=32000
  ```

## Tests

Run the adapter tests without a container or an LLM call.

```bash
cd bench/harbor && uv run pytest -q tests
```

The tests run `build_command` in a real shell with a fake `reagent` that
records its arguments and its stdin.
They check that the task text arrives on stdin and that it is in none of the
argv.
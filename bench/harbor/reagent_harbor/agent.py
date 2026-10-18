"""A Harbor installed-agent adapter for the reagent binary.

``ReagentAgent`` copies the statically linked ``dist/reagent`` binary into the
task container, then runs it headless in ``--json`` mode. The adapter reads
reagent's JSON event stream and reports the token and turn counts back to
Harbor on :class:`AgentContext`.

The JSON parsing (``parse_events``/``UsageSummary``) is pure Python with no
Harbor import, so the unit tests exercise it against a recorded event file
without a container or an LLM call.
"""

from __future__ import annotations

import json
import shlex
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import override

from pydantic import Field

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import (
    BaseInstalledAgent,
    with_prompt_template,
)
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

# Where the binary is installed inside the container. The orchestrator creates
# ``/installed-agent`` before calling ``install`` (see ``BaseInstalledAgent``).
_AGENT_BINARY = "/installed-agent/reagent"
# Files written under the trial's agent log directory. Harbor syncs that
# directory to ``self.logs_dir`` on the host after ``run`` returns.
_EVENT_FILENAME = "reagent_events.jsonl"
_STDERR_FILENAME = "reagent_stderr.txt"
_EXIT_CODE_FILENAME = "reagent_exit_code.txt"
# Seconds to shave off the task timeout so reagent's own ``--deadline`` fires
# (and writes a summary line) before Harbor hard-kills the run.
_DEADLINE_MARGIN_SEC = 30
# The host-built binary from ``build.sh``.
DIST_BINARY = Path(__file__).resolve().parent.parent / "dist" / "reagent"

# The keys reagent's summary and per-event usage lines use. See
# ``crates/cli/src/json.rs`` in the reagent repository.
_SUMMARY = "summary"
_USAGE = "usage"
_TURN_START = "turn_start"


@dataclass(frozen=True)
class UsageSummary:
    """Token and turn totals extracted from a reagent JSON event stream."""

    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_write_tokens: int = 0
    turns: int = 0
    malformed_lines: int = 0

    @property
    def total_tokens(self) -> int:
        """All prompt-side tokens including the cache, as Harbor counts input."""
        return self.input_tokens + self.cache_read_tokens + self.cache_write_tokens


def _as_int(value: object) -> int:
    """A non-negative integer field, or zero when it is missing or malformed."""
    if isinstance(value, bool) or not isinstance(value, int):
        return 0
    return max(0, value)


def parse_events(output: str) -> UsageSummary:
    """Parse reagent's JSON event lines into a :class:`UsageSummary`.

    The final ``summary`` line is authoritative for the totals when present.
    Without one, the per-event ``usage`` lines are summed and the turn count
    comes from ``turn_start`` events. Any non-empty line that is not a JSON
    object, or is a JSON object without a recognized shape, is counted as
    malformed and skipped.
    """

    summary: dict[str, object] | None = None
    input_tokens = 0
    output_tokens = 0
    cache_read_tokens = 0
    cache_write_tokens = 0
    turns = 0
    malformed_lines = 0

    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line:
            continue

        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            malformed_lines += 1
            continue
        if not isinstance(event, dict) or not isinstance(event.get("type"), str):
            malformed_lines += 1
            continue

        event_type = event["type"]
        if event_type == _SUMMARY:
            summary = event
            continue
        if event_type == _USAGE:
            input_tokens += _as_int(event.get("input"))
            output_tokens += _as_int(event.get("output"))
            cache_read_tokens += _as_int(event.get("cache_read"))
            cache_write_tokens += _as_int(event.get("cache_write"))
            continue
        if event_type == _TURN_START:
            turns += 1

    if summary is not None:
        return UsageSummary(
            input_tokens=_as_int(summary.get("input")),
            output_tokens=_as_int(summary.get("output")),
            cache_read_tokens=_as_int(summary.get("cache_read")),
            cache_write_tokens=_as_int(summary.get("cache_write")),
            turns=_as_int(summary.get("turns")),
            malformed_lines=malformed_lines,
        )

    return UsageSummary(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        cache_read_tokens=cache_read_tokens,
        cache_write_tokens=cache_write_tokens,
        turns=turns,
        malformed_lines=malformed_lines,
    )


def build_command(
    *,
    workdir: str,
    model_id: str,
    instruction: str,
    logs_dir: PurePosixPath,
    timeout_sec: int | None,
) -> str:
    """The shell command that runs reagent inside the container.

    stdout (the JSON events) and stderr go to separate files. reagent's exit
    code is saved to a file and the command itself always exits 0: exit 1
    (max turns, error) and 124 (deadline) are normal agent outcomes that
    the verifier still scores, not adapter crashes.
    """
    deadline = ""
    if timeout_sec is not None:
        deadline = f"--deadline {max(1, timeout_sec - _DEADLINE_MARGIN_SEC)} "
    events = shlex.quote(str(logs_dir / _EVENT_FILENAME))
    stderr = shlex.quote(str(logs_dir / _STDERR_FILENAME))
    exit_code = shlex.quote(str(logs_dir / _EXIT_CODE_FILENAME))
    return (
        f"mkdir -p {shlex.quote(str(logs_dir))}; "
        f"{_AGENT_BINARY} --json --cwd {shlex.quote(workdir)} "
        f"--model {shlex.quote(model_id)} "
        f"{deadline}"
        f"-p {shlex.quote(instruction)} "
        f"</dev/null >{events} 2>{stderr}; "
        f"echo $? >{exit_code}; exit 0"
    )


class ReagentOptions(InstalledAgentOptions):
    """Agent options for the reagent binary."""

    agent_timeout_sec: int | None = Field(
        default=None,
        ge=1,
        description=(
            "Task timeout in seconds; Harbor does not pass it to agents. "
            "When set, reagent's --deadline is this minus a small margin. "
            "Without it Harbor hard-kills reagent, and the token counts come "
            "from the per-turn usage lines."
        ),
    )


class ReagentAgent(BaseInstalledAgent):
    """Runs the statically linked reagent binary as a Harbor installed agent."""

    capabilities = AgentCapabilities()
    options_model = ReagentOptions
    options: ReagentOptions

    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)

    @classmethod
    @override
    def name(cls) -> str:
        return "reagent"

    @override
    def get_version_command(self) -> str | None:
        return f"{_AGENT_BINARY} --version"

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        """Copy the host-built reagent binary into the container and make it runnable."""
        if not DIST_BINARY.is_file():
            raise FileNotFoundError(
                f"{DIST_BINARY} is missing; run bench/harbor/build.sh first"
            )
        await environment.upload_file(str(DIST_BINARY), _AGENT_BINARY)
        await self.exec_as_root(
            environment,
            command=f"chmod +x {shlex.quote(_AGENT_BINARY)}",
        )

    async def _resolve_workdir(self, environment: BaseEnvironment) -> str:
        """The task working directory for ``--cwd``.

        Uses the task environment's ``workdir`` when the task sets one,
        otherwise the container's current working directory.
        """
        configured = getattr(environment.task_env_config, "workdir", None)
        if configured and isinstance(configured, str) and configured.strip():
            return configured
        result = await self.exec_as_agent(environment, command="pwd -P")
        return result.stdout.strip() if result.stdout else "/"

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name:
            raise ValueError(
                "Model name must be set (e.g. anthropic/claude-sonnet-5-5)"
            )
        model_id = self._model_name().split("/", 1)[-1]

        # ``passthrough=True`` makes Harbor resolve the Anthropic provider env
        # with the original names, so ``access.env`` carries ANTHROPIC_API_KEY
        # and, when set, ANTHROPIC_BASE_URL. Fill any gap from the host env so
        # the adapter still works with --skip-credential-checks.
        env = dict(self.model_connection.env)
        if not env.get("ANTHROPIC_API_KEY"):
            api_key = self._get_env("ANTHROPIC_API_KEY")
            if api_key:
                env["ANTHROPIC_API_KEY"] = api_key
        if not env.get("ANTHROPIC_BASE_URL"):
            base_url = self._get_env("ANTHROPIC_BASE_URL", "ANTHROPIC_API_BASE")
            if base_url:
                env["ANTHROPIC_BASE_URL"] = base_url

        workdir = await self._resolve_workdir(environment)
        command = build_command(
            workdir=workdir,
            model_id=model_id,
            instruction=instruction,
            logs_dir=self.environment_logs_dir,
            timeout_sec=self.options.agent_timeout_sec,
        )
        await self.exec_as_agent(environment, command=command, env=env)

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        """Report token and turn counts from the synced event file."""
        populate_context(context, Path(self.logs_dir))

    def _model_name(self) -> str:
        return self.model_name or ""


def populate_context(context: AgentContext, logs_dir: Path) -> None:
    """Fill ``context`` from the event and exit-code files in ``logs_dir``.

    A missing event file leaves the context empty: the run never started.
    """
    try:
        output = (logs_dir / _EVENT_FILENAME).read_text(encoding="utf-8")
    except OSError:
        return
    try:
        exit_code: int | None = int(
            (logs_dir / _EXIT_CODE_FILENAME).read_text(encoding="utf-8").strip()
        )
    except (OSError, ValueError):
        exit_code = None

    summary = parse_events(output)
    context.n_input_tokens = summary.total_tokens
    context.n_cache_tokens = summary.cache_read_tokens
    context.n_output_tokens = summary.output_tokens
    context.metadata = {
        "cache_write_tokens": summary.cache_write_tokens,
        "turns": summary.turns,
        "malformed_event_lines": summary.malformed_lines,
        "exit_code": exit_code,
    }

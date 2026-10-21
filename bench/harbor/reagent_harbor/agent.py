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
import secrets
import shlex
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Literal, override

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

# Wire names shared with the reagent CLI's ``--provider`` flag.
Wire = Literal["anthropic", "openai-chat", "openai-responses"]
_WIRE_ANTHROPIC: Wire = "anthropic"
_WIRE_OPENAI_CHAT: Wire = "openai-chat"
_WIRE_OPENAI_RESPONSES: Wire = "openai-responses"
# Model prefixes the adapter maps to a wire, a base URL and a key variable.
_ANTHROPIC = "anthropic"
_OPENCODE_GO = "opencode-go"
_COMMANDCODE = "commandcode"
_ANTHROPIC_DEFAULT_BASE_URL = "https://api.anthropic.com"
_ANTHROPIC_BASE_URL_ENVS = ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_BASE")
_OPENCODE_BASE_URLS: dict[str, str] = {
    _WIRE_ANTHROPIC: "https://opencode.ai/inference/go/anthropic",
    _WIRE_OPENAI_CHAT: "https://opencode.ai/inference/go/openai/v1",
    _WIRE_OPENAI_RESPONSES: "https://opencode.ai/inference/go/openai/v1",
}
_COMMANDCODE_BASE_URLS: dict[str, str] = {
    _WIRE_ANTHROPIC: "https://api.commandcode.ai/provider",
    _WIRE_OPENAI_CHAT: "https://api.commandcode.ai/provider/v1",
    _WIRE_OPENAI_RESPONSES: "https://api.commandcode.ai/provider/v1",
}
# The opencode-go gateway wants one session id per trial.
_OPENCODE_SESSION_HEADER = "x-opencode-session"

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


def model_id(model_name: str) -> str:
    """The provider model id: everything after the first slash.

    ``commandcode/deepseek/deepseek-v4-flash`` becomes
    ``deepseek/deepseek-v4-flash``.
    """
    return model_name.split("/", 1)[-1]


@dataclass(frozen=True)
class ProviderDefaults:
    """The wire, endpoint and key variable a model runs with."""

    wire: str
    base_url: str
    api_key_env: str


def provider_defaults(
    model_name: str,
    *,
    wire: str | None = None,
    base_url: str | None = None,
    api_key_env: str | None = None,
    anthropic_base_url: str = _ANTHROPIC_DEFAULT_BASE_URL,
) -> ProviderDefaults:
    """Map a model prefix to its defaults, with each option overriding one.

    ``anthropic/`` uses the Anthropic wire and ``ANTHROPIC_BASE_URL`` (passed
    in as ``anthropic_base_url``), ``opencode-go/`` and ``commandcode/``
    default to Chat Completions and their gateway endpoint. The endpoint
    follows the chosen wire. A model with any other prefix needs all three
    options, so an unknown gateway never silently sends a key to the wrong
    host.
    """
    prefix = model_name.split("/", 1)[0]
    if prefix == _ANTHROPIC:
        default_wire = _WIRE_ANTHROPIC
        default_key_env = "ANTHROPIC_API_KEY"
        default_base_urls = None
    elif prefix == _OPENCODE_GO:
        default_wire = _WIRE_OPENAI_CHAT
        default_key_env = "OPENCODE_API_KEY"
        default_base_urls = _OPENCODE_BASE_URLS
    elif prefix == _COMMANDCODE:
        default_wire = _WIRE_OPENAI_CHAT
        default_key_env = "CMD_API_KEY"
        default_base_urls = _COMMANDCODE_BASE_URLS
    else:
        if wire is None or base_url is None or api_key_env is None:
            raise ValueError(
                f"model {model_name!r} has no known provider prefix; pass the "
                "agent options wire, base_url and api_key_env"
            )
        return ProviderDefaults(wire=wire, base_url=base_url, api_key_env=api_key_env)

    chosen_wire = wire or default_wire
    if base_url is None:
        if default_base_urls is None:
            base_url = anthropic_base_url
        else:
            base_url = default_base_urls.get(chosen_wire)
            if base_url is None:
                raise ValueError(
                    f"no default base URL for wire {chosen_wire!r} on prefix "
                    f"{prefix!r}; pass the base_url agent option"
                )
    return ProviderDefaults(
        wire=chosen_wire,
        base_url=base_url,
        api_key_env=api_key_env or default_key_env,
    )


def session_headers(model_name: str) -> list[tuple[str, str]]:
    """Extra request headers for a model: one session id for opencode-go."""
    if model_name.split("/", 1)[0] == _OPENCODE_GO:
        return [(_OPENCODE_SESSION_HEADER, secrets.token_hex(16))]
    return []


def build_command(
    *,
    workdir: str,
    model_id: str,
    task_env: str,
    logs_dir: PurePosixPath,
    timeout_sec: int | None,
    provider: str | None = None,
    base_url: str | None = None,
    api_key_env: str | None = None,
    headers: list[tuple[str, str]] | None = None,
    policy: str | None = None,
    max_tokens: int | None = None,
) -> str:
    """The shell command that runs reagent inside the container.

    The task text is never placed in any process's argv: it arrives in the
    `task_env` environment variable, and a `printf` builtin (which spawns no
    process) feeds it to reagent on stdin. The JSON events go to stdout and
    then to a file; stderr goes to its own file. reagent's exit code is saved
    to a file and the command itself always exits 0: exit 1 (max turns,
    error) and 124 (deadline) are normal agent outcomes that the verifier
    still scores, not adapter crashes.
    """
    deadline = ""
    if timeout_sec is not None:
        deadline = f"--deadline {max(1, timeout_sec - _DEADLINE_MARGIN_SEC)} "
    policy_flag = f"--policy {shlex.quote(policy)} " if policy else ""
    max_tokens_flag = f"--max-tokens {max_tokens} " if max_tokens is not None else ""
    provider_flags = ""
    if provider is not None:
        provider_flags += f"--provider {shlex.quote(provider)} "
    if base_url is not None:
        provider_flags += f"--base-url {shlex.quote(base_url)} "
    if api_key_env is not None:
        provider_flags += f"--api-key-env {shlex.quote(api_key_env)} "
    for name, value in headers or []:
        provider_flags += f"--header {shlex.quote(f'{name}:{value}')} "
    events = shlex.quote(str(logs_dir / _EVENT_FILENAME))
    stderr = shlex.quote(str(logs_dir / _STDERR_FILENAME))
    exit_code = shlex.quote(str(logs_dir / _EXIT_CODE_FILENAME))
    return (
        f"mkdir -p {shlex.quote(str(logs_dir))}; "
        f'v="${{{task_env}}}"; unset {task_env}; '
        f"printf '%s' \"$v\" | "
        f"{_AGENT_BINARY} --json --cwd {shlex.quote(workdir)} "
        f"--model {shlex.quote(model_id)} "
        f"{provider_flags}{policy_flag}{max_tokens_flag}{deadline}"
        f"-p - "
        f">{events} 2>{stderr}; "
        f"echo $? >{exit_code}; exit 0"
    )


def put_task_in_env(env: dict[str, str], instruction: str) -> str:
    """Store the task text in `env` under a new random name and return it.

    The text travels in this variable and is piped to reagent on stdin, so no
    process ever holds it in argv (Harbor issue #2311: `pkill -f` matched the
    task text and killed the agent).
    """
    task_env = f"REAGENT_TASK_{secrets.token_hex(8)}"
    env[task_env] = instruction
    return task_env


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
    policy: str | None = Field(
        default=None,
        description=(
            "Loop policies to enable, passed to reagent as --policy <value>, "
            "for example `completion,budget`. Unset runs reagent's default "
            "(budget, plus mask on the openai-chat wire); `none` turns every policy off."
        ),
    )
    max_tokens: int | None = Field(
        default=None,
        gt=0,
        description=(
            "The reply token cap, passed to reagent as --max-tokens. Set it "
            "for a gateway model whose output limit is below 64000."
        ),
    )
    wire: Wire | None = Field(
        default=None,
        description=(
            "Override the wire the model prefix picks: anthropic, openai-chat "
            "or openai-responses."
        ),
    )
    base_url: str | None = Field(
        default=None,
        description="Override the endpoint the model prefix picks.",
    )
    api_key_env: str | None = Field(
        default=None,
        description=(
            "Override the environment variable that holds the API key, for "
            "example OPENCODE_API_KEY."
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
        model = self._model_name()
        resolved = self._resolve_provider()

        # Harbor resolves a key only for a provider it knows. opencode-go and
        # commandcode are not in its table, so read the key from the host
        # environment instead. The key goes in the env, never in the command.
        env = dict(self.model_connection.env)
        key = env.get(resolved.api_key_env) or self._get_env(resolved.api_key_env)
        if not key:
            raise ValueError(
                f"{resolved.api_key_env} is not set in the host environment"
            )
        env[resolved.api_key_env] = key

        workdir = await self._resolve_workdir(environment)
        task_env = put_task_in_env(env, instruction)
        command = build_command(
            workdir=workdir,
            model_id=model_id(model),
            task_env=task_env,
            logs_dir=self.environment_logs_dir,
            timeout_sec=self.options.agent_timeout_sec,
            provider=resolved.wire,
            base_url=resolved.base_url,
            api_key_env=resolved.api_key_env,
            headers=session_headers(model),
            policy=self.options.policy,
            max_tokens=self.options.max_tokens,
        )
        await self.exec_as_agent(environment, command=command, env=env)

    def _resolve_provider(self) -> ProviderDefaults:
        """The wire, endpoint and key variable for the model, with overrides."""
        return provider_defaults(
            self._model_name(),
            wire=self.options.wire,
            base_url=self.options.base_url,
            api_key_env=self.options.api_key_env,
            anthropic_base_url=(
                self._get_env(*_ANTHROPIC_BASE_URL_ENVS)
                or _ANTHROPIC_DEFAULT_BASE_URL
            ),
        )

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

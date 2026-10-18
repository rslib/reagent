"""Tests for the ReagentAgent command, binary path and context population.

The command is run by a real ``bash`` with a fake reagent that records its
arguments, so the shell quoting is checked end to end without a container.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path, PurePosixPath

from harbor.models.agent.context import AgentContext

from reagent_harbor import agent
from reagent_harbor.agent import DIST_BINARY, build_command, populate_context

FIXTURE = Path(__file__).parent / "fixtures" / "events.jsonl"
HARD_INSTRUCTION = "fix \"it\" & 'that'\nthen $HOME `whoami` \\ done; exit 3"

FAKE_REAGENT = """#!/usr/bin/env python3
import json, sys
json.dump(sys.argv[1:], open(sys.argv[0] + ".args", "w"))
print('{"type":"summary","input":1,"output":2,"cache_read":0,"cache_write":0,"turns":1}')
print("an error line", file=sys.stderr)
sys.exit(124)
"""


def run_command(tmp_path: Path, timeout_sec: int | None) -> tuple[subprocess.CompletedProcess[str], Path]:
    fake = tmp_path / "reagent"
    fake.write_text(FAKE_REAGENT)
    fake.chmod(0o755)
    logs = tmp_path / "logs"
    command = build_command(
        workdir="/app dir",
        model_id="claude-sonnet-5-5",
        instruction=HARD_INSTRUCTION,
        logs_dir=PurePosixPath(logs),
        timeout_sec=timeout_sec,
    ).replace(agent._AGENT_BINARY, str(fake), 1)
    result = subprocess.run(["bash", "-c", command], capture_output=True, text=True, check=False)
    return result, logs


def test_the_command_passes_the_instruction_unchanged(tmp_path: Path) -> None:
    result, _ = run_command(tmp_path, timeout_sec=None)
    assert result.returncode == 0, result.stderr
    args = json.loads((tmp_path / "reagent.args").read_text())
    assert args == [
        "--json",
        "--cwd",
        "/app dir",
        "--model",
        "claude-sonnet-5-5",
        "-p",
        HARD_INSTRUCTION,
    ]


def test_the_command_sets_the_deadline_below_the_timeout(tmp_path: Path) -> None:
    run_command(tmp_path, timeout_sec=900)
    args = json.loads((tmp_path / "reagent.args").read_text())
    assert args[args.index("--deadline") + 1] == "870"


def test_a_failed_run_exits_0_and_keeps_its_files(tmp_path: Path) -> None:
    result, logs = run_command(tmp_path, timeout_sec=None)
    assert result.returncode == 0
    assert (logs / "reagent_exit_code.txt").read_text().strip() == "124"
    assert (logs / "reagent_stderr.txt").read_text() == "an error line\n"
    assert json.loads((logs / "reagent_events.jsonl").read_text())["type"] == "summary"


def test_populate_context_reads_the_synced_files(tmp_path: Path) -> None:
    (tmp_path / "reagent_events.jsonl").write_text(FIXTURE.read_text())
    (tmp_path / "reagent_exit_code.txt").write_text("124\n")
    context = AgentContext()
    populate_context(context, tmp_path)
    assert context.n_input_tokens == 90 + 165 + 15
    assert context.n_cache_tokens == 165
    assert context.n_output_tokens == 26
    assert context.metadata == {
        "cache_write_tokens": 15,
        "turns": 3,
        "malformed_event_lines": 0,
        "exit_code": 124,
    }


def test_populate_context_without_events_leaves_the_context_empty(tmp_path: Path) -> None:
    context = AgentContext()
    populate_context(context, tmp_path)
    assert context.n_input_tokens is None


def test_the_binary_path_is_where_build_sh_writes() -> None:
    assert DIST_BINARY == Path(__file__).resolve().parent.parent / "dist" / "reagent"


IMPORT = (
    "from harbor.utils.import_path import import_symbol; "
    "import_symbol('reagent_harbor.agent:ReagentAgent')"
)


def import_like_harbor(pythonpath: str | None) -> subprocess.CompletedProcess[str]:
    """Import the agent the way Harbor's console script does: `-P` keeps the
    cwd off sys.path, so only PYTHONPATH can make the module visible."""
    harbor_dir = Path(__file__).resolve().parent.parent
    env = {key: value for key, value in os.environ.items() if key != "PYTHONPATH"}
    if pythonpath is not None:
        env["PYTHONPATH"] = pythonpath
    return subprocess.run(
        ["uv", "run", "python", "-P", "-c", IMPORT],
        cwd=harbor_dir,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_harbor_imports_the_agent_with_the_run_sh_pythonpath() -> None:
    harbor_dir = str(Path(__file__).resolve().parent.parent)
    assert import_like_harbor(None).returncode != 0
    result = import_like_harbor(harbor_dir)
    assert result.returncode == 0, result.stderr

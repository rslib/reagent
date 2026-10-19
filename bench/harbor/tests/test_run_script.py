"""Tests for the ``run.sh`` launcher.

``run.sh --dry-run`` prints the exact harbor command it would run without
executing it (so the test needs no Harbor installation beyond the package and
no Docker). The printed command must name the agent import path, the model,
the dataset, and the trial count, and must never leak an API key value.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

RUN_SCRIPT = Path(__file__).parent.parent / "run.sh"
FAKE_KEY = "sk-ant-not-a-real-key-1234567890"


@pytest.fixture(autouse=True)
def _fake_key_env(monkeypatch: pytest.MonkeyPatch) -> None:
    """A sentinel key must never appear in run.sh's printed command."""
    monkeypatch.setenv("ANTHROPIC_API_KEY", FAKE_KEY)


def run_script(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(RUN_SCRIPT), *args],
        capture_output=True,
        text=True,
        env=os.environ.copy(),
        check=False,
    )


def test_dry_run_prints_exact_harbor_command() -> None:
    result = run_script("--dry-run", "claude-sonnet-5-5", "terminal-bench/terminal-bench-2-1", "3")
    assert result.returncode == 0, result.stderr

    line = result.stdout.strip()
    assert line.startswith("uv run harbor run ")
    assert "--dataset terminal-bench/terminal-bench-2-1" in line
    assert "--agent reagent_harbor.agent:ReagentAgent" in line
    assert "--model claude-sonnet-5-5" in line
    assert "--n-attempts 3" in line


def test_dry_run_passes_through_extra_harbor_args() -> None:
    result = run_script(
        "--dry-run", "claude-sonnet-5-5", "terminal-bench/terminal-bench-2-1", "3", "--n-concurrent", "4"
    )
    assert result.returncode == 0, result.stderr
    assert "--n-concurrent 4" in result.stdout


def test_dry_run_never_prints_the_api_key() -> None:
    result = run_script("--dry-run", "claude-sonnet-5-5", "terminal-bench/terminal-bench-2-1", "3")
    assert result.returncode == 0, result.stderr
    assert FAKE_KEY not in result.stdout
    assert FAKE_KEY not in result.stderr


def test_missing_arguments_fail_with_usage() -> None:
    result = run_script("--dry-run", "claude-sonnet-5-5", "terminal-bench/terminal-bench-2-1")
    assert result.returncode == 2
    assert "usage:" in result.stderr

DEV_TASKS = Path(__file__).parent.parent / "dev-tasks.txt"


def dev_task_names() -> list[str]:
    return [
        line.strip()
        for line in DEV_TASKS.read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def test_dev_includes_each_listed_task_and_the_caps() -> None:
    result = run_script(
        "--dry-run", "--dev", "anthropic/claude-haiku-5-5", "terminal-bench/terminal-bench-2-1", "1"
    )
    assert result.returncode == 0, result.stderr
    command = result.stdout
    names = dev_task_names()
    assert 20 <= len(names) <= 30
    for name in names:
        assert f"-i {name} " in command or command.rstrip().endswith(f"-i {name}")
    assert command.count(" -i ") == len(names)
    assert "-n 8" in command
    assert "--ak agent_timeout_sec=600" in command
    assert "#" not in command


def test_dev_extra_args_come_after_the_dev_flags() -> None:
    result = run_script(
        "--dry-run",
        "--dev",
        "anthropic/claude-haiku-5-5",
        "terminal-bench/terminal-bench-2-1",
        "1",
        "--n-concurrent",
        "2",
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.index("-n 8") < result.stdout.index("--n-concurrent 2")


def test_dev_list_names_are_unique_and_full() -> None:
    names = dev_task_names()
    assert len(names) == len(set(names))
    assert all(name.startswith("terminal-bench/") for name in names)

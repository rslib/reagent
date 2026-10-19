"""Tests for ``reagent_harbor.report``, on small fake job folders."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from reagent_harbor.report import (
    bootstrap_interval,
    failure_type,
    load_job,
    main,
    paired_difference,
    pass_rate,
    summarize,
)


def add_trial(
    job: Path,
    name: str,
    reward: float | None,
    *,
    exception: str | None = None,
    usage: list[tuple[int, int, int, int]] = (),
    stop: str | None = None,
) -> None:
    """Writes one trial folder the way Harbor and reagent lay it out."""
    trial = job / f"{name}__{len(list(job.glob(name + '__*')))}"
    (trial / "agent").mkdir(parents=True)
    result: dict = {"task_name": f"terminal-bench/{name}"}
    if reward is not None:
        result["verifier_result"] = {"rewards": {"reward": reward}}
    if exception is not None:
        result["exception_info"] = {"exception_type": exception}
    (trial / "result.json").write_text(json.dumps(result))
    lines = []
    for read, write, fresh, output in usage:
        lines.append({"type": "turn_start"})
        lines.append(
            {
                "type": "usage",
                "cache_read": read,
                "cache_write": write,
                "input": fresh,
                "output": output,
            }
        )
    if stop is not None:
        lines.append({"type": "summary", "stop": stop, "exit": "stopped"})
    (trial / "agent" / "reagent_events.jsonl").write_text(
        "".join(json.dumps(line) + "\n" for line in lines)
    )


@pytest.fixture
def job(tmp_path: Path) -> Path:
    job = tmp_path / "job"
    job.mkdir()
    (job / "result.json").write_text("{}")
    add_trial(job, "a", 1.0, usage=[(100, 10, 1, 5), (200, 10, 1, 5)])
    add_trial(job, "a", 0.0, usage=[(50, 0, 1, 4)])
    add_trial(job, "b", 1.0, usage=[(10, 0, 0, 1)])
    add_trial(job, "c", None, exception="AgentTimeoutError", usage=[(30, 0, 0, 2)])
    add_trial(job, "d", 0.0, usage=[(5, 0, 0, 1)], stop="max_tokens")
    add_trial(job, "e", 0.0, usage=[(5, 0, 0, 1)], stop="cancelled")
    return job


def test_load_job_reads_every_trial_and_its_tokens(job: Path) -> None:
    trials = load_job(job)
    assert len(trials) == 6
    first = next(t for t in trials if t.task == "terminal-bench/a" and t.reward == 1.0)
    assert first.prompt_tokens == 100 + 10 + 1 + 200 + 10 + 1
    assert first.output_tokens == 10
    assert first.turns == 2


def test_a_trial_without_a_reward_counts_as_a_failure(job: Path) -> None:
    trial = next(t for t in load_job(job) if t.task == "terminal-bench/c")
    assert trial.reward == 0.0
    assert failure_type(trial) == "AgentTimeoutError"


def test_failure_types_name_the_stop_or_a_wrong_answer(job: Path) -> None:
    kinds = {t.task: failure_type(t) for t in load_job(job) if t.reward == 0.0}
    assert kinds["terminal-bench/d"] == "max_tokens"
    assert kinds["terminal-bench/e"] == "cancelled"
    assert kinds["terminal-bench/a"] == "wrong_answer"


def test_pass_rate_averages_tasks_not_trials(job: Path) -> None:
    # a: 0.5, b: 1, c: 0, d: 0, e: 0 -> 1.5 / 5
    assert pass_rate(load_job(job)) == pytest.approx(0.3)


def test_bootstrap_interval_is_repeatable_and_contains_the_rate(job: Path) -> None:
    trials = load_job(job)
    low, high = bootstrap_interval(trials, samples=2000, seed=7)
    assert (low, high) == bootstrap_interval(trials, samples=2000, seed=7)
    assert 0.0 <= low <= pass_rate(trials) <= high <= 1.0


def test_summary_has_tokens_per_solved_task(job: Path) -> None:
    summary = summarize(load_job(job))
    assert summary["tasks"] == 5
    assert summary["trials"] == 6
    assert summary["solved"] == 2
    total = sum(t.prompt_tokens + t.output_tokens for t in load_job(job))
    assert summary["tokens_per_solved"] == total / 2
    assert summary["failures"] == {
        "AgentTimeoutError": 1,
        "cancelled": 1,
        "max_tokens": 1,
        "wrong_answer": 1,
    }


def test_paired_difference_uses_shared_tasks_only(tmp_path: Path) -> None:
    old = tmp_path / "old"
    new = tmp_path / "new"
    for job in (old, new):
        job.mkdir()
    for name, before, after in [("a", 0.0, 1.0), ("b", 1.0, 1.0), ("c", 0.0, 1.0)]:
        add_trial(old, name, before)
        add_trial(new, name, after)
    add_trial(new, "only-new", 0.0)
    diff, low, high, shared = paired_difference(
        load_job(new), load_job(old), samples=2000, seed=1
    )
    assert shared == 3
    assert diff == pytest.approx(2 / 3)
    assert low <= diff <= high


def test_main_prints_the_rate_interval_and_tokens(job: Path, capsys) -> None:
    assert main([str(job)]) == 0
    out = capsys.readouterr().out
    assert "pass rate" in out
    assert "95%" in out
    assert "tokens per solved task" in out
    assert "AgentTimeoutError" in out


def test_main_json_and_task_filter(job: Path, tmp_path: Path, capsys) -> None:
    tasks = tmp_path / "tasks.txt"
    tasks.write_text("# comment\nterminal-bench/a\nterminal-bench/b\n")
    assert main([str(job), "--tasks", str(tasks), "--json"]) == 0
    data = json.loads(capsys.readouterr().out)
    assert data["tasks"] == 2
    assert data["pass_rate"] == pytest.approx(0.75)


def test_main_against_prints_the_paired_difference(job: Path, capsys) -> None:
    assert main([str(job), "--against", str(job)]) == 0
    assert "difference" in capsys.readouterr().out

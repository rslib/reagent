"""Tests for ``reagent_harbor.internal.validate``, on fake job folders."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from reagent_harbor.internal.validate import (
    clear_expect,
    harbor_command,
    is_valid,
    main,
    fix_functions,
    split_results,
    task_reward,
    task_results,
    trial_reward,
)


def add_trial(jobs: Path, job: str, name: str, reward: float | None) -> Path:
    """Writes one trial folder the way Harbor lays it out."""
    trial = jobs / job / f"{name}__{len(list((jobs / job).glob(name + '__*')))}"
    trial.mkdir(parents=True)
    result: dict = {"task_name": f"internal/{name}"}
    if reward is not None:
        result["verifier_result"] = {"rewards": {"reward": reward}}
    (trial / "result.json").write_text(json.dumps(result))
    return trial


def test_trial_reward_reads_reward(tmp_path: Path) -> None:
    trial = add_trial(tmp_path, "j", "a", 1.0)
    assert trial_reward(trial / "result.json") == 1.0


def test_trial_reward_missing_is_zero(tmp_path: Path) -> None:
    trial = add_trial(tmp_path, "j", "a", None)
    assert trial_reward(trial / "result.json") == 0.0
    assert trial_reward(tmp_path / "absent.json") == 0.0


def test_trial_reward_null_verifier_is_zero(tmp_path: Path) -> None:
    trial = tmp_path / "t"
    trial.mkdir()
    (trial / "result.json").write_text(json.dumps({"verifier_result": None}))
    assert trial_reward(trial / "result.json") == 0.0


def test_task_reward_picks_task_and_latest_job(tmp_path: Path) -> None:
    add_trial(tmp_path, "2026-01-01__00-00-00", "a", 0.0)
    add_trial(tmp_path, "2026-01-02__00-00-00", "a", 1.0)
    add_trial(tmp_path, "2026-01-02__00-00-00", "b", 0.0)
    assert task_reward(tmp_path, "a") == 1.0
    assert task_reward(tmp_path, "b") == 0.0


def test_task_reward_no_jobs_is_zero(tmp_path: Path) -> None:
    assert task_reward(tmp_path / "none", "a") == 0.0
    assert task_reward(tmp_path, "a") == 0.0


def test_is_valid() -> None:
    assert is_valid(["a"])
    assert not is_valid([])


def test_harbor_command(tmp_path: Path) -> None:
    cmd = harbor_command(tmp_path, "x-1", "oracle", 2)
    assert cmd[:3] == ["uv", "run", "harbor"]
    assert cmd[cmd.index("--path") + 1] == str(tmp_path / "tasks" / "x-1")
    assert cmd[cmd.index("--max-retries") + 1] == "2"
    assert cmd[cmd.index("--retry-include") + 1] == "RuntimeError"
    assert cmd[cmd.index("--agent") + 1] == "oracle"
    assert cmd[cmd.index("--n-attempts") + 1] == "1"
    assert cmd[cmd.index("-n") + 1] == "2"
    assert cmd[cmd.index("--jobs-dir") + 1] == str(
        tmp_path / "validate-jobs" / "oracle"
    )


def test_dry_run_prints_commands(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "tasks" / "x-1").mkdir(parents=True)
    (tmp_path / "tasks" / "y-2").mkdir(parents=True)
    assert main(["--root", str(tmp_path), "--dry-run", "-n", "3"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 4
    assert sum("--agent oracle" in line for line in lines) == 2
    assert sum("--agent nop" in line for line in lines) == 2
    assert all("-n 3" in line for line in lines)
    assert not (tmp_path / "valid.json").exists()


def test_dry_run_id_filter(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "tasks" / "x-1").mkdir(parents=True)
    (tmp_path / "tasks" / "y-2").mkdir(parents=True)
    main(["--root", str(tmp_path), "--dry-run", "--id", "y-2"])
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 2
    assert all("y-2" in line and "x-1" not in line for line in lines)


def add_results(jobs: Path, job: str, name: str, results: dict | None) -> None:
    trial = add_trial(jobs, job, name, 1.0)
    if results is not None:
        (trial / "verifier").mkdir()
        (trial / "verifier" / "results.json").write_text(json.dumps(results))


def test_fail_to_pass_split() -> None:
    oracle = {"a": "pass", "b": "pass", "c": "pass", "d": "fail", "e": "pass"}
    nop = {"a": "fail", "b": "pass", "d": "fail"}
    f2p, p2p = split_results(oracle, nop)
    assert f2p == ["a", "c", "e"]
    assert p2p == ["b"]


def test_fail_to_pass_skips_tests_the_fix_adds() -> None:
    fix = "+    #[test]\n+    fn unit_in_src() {}\n+pub(crate) fn helper() {}\n fn old() {}\n"
    assert fix_functions(fix) == {"unit_in_src", "helper"}
    oracle = {"b m::tests::unit_in_src": "pass", "b hidden": "pass", "b f": "pass"}
    nop = {"b f": "fail"}
    f2p, _ = split_results(oracle, nop, fix_functions(fix))
    assert f2p == ["b f", "b hidden"]


def test_regrade_uses_stored_jobs_without_harbor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (tmp_path / "tasks" / "x-1" / "solution").mkdir(parents=True)
    (tmp_path / "tasks" / "x-1" / "tests").mkdir()
    (tmp_path / "tasks/x-1/solution/fix.patch").write_text("+fn t2() {}\n")
    for agent, results in (("oracle", {"b t1": "pass", "b t2": "pass"}), ("nop", {})):
        add_results(tmp_path / "validate-jobs" / agent, "j", "x-1", results)

    def run(cmd: list[str], check: bool = False) -> None:
        raise AssertionError("harbor ran")

    monkeypatch.setattr("reagent_harbor.internal.validate.subprocess.run", run)
    assert main(["--root", str(tmp_path), "--regrade"]) == 0
    expect = json.loads((tmp_path / "tasks/x-1/tests/expect.json").read_text())
    assert expect == {"fail_to_pass": ["b t1"], "pass_to_pass": []}


def test_fail_to_pass_task_results_reads_latest(tmp_path: Path) -> None:
    add_results(tmp_path, "2026-01-01__00-00-00", "a", {"old": "pass"})
    add_results(tmp_path, "2026-01-02__00-00-00", "a", {"new": "pass"})
    assert task_results(tmp_path, "a") == {"new": "pass"}
    assert task_results(tmp_path, "zzz") == {}
    assert task_results(tmp_path / "none", "a") == {}


def test_fail_to_pass_missing_results_is_empty(tmp_path: Path) -> None:
    add_results(tmp_path, "j", "a", None)
    assert task_results(tmp_path, "a") == {}


def fake_harbor(root: Path, task_id: str, oracle: dict | None, nop: dict | None):
    """Returns a runner that writes job folders instead of calling Harbor."""

    def run(cmd: list[str], check: bool = False) -> None:
        agent = cmd[cmd.index("--agent") + 1]
        results = oracle if agent == "oracle" else nop
        add_results(root / "validate-jobs" / agent, "j", task_id, results)

    return run


def run_main(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    oracle: dict | None,
    nop: dict | None,
) -> dict:
    (tmp_path / "tasks" / "x-1" / "tests").mkdir(parents=True)
    monkeypatch.setattr(
        "reagent_harbor.internal.validate.subprocess.run",
        fake_harbor(tmp_path, "x-1", oracle, nop),
    )
    assert main(["--root", str(tmp_path)]) == 0
    return json.loads((tmp_path / "valid.json").read_text())


def test_fail_to_pass_valid_and_expect_written(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    oracle = {"t1": "pass", "t2": "pass", "t3": "fail"}
    nop = {"t1": "fail", "t2": "pass"}
    valid = run_main(tmp_path, monkeypatch, oracle, nop)
    assert valid["x-1"] == {
        "fail_to_pass": 1,
        "pass_to_pass": 1,
        "valid": True,
        "oracle_reward": 1.0,
        "nop_reward": 1.0,
    }
    expect = json.loads((tmp_path / "tasks/x-1/tests/expect.json").read_text())
    assert expect == {"fail_to_pass": ["t1"], "pass_to_pass": ["t2"]}


def test_fail_to_pass_none_is_invalid_but_expect_written(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    same = {"t1": "pass", "t2": "fail"}
    valid = run_main(tmp_path, monkeypatch, same, same)
    assert valid["x-1"]["valid"] is False
    assert valid["x-1"]["fail_to_pass"] == 0
    expect = json.loads((tmp_path / "tasks/x-1/tests/expect.json").read_text())
    assert expect == {"fail_to_pass": [], "pass_to_pass": ["t1"]}


def test_fail_to_pass_missing_results_invalid(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    valid = run_main(tmp_path, monkeypatch, None, None)
    assert valid["x-1"]["valid"] is False
    assert valid["x-1"]["fail_to_pass"] == 0 and valid["x-1"]["pass_to_pass"] == 0


def test_expect_removed_before_collect(tmp_path: Path) -> None:
    tests = tmp_path / "tasks" / "x-1" / "tests"
    tests.mkdir(parents=True)
    (tests / "expect.json").write_text("{}")
    clear_expect(tmp_path, "x-1")
    assert not (tests / "expect.json").exists()
    clear_expect(tmp_path, "x-1")  # absent is fine


def test_expect_cleared_by_main_before_jobs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    tests = tmp_path / "tasks" / "x-1" / "tests"
    tests.mkdir(parents=True)
    (tests / "expect.json").write_text("{}")
    seen = []

    def run(cmd: list[str], check: bool = False) -> None:
        seen.append((tests / "expect.json").exists())

    monkeypatch.setattr("reagent_harbor.internal.validate.subprocess.run", run)
    main(["--root", str(tmp_path)])
    assert seen == [False, False]

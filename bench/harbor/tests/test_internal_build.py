"""Tests for ``reagent_harbor.internal.build``, on a fake git repo."""

from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path

import pytest

from reagent_harbor.internal.build import (
    DEV_DEPS_PY,
    GRADE_PY,
    NEXTEST_VERSION,
    SCCACHE_VERSION,
    base_image,
    build_task,
    fix_dev_deps,
    main,
)

GIT = ["git", "-c", "user.email=t@t", "-c", "user.name=t"]


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        [*GIT, *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()


@pytest.fixture
def repo(tmp_path: Path) -> dict:
    repo = tmp_path / "fake-repo"
    (repo / "src").mkdir(parents=True)
    (repo / "tests").mkdir()
    git(repo, "init", "-q")
    (repo / "Cargo.toml").write_text('[package]\nname = "fakecrate"\nversion = "0.1.0"\n')
    (repo / "src" / "lib.rs").write_text("pub fn f() -> i32 { 1 }\n")
    (repo / "tests" / "it.rs").write_text("#[test]\nfn t() { assert_eq!(1, 1); }\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "base")
    parent = git(repo, "rev-parse", "HEAD")
    (repo / "src" / "lib.rs").write_text("pub fn f() -> i32 { 2 }\n")
    (repo / "tests" / "it.rs").write_text(
        "#[test]\nfn t() { assert_eq!(fakecrate::f(), 2); }\n"
    )
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "fix: f returns two")
    fix = git(repo, "rev-parse", "HEAD")
    return {
        "id": "fake-repo-" + fix[:10],
        "repo": str(repo),
        "fix": fix,
        "parent": parent,
        "subject": "fix: f returns two",
        "src_files": ["src/lib.rs"],
        "test_files": ["tests/it.rs"],
        "lang": "rust",
        "toolchain": None,
        "crates": ["fakecrate"],
        "session": "/nonexistent",
        "request": "make f return two",
        "correction": False,
        "diff_lines": 2,
    }


def make_root(tmp_path: Path, candidates: list[dict], instruct: list[str]) -> Path:
    root = tmp_path / "root"
    (root / "instructions").mkdir(parents=True)
    (root / "candidates.jsonl").write_text(
        "".join(json.dumps(c) + "\n" for c in candidates)
    )
    for cid in instruct:
        (root / "instructions" / f"{cid}.md").write_text(f"Do {cid}.\n")
    return root


def apply(workdir: Path, patch: Path) -> None:
    subprocess.run(
        ["git", "apply", "--whitespace=nowarn", str(patch)],
        cwd=workdir,
        check=True,
        capture_output=True,
    )


def test_builds_task_files(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    assert main(["--root", str(root)]) == 0
    task = root / "tasks" / repo["id"]
    for rel in [
        "task.toml",
        "instruction.md",
        "environment/Dockerfile",
        "environment/repo/Cargo.toml",
        "environment/repo/src/lib.rs",
        "tests/hidden.patch",
        "tests/test.sh",
        "tests/files/tests/it.rs",
        "solution/solve.sh",
        "solution/fix.patch",
    ]:
        assert (task / rel).is_file(), rel
    assert not (task / "environment/repo/.git").exists()
    assert (task / "instruction.md").read_text() == f"Do {repo['id']}.\n"


def test_patches_apply_to_parent_archive(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    task = root / "tasks" / repo["id"]
    work = tmp_path / "work"
    subprocess.run(
        ["cp", "-R", str(task / "environment/repo"), str(work)], check=True
    )
    apply(work, task / "tests/hidden.patch")
    assert "fakecrate::f()" in (work / "tests/it.rs").read_text()
    assert (work / "src/lib.rs").read_text() == "pub fn f() -> i32 { 1 }\n"
    apply(work, task / "solution/fix.patch")
    assert (work / "src/lib.rs").read_text() == "pub fn f() -> i32 { 2 }\n"
    assert (task / "tests/files/tests/it.rs").read_text() == (
        work / "tests/it.rs"
    ).read_text()


@pytest.mark.parametrize(
    "toolchain, image",
    [
        ("1.98", "rust:1.98-bookworm"),
        ("stable", "rust:1-bookworm"),
        (None, "rust:1-bookworm"),
        ("nightly-2025-01-01", "rust:1-bookworm"),
    ],
)
def test_base_image(toolchain: str | None, image: str) -> None:
    assert base_image(toolchain) == image


def test_dockerfile_uses_toolchain(tmp_path: Path, repo: dict) -> None:
    repo["toolchain"] = "1.98"
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    text = (root / "tasks" / repo["id"] / "environment/Dockerfile").read_text()
    assert text.startswith("FROM rust:1.98-bookworm\n")
    assert "COPY repo/ /app/" in text


def test_dockerfile_downloads_tools_from_github(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    text = (root / "tasks" / repo["id"] / "environment/Dockerfile").read_text()
    assert "get.nexte.st" not in text
    assert (
        "https://github.com/nextest-rs/nextest/releases/download/"
        f"cargo-nextest-{NEXTEST_VERSION}/cargo-nextest-{NEXTEST_VERSION}-$arch-unknown-linux-musl.tar.gz"
    ) in text
    assert (
        f"https://github.com/mozilla/sccache/releases/download/v{SCCACHE_VERSION}/"
        f"sccache-v{SCCACHE_VERSION}-$arch-unknown-linux-musl.tar.gz"
    ) in text
    assert "arm64) arch=aarch64" in text


def test_dockerfile_warm_build_uses_a_shared_sccache_cache(
    tmp_path: Path, repo: dict
) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    text = (root / "tasks" / repo["id"] / "environment/Dockerfile").read_text()
    warm = next(line for line in text.splitlines() if "--no-run" in line)
    assert warm.startswith("RUN --mount=type=cache,target=/sccache,sharing=shared ")
    assert "RUSTC_WRAPPER=sccache" in warm
    assert "SCCACHE_DIR=/sccache" in warm
    assert "ENV RUSTC_WRAPPER" not in text


def test_test_sh(tmp_path: Path, repo: dict) -> None:
    repo["crates"] = ["fakecrate", "othercrate"]
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    script = root / "tasks" / repo["id"] / "tests/test.sh"
    subprocess.run(["bash", "-n", str(script)], check=True)
    text = script.read_text()
    assert "-p fakecrate" in text and "-p othercrate" in text
    assert "/logs/verifier/reward.txt" in text


def test_test_sh_workspace_when_no_crates(tmp_path: Path, repo: dict) -> None:
    repo["crates"] = []
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    script = root / "tasks" / repo["id"] / "tests/test.sh"
    subprocess.run(["bash", "-n", str(script)], check=True)
    assert "--workspace" in script.read_text()


def test_task_toml(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    data = tomllib.loads((root / "tasks" / repo["id"] / "task.toml").read_text())
    assert data["schema_version"] == "1.1"
    assert data["task"]["name"] == f"internal/{repo['id']}"
    assert data["agent"]["timeout_sec"] == 1200.0
    assert data["verifier"]["timeout_sec"] == 1200.0
    env = data["environment"]
    assert env["build_timeout_sec"] == 1800.0
    assert env["cpus"] == 2
    assert env["memory_mb"] == 4096
    assert env["storage_mb"] == 20480
    assert env["allow_internet"] is True
    assert "docker_image" not in env


def test_skips_without_instruction_and_non_rust(
    tmp_path: Path, repo: dict, capsys: pytest.CaptureFixture[str]
) -> None:
    other = {**repo, "id": "py-1", "lang": "python"}
    bare = {**repo, "id": "bare-1"}
    root = make_root(tmp_path, [repo, other, bare], [repo["id"], "py-1"])
    assert main(["--root", str(root)]) == 0
    out = capsys.readouterr().out
    assert (root / "tasks" / repo["id"]).is_dir()
    assert not (root / "tasks" / "py-1").exists()
    assert not (root / "tasks" / "bare-1").exists()
    assert "bare-1" in out and "no instruction" in out
    assert "py-1" in out and "python" in out


def test_id_filter_and_force(tmp_path: Path, repo: dict) -> None:
    other = {**repo, "id": "other-1"}
    root = make_root(tmp_path, [repo, other], [repo["id"], "other-1"])
    main(["--root", str(root), "--id", repo["id"]])
    assert (root / "tasks" / repo["id"]).is_dir()
    assert not (root / "tasks" / "other-1").exists()

    marker = root / "tasks" / repo["id"] / "marker"
    marker.write_text("x")
    main(["--root", str(root), "--id", repo["id"]])
    assert marker.exists()
    main(["--root", str(root), "--id", repo["id"], "--force"])
    assert not marker.exists()
    assert (root / "tasks" / repo["id"] / "task.toml").is_file()


def test_build_task_returns_dir(tmp_path: Path, repo: dict) -> None:
    out = build_task(repo, "hello\n", tmp_path / "tasks")
    assert out == tmp_path / "tasks" / repo["id"]
    assert (out / "instruction.md").read_text() == "hello\n"


NEXTEST_SAMPLE = """\
    Starting 6 tests across 2 binaries
        PASS [   0.002s] ( 1/6) reagent::bin/reagent args::tests::ok
        FAIL [   0.003s] ( 2/6) reagent::bin/reagent args::tests::bad
        FAIL [   0.003s] ( 2/6) reagent::bin/reagent args::tests::bad
        SIGSEGV [   0.004s] ( 3/6) reagent::bin/reagent crash::tests::boom
        TRY 1 FAIL [   0.005s] ( 4/6) reagent::bin/reagent flaky::tests::sometimes
        TRY 2 PASS [   0.005s] ( 4/6) reagent::bin/reagent flaky::tests::sometimes
       FLAKY 2/3 [   0.010s] ( 4/6) reagent::bin/reagent flaky::tests::sometimes
        TIMEOUT [  60.000s] ( 5/6) reagent lib::tests::slow
        PASS [   0.001s] ( 6/6) reagent lib::tests::fine
------------
     Summary [   0.5s] 6 tests run: 3 passed, 3 failed
"""


def run_grade(
    tmp_path: Path, outputs: list[str], expect: dict | None
) -> tuple[dict, str]:
    script = tmp_path / "grade.py"
    script.write_text(GRADE_PY)
    logs = tmp_path / "logs"
    logs.mkdir(exist_ok=True)
    for i, text in enumerate(outputs):
        (logs / f"nextest-c{i}.txt").write_text(text)
    cmd = [sys.executable, str(script), str(logs)]
    if expect is not None:
        (tmp_path / "expect.json").write_text(json.dumps(expect))
        cmd.append(str(tmp_path / "expect.json"))
    else:
        cmd.append(str(tmp_path / "absent.json"))
    subprocess.run(cmd, check=True, cwd=tmp_path)
    return (
        json.loads((logs / "results.json").read_text()),
        (logs / "reward.txt").read_text().strip(),
    )


def test_per_test_results_parse(tmp_path: Path) -> None:
    results, _ = run_grade(tmp_path, [NEXTEST_SAMPLE], None)
    assert results == {
        "reagent::bin/reagent args::tests::ok": "pass",
        "reagent::bin/reagent args::tests::bad": "fail",
        "reagent::bin/reagent crash::tests::boom": "fail",
        "reagent::bin/reagent flaky::tests::sometimes": "pass",
        "reagent lib::tests::slow": "fail",
        "reagent lib::tests::fine": "pass",
    }


def test_per_test_results_merge_files(tmp_path: Path) -> None:
    a = "        PASS [   0.002s] (1/1) a::bin/a t::one\n"
    b = "        FAIL [   0.002s] (1/1) b::bin/b t::two\n"
    results, _ = run_grade(tmp_path, [a, b], None)
    assert results == {"a::bin/a t::one": "pass", "b::bin/b t::two": "fail"}


def test_expect_reward_all_pass(tmp_path: Path) -> None:
    expect = {
        "fail_to_pass": ["reagent::bin/reagent args::tests::ok"],
        "pass_to_pass": ["reagent lib::tests::fine"],
    }
    _, reward = run_grade(tmp_path, [NEXTEST_SAMPLE], expect)
    assert reward == "1"


def test_expect_reward_one_fails(tmp_path: Path) -> None:
    expect = {
        "fail_to_pass": ["reagent::bin/reagent args::tests::ok"],
        "pass_to_pass": ["reagent::bin/reagent args::tests::bad"],
    }
    _, reward = run_grade(tmp_path, [NEXTEST_SAMPLE], expect)
    assert reward == "0"


def test_expect_reward_missing_key(tmp_path: Path) -> None:
    expect = {"fail_to_pass": ["no::such test"], "pass_to_pass": []}
    _, reward = run_grade(tmp_path, [NEXTEST_SAMPLE], expect)
    assert reward == "0"


def test_expect_reward_empty_fail_to_pass(tmp_path: Path) -> None:
    expect = {"fail_to_pass": [], "pass_to_pass": ["reagent lib::tests::fine"]}
    _, reward = run_grade(tmp_path, [NEXTEST_SAMPLE], expect)
    assert reward == "0"


def test_expect_collect_mode(tmp_path: Path) -> None:
    ok = "        PASS [   0.002s] (1/2) a::bin/a t::one\n        PASS [   0.002s] (2/2) a::bin/a t::two\n"
    assert run_grade(tmp_path, [ok], None)[1] == "1"
    assert run_grade(tmp_path, [NEXTEST_SAMPLE], None)[1] == "0"
    assert run_grade(tmp_path, ["error: could not compile\n"], None)[1] == "0"


def test_features_all_features_with_fallback(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    task = root / "tasks" / repo["id"]
    text = (task / "tests/test.sh").read_text()
    assert "run_nextest fakecrate -p fakecrate" in text
    assert "/logs/verifier/nextest-$name.txt" in text
    assert "python3 /tests/grade.py" in text
    # The fallback run is the one without the flag.
    assert text.count("cargo nextest run") == 2
    assert "status-level none --all-features" in text
    assert (task / "tests/grade.py").read_text() == GRADE_PY
    subprocess.run(["bash", "-n", str(task / "tests/test.sh")], check=True)
    docker = (task / "environment/Dockerfile").read_text()
    assert (
        "(cargo nextest run --no-run --all-features -p fakecrate "
        "|| cargo nextest run --no-run -p fakecrate || true)"
    ) in docker


def test_memory_and_cpus(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    data = tomllib.loads((root / "tasks" / repo["id"] / "task.toml").read_text())
    assert data["environment"]["memory_mb"] == 4096
    assert data["environment"]["cpus"] == 2


FIX_WITH_DEV_DEPS = """diff --git a/crates/db/Cargo.toml b/crates/db/Cargo.toml
--- a/crates/db/Cargo.toml
+++ b/crates/db/Cargo.toml
@@ -12,5 +12,9 @@ workspace = true
 [dependencies]
 rusqlite = { workspace = true }
+rmp-serde.workspace = true
+
+[dev-dependencies]
+serde_json.workspace = true
+# a comment
diff --git a/crates/db/src/lib.rs b/crates/db/src/lib.rs
--- a/crates/db/src/lib.rs
+++ b/crates/db/src/lib.rs
@@ -1 +1 @@
+[dev-dependencies]
+not_a_dep = 1
"""


def test_fix_dev_deps_finds_only_added_dev_dependencies() -> None:
    assert fix_dev_deps(FIX_WITH_DEV_DEPS) == {
        "crates/db/Cargo.toml": ["serde_json.workspace = true"]
    }


def run_dev_deps(tmp_path: Path, manifest: str, lines: list[str]) -> str:
    (tmp_path / "Cargo.toml").write_text(manifest)
    (tmp_path / "dev_deps.py").write_text(DEV_DEPS_PY)
    (tmp_path / "spec.json").write_text(json.dumps({"Cargo.toml": lines}))
    subprocess.run(
        [sys.executable, str(tmp_path / "dev_deps.py"), str(tmp_path / "spec.json"), str(tmp_path)],
        check=True,
    )
    return (tmp_path / "Cargo.toml").read_text()


def test_dev_deps_adds_a_missing_section(tmp_path: Path) -> None:
    text = run_dev_deps(
        tmp_path, '[package]\nname = "a"\n', ["serde_json.workspace = true"]
    )
    assert tomllib.loads(text)["dev-dependencies"] == {"serde_json": {"workspace": True}}


def test_dev_deps_adds_to_the_section_and_keeps_present_ones(tmp_path: Path) -> None:
    manifest = '[package]\nname = "a"\n\n[dev-dependencies]\ntempfile = "3"\n'
    text = run_dev_deps(tmp_path, manifest, ['tempfile = "3"', 'serde_json = "1"'])
    assert tomllib.loads(text)["dev-dependencies"] == {"tempfile": "3", "serde_json": "1"}


def test_dev_deps_without_a_spec_changes_nothing(tmp_path: Path) -> None:
    (tmp_path / "dev_deps.py").write_text(DEV_DEPS_PY)
    subprocess.run(
        [sys.executable, str(tmp_path / "dev_deps.py"), str(tmp_path / "none.json"), str(tmp_path)],
        check=True,
    )


def test_test_sh_adds_dev_deps_after_the_hidden_patch(tmp_path: Path, repo: dict) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    task = root / "tasks" / repo["id"]
    script = (task / "tests" / "test.sh").read_text()
    assert script.index("hidden.patch") < script.index("dev_deps.py") < script.index("run_nextest ")
    assert (task / "tests" / "dev_deps.py").is_file()
    assert not (task / "tests" / "dev-deps.json").exists()


def test_force_rebuild_keeps_the_validated_test_lists(
    tmp_path: Path, repo: dict
) -> None:
    root = make_root(tmp_path, [repo], [repo["id"]])
    main(["--root", str(root)])
    expect = root / "tasks" / repo["id"] / "tests" / "expect.json"
    expect.write_text('{"fail_to_pass": ["t"], "pass_to_pass": []}\n')
    main(["--root", str(root), "--force"])
    assert json.loads(expect.read_text())["fail_to_pass"] == ["t"]

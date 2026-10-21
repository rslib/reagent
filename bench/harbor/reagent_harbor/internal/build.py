"""Builds Harbor task folders from mined candidates.

Reads ``candidates.jsonl`` under the data root and, for each candidate that has
``instructions/<id>.md``, writes ``tasks/<id>/``. Run it with
``uv run python -m reagent_harbor.internal.build [--root DATA_ROOT] [--id ID ...] [--force]``.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

DEFAULT_ROOT = "~/.local/share/reagent/internal-bench"

TASK_TOML = """schema_version = "1.1"

[task]
name = "internal/{id}"
description = {description}

[agent]
timeout_sec = 1200.0

[verifier]
timeout_sec = 1200.0

[environment]
build_timeout_sec = 1800.0
cpus = 2
memory_mb = 4096
storage_mb = 20480
allow_internet = true
"""

NEXTEST_VERSION = "0.9.148"
SCCACHE_VERSION = "0.18.0"

# Releases come from GitHub, which lab proxies allow when other hosts are blocked.
DOCKERFILE = """FROM {image}
RUN apt-get update \\
    && apt-get install -y --no-install-recommends git python3 curl ca-certificates cmake \\
    && rm -rf /var/lib/apt/lists/*
RUN arch="$(uname -m)" \\
    && case "$arch" in arm64) arch=aarch64 ;; esac \\
    && curl -LsSf "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{nextest}/cargo-nextest-{nextest}-$arch-unknown-linux-musl.tar.gz" \\
        | tar zxf - -C /usr/local/cargo/bin \\
    && curl -LsSf "https://github.com/mozilla/sccache/releases/download/v{sccache}/sccache-v{sccache}-$arch-unknown-linux-musl.tar.gz" \\
        | tar zxf - --strip-components=1 -C /usr/local/bin "sccache-v{sccache}-$arch-unknown-linux-musl/sccache"
COPY repo/ /app/
WORKDIR /app
RUN git init -q && git add -A && git -c user.email=b@b -c user.name=b commit -qm base
# Warm the cache with the grader's exact commands: cargo picks dependency
# features per package selection, so a workspace build would not match. The
# sccache folder is a BuildKit cache shared by every task build on the host.
RUN --mount=type=cache,target=/sccache,sharing=shared RUSTC_WRAPPER=sccache SCCACHE_DIR=/sccache sh -c '{warm}'
"""

TEST_SH = """#!/bin/bash
set -u
mkdir -p /logs/verifier
cd /app || exit 1

# The agent may already have created the same test files, so the plain apply
# can fail; fall back to a three-way apply, then to the stored fix versions.
if ! git apply --whitespace=nowarn /tests/hidden.patch; then
    if ! git apply --3way /tests/hidden.patch; then
        cp -R /tests/files/. /app/
    fi
fi
python3 /tests/dev_deps.py /tests/dev-deps.json /app

# run_nextest NAME ARGS...: tests that sit behind cargo features need
# --all-features, but some crates cannot build with it, so a run that did not
# get as far as reporting any test is repeated without it.
run_nextest() {{
    local name=$1 out rc
    shift
    out=/logs/verifier/nextest-$name.txt
    cargo nextest run --no-fail-fast --status-level all --final-status-level none --all-features "$@" > "$out" 2>&1
    rc=$?
    if [ "$rc" -ne 0 ] && ! grep -Eq '^[[:space:]]+(TRY [0-9]+ )?(PASS|FAIL|SIGSEGV|SIGABRT|SIGKILL|TIMEOUT|LEAK)' "$out"; then
        cargo nextest run --no-fail-fast --status-level all --final-status-level none "$@" > "$out" 2>&1
    fi
}}

{runs}
python3 /tests/grade.py || echo 0 > /logs/verifier/reward.txt
exit 0
"""

GRADE_PY = r'''"""Grades a run from the captured nextest output.

Writes results.json ({"<binary_id> <test_name>": "pass" | "fail"}) and
reward.txt under the logs dir. With an expect file the reward is 1 when every
listed test passed; without one (collect mode) it is 1 when all tests passed.
"""

import glob
import json
import os
import re
import sys

STATUS = re.compile(
    r"^\s+(?:TRY \d+ )?(PASS|FAIL|SIGSEGV|SIGABRT|SIGKILL|TIMEOUT|LEAK-FAIL|LEAK)\s+\["
)
TEST = re.compile(r"\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)")


def parse(text, results):
    for line in text.splitlines():
        status = STATUS.match(line)
        test = TEST.search(line)
        if status and test:
            # The last status wins, so a retry that ends in PASS is a pass.
            results[test.group(1) + " " + test.group(2)] = (
                "pass" if status.group(1) == "PASS" else "fail"
            )


def main():
    logs = sys.argv[1] if len(sys.argv) > 1 else "/logs/verifier"
    expect_file = sys.argv[2] if len(sys.argv) > 2 else "/tests/expect.json"
    results = {}
    for path in sorted(glob.glob(os.path.join(logs, "nextest-*.txt"))):
        with open(path, errors="replace") as f:
            parse(f.read(), results)
    with open(os.path.join(logs, "results.json"), "w") as f:
        json.dump(results, f, indent=2, sort_keys=True)

    if os.path.exists(expect_file):
        with open(expect_file) as f:
            expect = json.load(f)
        want = list(expect.get("fail_to_pass", [])) + list(expect.get("pass_to_pass", []))
        ok = bool(expect.get("fail_to_pass")) and all(
            results.get(key) == "pass" for key in want
        )
    else:
        ok = bool(results) and all(v == "pass" for v in results.values())
    with open(os.path.join(logs, "reward.txt"), "w") as f:
        f.write("1\n" if ok else "0\n")


main()
'''

DEV_DEPS_PY = r'''"""Adds the fix's dev-dependencies that a manifest lacks.

The hidden tests may use a crate that only the fix adds, and the request
does not name it. Reads {"<manifest>": ["<dependency line>", ...]}.
"""

import json
import os
import re
import sys


def main():
    spec, root = sys.argv[1], sys.argv[2]
    if not os.path.exists(spec):
        return
    with open(spec) as f:
        wanted = json.load(f)
    for manifest, lines in wanted.items():
        path = os.path.join(root, manifest)
        if not os.path.exists(path):
            continue
        with open(path) as f:
            text = f.read()
        missing = []
        for line in lines:
            name = re.match(r"\s*([A-Za-z0-9_-]+)", line).group(1)
            if not re.search(rf"(?m)^\s*{re.escape(name)}\s*[.=]", text):
                missing.append(line)
        if not missing:
            continue
        block = "".join(line + "\n" for line in missing)
        if re.search(r"(?m)^\[dev-dependencies\][ \t]*$", text):
            text = re.sub(
                r"(?m)^\[dev-dependencies\][ \t]*$\n?",
                lambda m: m.group(0).rstrip("\n") + "\n" + block,
                text,
                count=1,
            )
        else:
            text = text.rstrip("\n") + "\n\n[dev-dependencies]\n" + block
        with open(path, "w") as f:
            f.write(text)


main()
'''

SOLVE_SH = """#!/bin/bash
set -eu
cd /app
git apply --whitespace=nowarn "$(dirname "$0")/fix.patch"
"""


def base_image(toolchain: str | None) -> str:
    """Returns the Rust base image for a toolchain channel.

    Only an exact version such as ``1.98`` pins the image; ``stable``, a
    nightly or no toolchain use the latest stable.
    """
    if toolchain and re.fullmatch(r"\d+\.\d+(\.\d+)?", toolchain):
        return f"rust:{toolchain}-bookworm"
    return "rust:1-bookworm"


def warm_commands(candidate: dict) -> str:
    """The grader's nextest builds, with --no-run, chained for the warm-up step."""
    selections = [f"-p {_quote(crate)}" for crate in candidate.get("crates") or []]
    steps = [
        f"(cargo nextest run --no-run --all-features {sel} || cargo nextest run --no-run {sel} || true)"
        for sel in selections or ["--workspace"]
    ]
    return " && ".join(steps)


def dockerfile(candidate: dict) -> str:
    return DOCKERFILE.format(
        image=base_image(candidate.get("toolchain")),
        nextest=NEXTEST_VERSION,
        sccache=SCCACHE_VERSION,
        warm=warm_commands(candidate),
    )


def test_script(candidate: dict) -> str:
    crates = candidate.get("crates") or []
    if crates:
        cmds = [
            f"run_nextest {_quote(crate)} -p {_quote(crate)}" for crate in crates
        ]
    else:
        cmds = ["run_nextest workspace --workspace"]
    return TEST_SH.format(runs="\n".join(cmds))


def task_toml(candidate: dict) -> str:
    description = json.dumps(candidate.get("subject", ""))
    return TASK_TOML.format(id=candidate["id"], description=description)


def fix_dev_deps(fix_patch: str) -> dict[str, list[str]]:
    """The dependency lines that the fix adds under ``[dev-dependencies]``,
    per ``Cargo.toml``."""
    found: dict[str, list[str]] = {}
    manifest = None
    section = None
    for line in fix_patch.splitlines():
        if line.startswith("diff --git "):
            path = line.split(" b/", 1)[-1]
            manifest = path if path.endswith("Cargo.toml") else None
            section = None
        elif manifest is None or line.startswith(("+++", "---", "@@")):
            continue
        elif line[:1] in (" ", "+"):
            body = line[1:].strip()
            if body.startswith("["):
                section = body.strip("[]").strip()
            elif (
                line[0] == "+"
                and section == "dev-dependencies"
                and body
                and not body.startswith("#")
            ):
                found.setdefault(manifest, []).append(body)
    return found


def _quote(text: str) -> str:
    if re.fullmatch(r"[\w.@+-]+", text):
        return text
    return "'" + text.replace("'", "'\\''") + "'"


def _git(repo: str, *args: str) -> bytes:
    return subprocess.run(
        ["git", "-C", repo, *args], check=True, capture_output=True
    ).stdout


def _write(path: Path, content: str | bytes, mode: int | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(content, str):
        path.write_text(content)
    else:
        path.write_bytes(content)
    if mode is not None:
        path.chmod(mode)


def build_task(
    candidate: dict, instruction: str, tasks_dir: Path, force: bool = False
) -> Path:
    """Writes ``tasks_dir/<id>/`` and returns it.

    An existing folder is kept as is unless ``force`` is set.
    """
    task = tasks_dir / candidate["id"]
    expect = None
    if task.exists():
        if not force:
            return task
        # The test lists come from validation, which a rebuild does not redo.
        expect_file = task / "tests" / "expect.json"
        expect = expect_file.read_bytes() if expect_file.exists() else None
        shutil.rmtree(task)
    repo, parent, fix = candidate["repo"], candidate["parent"], candidate["fix"]

    archive = _git(repo, "archive", "--format=tar", parent)
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(task / "environment" / "repo", filter="data")

    test_files = candidate["test_files"]
    src_files = candidate["src_files"]
    _write(
        task / "tests" / "hidden.patch",
        _git(repo, "diff", "--binary", parent, fix, "--", *test_files),
    )
    for rel in test_files:
        try:
            blob = _git(repo, "show", f"{fix}:{rel}")
        except subprocess.CalledProcessError:
            continue  # deleted in the fix
        _write(task / "tests" / "files" / rel, blob)
    fix_patch = _git(repo, "diff", "--binary", parent, fix, "--", *src_files)
    _write(task / "solution" / "fix.patch", fix_patch)
    dev_deps = fix_dev_deps(fix_patch.decode(errors="replace"))
    if dev_deps:
        _write(
            task / "tests" / "dev-deps.json", json.dumps(dev_deps, indent=2) + "\n"
        )

    _write(task / "environment" / "Dockerfile", dockerfile(candidate))
    _write(task / "tests" / "test.sh", test_script(candidate), 0o755)
    _write(task / "tests" / "grade.py", GRADE_PY)
    _write(task / "tests" / "dev_deps.py", DEV_DEPS_PY)
    _write(task / "solution" / "solve.sh", SOLVE_SH, 0o755)
    _write(task / "task.toml", task_toml(candidate))
    _write(task / "instruction.md", instruction)
    if expect is not None:
        _write(task / "tests" / "expect.json", expect)
    return task


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root", default=os.environ.get("REAGENT_INTERNAL_BENCH", DEFAULT_ROOT)
    )
    parser.add_argument("--id", action="append", dest="ids", default=[])
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args(argv)

    root = Path(args.root).expanduser()
    candidates = [
        json.loads(line)
        for line in (root / "candidates.jsonl").read_text().splitlines()
        if line.strip()
    ]
    built = 0
    for candidate in candidates:
        cid = candidate["id"]
        if args.ids and cid not in args.ids:
            continue
        if candidate.get("lang") != "rust":
            print(f"skip {cid}: unsupported lang {candidate.get('lang')}")
            continue
        instruction_file = root / "instructions" / f"{cid}.md"
        if not instruction_file.is_file():
            print(f"skip {cid}: no instruction file")
            continue
        build_task(
            candidate, instruction_file.read_text(), root / "tasks", args.force
        )
        print(f"built {cid}")
        built += 1
    print(f"{built} built")
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Mine candidate benchmark tasks from agent sessions and git history.

A candidate is a conventional commit (fix, feat, perf or refactor) that
changes both source and test files and was made inside the time window of a
Claude Code or pi session in the same repository. The record format is
documented in the internal benchmark design; one JSON object is written per
line to candidates.jsonl.

Usage:
    uv run python -m reagent_harbor.internal.mine [--out PATH]
        [--sessions-glob GLOB ...] [--repo-filter SUBSTR]
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_GLOBS = [
    "~/.claude/projects/*/*.jsonl",
    "~/.claude-swap-backup/sessions/*/projects/*/*.jsonl",
    "~/.pi/agent/sessions/*/*.jsonl",
]
SKIP_PREFIXES = (
    "<command-",
    "<local-command",
    "<system-reminder",
    "This session is being continued",
    "[Request interrupted",
    "<task-notification",
)
MIN_REQUEST_CHARS = 15
MAX_REQUEST_CHARS = 2000
GIT_TIMEOUT = 60

SUBJECT_RE = re.compile(r"^(fix|feat|perf|refactor)(\([^)]*\))?!?:")
TEST_RE = re.compile(r"(^|/)(tests?|spec)/|_test\.|test_|\.test\.|_spec\.")
CORRECTION_RE = re.compile(
    r"wrong|didn'?t|doesn'?t|not work|still|again|broken|fail|error|no,? i meant",
    re.IGNORECASE,
)
CHANNEL_RE = re.compile(r'^\s*channel\s*=\s*["\']([^"\']+)["\']', re.MULTILINE)


@dataclass
class Session:
    path: str
    cwd: str
    first: float
    last: float
    users: list[tuple[float, str]] = field(default_factory=list)


@dataclass
class Commit:
    sha: str
    parent: str
    time: int
    subject: str
    files: list[str]


def data_root() -> Path:
    env = os.environ.get("REAGENT_INTERNAL_BENCH")
    if env:
        return Path(env).expanduser()
    return Path.home() / ".local/share/reagent/internal-bench"


def parse_ts(value: object) -> float | None:
    if not isinstance(value, str):
        return None
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.timestamp()


def is_substantive(text: str) -> bool:
    text = text.strip()
    return len(text) >= MIN_REQUEST_CHARS and not text.startswith(SKIP_PREFIXES)


def claude_text(message: dict) -> str | None:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            return None
        parts = [
            b.get("text", "")
            for b in content
            if isinstance(b, dict) and b.get("type") == "text"
        ]
        return "\n".join(parts)
    return None


def pi_text(message: dict) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "\n".join(
        b.get("text", "")
        for b in content
        if isinstance(b, dict) and b.get("type") == "text"
    )


def read_session(path: str) -> Session | None:
    cwd = ""
    times: list[float] = []
    users: list[tuple[float, str]] = []
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(row, dict):
                    continue
                kind = row.get("type")
                ts = parse_ts(row.get("timestamp"))
                if kind == "session":
                    if not cwd and isinstance(row.get("cwd"), str):
                        cwd = row["cwd"]
                    if ts is not None:
                        times.append(ts)
                    continue
                message = row.get("message")
                if not isinstance(message, dict):
                    continue
                if kind in ("user", "assistant"):
                    if not cwd and isinstance(row.get("cwd"), str):
                        cwd = row["cwd"]
                    if ts is not None:
                        times.append(ts)
                    if (
                        kind == "user"
                        and ts is not None
                        and not row.get("isMeta")
                        and not row.get("isSidechain")
                    ):
                        text = claude_text(message)
                        if text and is_substantive(text):
                            users.append((ts, text.strip()))
                elif kind == "message":
                    if ts is not None:
                        times.append(ts)
                    if message.get("role") == "user" and ts is not None:
                        text = pi_text(message)
                        if is_substantive(text):
                            users.append((ts, text.strip()))
    except OSError:
        return None
    if not cwd or not times:
        return None
    return Session(path, cwd, min(times), max(times), users)


def find_sessions(globs: list[str]) -> list[str]:
    paths: set[str] = set()
    for pattern in globs:
        for p in glob.glob(os.path.expanduser(pattern)):
            if "/subagents/" not in p:
                paths.add(p)
    return sorted(paths)


def git(repo: str, *args: str) -> str | None:
    try:
        out = subprocess.run(
            ["git", "-C", repo, *args],
            capture_output=True,
            timeout=GIT_TIMEOUT,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError):
        return None
    if out.returncode != 0:
        return None
    return out.stdout.decode("utf-8", errors="replace")


_toplevels: dict[str, str | None] = {}


def toplevel(cwd: str) -> str | None:
    if cwd not in _toplevels:
        top = None
        if os.path.isdir(cwd):
            out = git(cwd, "rev-parse", "--show-toplevel")
            top = out.strip() if out else None
        _toplevels[cwd] = top
    return _toplevels[cwd]


def commits_between(repo: str, first: float, last: float) -> list[Commit]:
    out = git(
        repo,
        "log",
        "--all",
        "--no-merges",
        "--no-renames",
        f"--since={int(first)}",
        f"--until={int(last) + 1}",
        "--name-only",
        "--format=%x1e%H%x1f%P%x1f%at%x1f%s",
    )
    if not out:
        return []
    commits = []
    for chunk in out.split("\x1e")[1:]:
        head, _, files = chunk.partition("\n")
        sha, parents, at, subject = head.split("\x1f", 3)
        parent = parents.split()
        if len(parent) != 1 or not at.isdigit():
            continue
        time = int(at)
        if not first <= time <= last:
            continue
        paths = [f for f in files.split("\n") if f]
        commits.append(Commit(sha, parent[0], time, subject, paths))
    return commits


def split_files(files: list[str]) -> tuple[list[str], list[str]]:
    src = [f for f in files if not TEST_RE.search(f)]
    tests = [f for f in files if TEST_RE.search(f)]
    return src, tests


def pick_request(users: list[tuple[float, str]], time: int) -> tuple[str, bool] | None:
    prior = [text for ts, text in users if ts <= time]
    if not prior:
        return None
    window = prior[-4:]
    correction = any(CORRECTION_RE.search(t) for t in window)
    return prior[-1][:MAX_REQUEST_CHARS], correction


def toolchain_at(repo: str, parent: str) -> str | None:
    for name in ("rust-toolchain.toml", "rust-toolchain"):
        text = git(repo, "show", f"{parent}:{name}")
        if text is None:
            continue
        m = CHANNEL_RE.search(text)
        if m:
            return m.group(1)
        plain = text.strip()
        if plain and "\n" not in plain and "=" not in plain and "[" not in plain:
            return plain
    return None


def crate_name(repo: str, fix: str, directory: str) -> str | None:
    rel = f"{directory}/Cargo.toml" if directory else "Cargo.toml"
    text = git(repo, "show", f"{fix}:{rel}")
    if text is None:
        return None
    try:
        name = tomllib.loads(text).get("package", {}).get("name")
    except tomllib.TOMLDecodeError:
        return None
    return name if isinstance(name, str) else None


def crates_for(repo: str, fix: str, files: list[str]) -> list[str]:
    names: set[str] = set()
    cache: dict[str, str | None] = {}
    for f in files:
        directory = os.path.dirname(f)
        while True:
            if directory not in cache:
                cache[directory] = crate_name(repo, fix, directory)
            if cache[directory]:
                names.add(cache[directory])
                break
            if not directory:
                break
            directory = os.path.dirname(directory)
    return sorted(names)


def lang_at(repo: str, parent: str) -> str:
    out = git(repo, "ls-tree", "--name-only", parent)
    names = set(out.split("\n")) if out else set()
    if "Cargo.toml" in names:
        return "rust"
    if "pyproject.toml" in names or "setup.py" in names:
        return "python"
    return "other"


def diff_lines(repo: str, parent: str, fix: str, files: list[str]) -> int:
    out = git(repo, "diff", "--numstat", "--no-renames", parent, fix, "--", *files)
    total = 0
    for line in (out or "").splitlines():
        added, removed, _ = line.split("\t", 2)
        if added.isdigit() and removed.isdigit():
            total += int(added) + int(removed)
    return total


def build_candidate(repo: str, session: Session, commit: Commit) -> dict | None:
    if not SUBJECT_RE.match(commit.subject):
        return None
    src, tests = split_files(commit.files)
    if not src or not tests:
        return None
    picked = pick_request(session.users, commit.time)
    if picked is None:
        return None
    request, correction = picked
    return {
        "id": f"{os.path.basename(repo)}-{commit.sha[:10]}",
        "repo": repo,
        "fix": commit.sha,
        "parent": commit.parent,
        "subject": commit.subject,
        "src_files": src,
        "test_files": tests,
        "lang": lang_at(repo, commit.parent),
        "toolchain": toolchain_at(repo, commit.parent),
        "crates": crates_for(repo, commit.sha, commit.files),
        "session": session.path,
        "request": request,
        "correction": correction,
        "diff_lines": diff_lines(repo, commit.parent, commit.sha, src),
    }


def mine(globs: list[str], repo_filter: str | None = None) -> list[dict]:
    """Return candidate records, one per fix commit.

    A commit covered by several sessions is attributed to the session whose
    time window is shortest.
    """
    best: dict[tuple[str, str], tuple[float, dict]] = {}
    for path in find_sessions(globs):
        session = read_session(path)
        if session is None:
            continue
        repo = toplevel(session.cwd)
        if repo is None or (repo_filter and repo_filter not in repo):
            continue
        width = session.last - session.first
        for commit in commits_between(repo, session.first, session.last):
            key = (repo, commit.sha)
            if key in best and best[key][0] <= width:
                continue
            cand = build_candidate(repo, session, commit)
            if cand is not None:
                best[key] = (width, cand)
    return sorted((c for _, c in best.values()), key=lambda c: c["id"])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="reagent_harbor.internal.mine")
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--sessions-glob", action="append", default=None)
    parser.add_argument("--repo-filter", default=None)
    args = parser.parse_args(argv)

    out = args.out or data_root() / "candidates.jsonl"
    rows = mine(args.sessions_glob or DEFAULT_GLOBS, args.repo_filter)
    out.parent.mkdir(parents=True, exist_ok=True)
    with open(out, "w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row) + "\n")
    print(f"wrote {len(rows)} candidates to {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

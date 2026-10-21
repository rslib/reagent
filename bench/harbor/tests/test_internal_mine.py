import json
import os
import subprocess
from pathlib import Path

from reagent_harbor.internal import mine


def git(repo: Path, *args: str, when: str | None = None) -> str:
    env = dict(os.environ)
    env.update(
        GIT_AUTHOR_NAME="t",
        GIT_AUTHOR_EMAIL="t@t",
        GIT_COMMITTER_NAME="t",
        GIT_COMMITTER_EMAIL="t@t",
    )
    if when:
        env["GIT_AUTHOR_DATE"] = when
        env["GIT_COMMITTER_DATE"] = when
    out = subprocess.run(
        ["git", "-c", "commit.gpgsign=false", *args],
        cwd=repo,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def write(repo: Path, rel: str, text: str) -> None:
    p = repo / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)


def commit(repo: Path, msg: str, when: str) -> str:
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", msg, when=when)
    return git(repo, "rev-parse", "HEAD")


def make_repo(root: Path) -> dict[str, str]:
    repo = root / "demo-proj"
    repo.mkdir()
    git(repo, "init", "-q")
    write(repo, "rust-toolchain.toml", '[toolchain]\nchannel = "1.98"\n')
    write(repo, "Cargo.toml", '[package]\nname = "demo"\nversion = "0.1.0"\n')
    write(repo, "src/lib.rs", "pub fn a() {}\n")
    shas = {"init": commit(repo, "chore: init", "2026-01-01T09:30:00+0000")}
    write(repo, "src/lib.rs", "pub fn a() { 1; }\npub fn b() {}\n")
    write(repo, "tests/it.rs", "#[test]\nfn t() {}\n")
    shas["fix"] = commit(repo, "fix: handle b", "2026-01-01T11:00:00+0000")
    write(repo, "README.md", "docs\n")
    shas["docs"] = commit(repo, "docs: readme", "2026-01-01T11:10:00+0000")
    write(repo, "src/lib.rs", "pub fn a() { 1; }\npub fn b() {}\npub fn c() {}\n")
    shas["feat"] = commit(repo, "feat: add c", "2026-01-01T11:20:00+0000")
    shas["parent"] = git(repo, "rev-parse", shas["fix"] + "^")
    return shas


def jl(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def claude_user(ts: str, content, cwd: str, **extra) -> dict:
    return {
        "type": "user",
        "timestamp": ts,
        "cwd": cwd,
        "message": {"role": "user", "content": content},
        **extra,
    }


def test_mine_end_to_end(tmp_path: Path) -> None:
    shas = make_repo(tmp_path)
    repo = str((tmp_path / "demo-proj").resolve())

    claude = tmp_path / "claude" / "proj" / "s1.jsonl"
    jl(
        claude,
        [
            claude_user("2026-01-01T10:00:00.000Z", "Please add a parser for foo files", repo),
            {
                "type": "assistant",
                "timestamp": "2026-01-01T10:01:00.000Z",
                "cwd": repo,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
            },
            claude_user("2026-01-01T10:20:00.000Z", "No, that is wrong, it still doesn't work", repo),
            claude_user("2026-01-01T10:30:00.000Z", "<system-reminder>ignore me, long enough text</system-reminder>", repo),
            claude_user("2026-01-01T10:31:00.000Z", [{"type": "text", "text": "[Request interrupted by user]"}], repo),
            claude_user("2026-01-01T10:32:00.000Z", "<command-name>/clear</command-name> padded padded", repo),
            claude_user("2026-01-01T10:33:00.000Z", [{"type": "tool_result", "content": "x" * 50}], repo),
            claude_user("2026-01-01T10:34:00.000Z", "meta message that is long enough", repo, isMeta=True),
            claude_user("2026-01-01T10:35:00.000Z", "ok", repo),
            claude_user("2026-01-01T11:50:00.000Z", "A later unrelated request after the fix", repo),
        ],
    )
    sub = tmp_path / "claude" / "proj" / "subagents" / "x.jsonl"
    jl(sub, [claude_user("2026-01-01T10:00:00.000Z", "subagent message here, long", repo)])

    pi = tmp_path / "pi" / "dir" / "p1.jsonl"
    jl(
        pi,
        [
            {"type": "session", "cwd": repo, "timestamp": "2026-01-01T09:00:00.000Z"},
            {
                "type": "message",
                "timestamp": "2026-01-01T09:10:00.000Z",
                "message": {"role": "user", "content": [{"type": "text", "text": "pi request that is long enough"}]},
            },
            {
                "type": "message",
                "timestamp": "2026-01-01T13:00:00.000Z",
                "message": {"role": "toolResult", "content": [{"type": "text", "text": "out"}]},
            },
        ],
    )

    out = tmp_path / "out" / "candidates.jsonl"
    rc = mine.main(
        [
            "--out", str(out),
            "--sessions-glob", str(tmp_path / "claude" / "*" / "*.jsonl"),
            "--sessions-glob", str(tmp_path / "pi" / "*" / "*.jsonl"),
        ]
    )
    assert rc == 0
    rows = [json.loads(line) for line in out.read_text().splitlines()]
    assert len(rows) == 1
    c = rows[0]
    assert c["id"] == "demo-proj-" + shas["fix"][:10]
    assert c["repo"] == repo
    assert c["fix"] == shas["fix"]
    assert c["parent"] == shas["parent"]
    assert c["subject"] == "fix: handle b"
    assert c["src_files"] == ["src/lib.rs"]
    assert c["test_files"] == ["tests/it.rs"]
    assert c["lang"] == "rust"
    assert c["toolchain"] == "1.98"
    assert c["crates"] == ["demo"]
    assert c["session"] == str(claude)
    assert c["request"] == "No, that is wrong, it still doesn't work"
    assert c["correction"] is True
    assert c["diff_lines"] == 3


def test_repo_filter_excludes(tmp_path: Path) -> None:
    make_repo(tmp_path)
    repo = str((tmp_path / "demo-proj").resolve())
    claude = tmp_path / "c" / "p" / "s.jsonl"
    jl(
        claude,
        [
            claude_user("2026-01-01T10:00:00.000Z", "Please fix the thing in b", repo),
            claude_user("2026-01-01T12:00:00.000Z", "Another long enough message", repo),
        ],
    )
    out = tmp_path / "o.jsonl"
    args = ["--out", str(out), "--sessions-glob", str(tmp_path / "c" / "*" / "*.jsonl")]
    assert mine.main([*args, "--repo-filter", "nomatch"]) == 0
    assert out.read_text() == ""
    assert mine.main([*args, "--repo-filter", "demo"]) == 0
    row = json.loads(out.read_text().splitlines()[0])
    assert row["correction"] is False
    assert row["request"] == "Please fix the thing in b"


def test_missing_repo_is_skipped(tmp_path: Path) -> None:
    s = tmp_path / "c" / "p" / "s.jsonl"
    jl(s, [claude_user("2026-01-01T10:00:00.000Z", "Please fix it, long enough", str(tmp_path / "gone"))])
    out = tmp_path / "o.jsonl"
    assert mine.main(["--out", str(out), "--sessions-glob", str(s)]) == 0
    assert out.read_text() == ""

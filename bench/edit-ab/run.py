#!/usr/bin/env python3
"""Run reagent on the edit-ab tasks as one named arm and record the results.

Run it with: uv run --no-project bench/edit-ab/run.py --model <m> --arm <name> ... [-- <reagent flags>]
Standard library only.
"""

import argparse
import concurrent.futures
import fnmatch
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SETS = {"small": os.path.join(HERE, "tasks"), "hard": os.path.join(HERE, "hard")}
# Files the agent must not see in its copy of the task.
HIDDEN = {"solution.patch", "TASK.md"}
DIFF_SKIP = ["target", "__pycache__", ".git", "Cargo.lock", "node_modules"]

lock = threading.Lock()


def parse_events(path):
    """Read a reagent --json stdout file, skipping lines that are not JSON."""
    events = []
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line.startswith("{"):
                    continue
                try:
                    events.append(json.loads(line))
                except ValueError:
                    continue
    except OSError:
        pass
    return events


def metrics(events):
    """Turn the event lines into the numbers the summary needs."""
    names = {}
    tools = {}
    edit_errors = 0
    turn_starts = 0
    summary = {}
    for event in events:
        kind = event.get("type")
        if kind == "turn_start":
            turn_starts += 1
        elif kind == "tool_start":
            names[event.get("id")] = event.get("name")
            tools[event.get("name")] = tools.get(event.get("name"), 0) + 1
        elif kind == "tool_result":
            if event.get("is_error") and names.get(event.get("id")) == "edit":
                edit_errors += 1
        elif kind == "summary":
            summary = event
    return {
        "turns": summary.get("turns", turn_starts),
        "input": summary.get("input", 0),
        "output": summary.get("output", 0),
        "cache_read": summary.get("cache_read", 0),
        "cost": summary.get("cost") or 0.0,
        "exit_reason": summary.get("exit"),
        "tools": tools,
        "edit_calls": tools.get("edit", 0),
        "edit_errors": edit_errors,
        "read_calls": tools.get("read", 0),
    }


def copy_task(src, dest):
    for name in os.listdir(src):
        if name in HIDDEN:
            continue
        path = os.path.join(src, name)
        if os.path.isdir(path):
            shutil.copytree(path, os.path.join(dest, name))
        else:
            shutil.copy2(path, os.path.join(dest, name))


def run_one(task, args):
    src = os.path.join(SETS[args.set], task)
    stem = "%s.%s" % (task, args.arm)
    tmp = tempfile.mkdtemp(prefix="edit-ab-%s-" % stem)
    work = os.path.join(tmp, "work")
    os.makedirs(work)
    copy_task(src, work)
    subprocess.run(["git", "init", "-q"], cwd=work, check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with open(os.path.join(src, "TASK.md"), encoding="utf-8") as handle:
        prompt = handle.read().strip()

    cmd = [args.reagent, "--json", "--model", args.model,
           "--reasoning", args.reasoning, "--deadline", str(args.timeout),
           "--cwd", work]
    if args.provider:
        cmd += ["--provider", args.provider]
    cmd += args.extra + ["-p", prompt]

    out_path = os.path.join(args.out, stem + ".jsonl")
    err_path = os.path.join(args.out, stem + ".err")
    started = time.time()
    rc = None
    timed_out = False
    with open(out_path, "wb") as out, open(err_path, "wb") as err:
        try:
            proc = subprocess.run(cmd, stdout=out, stderr=err, stdin=subprocess.DEVNULL,
                                  timeout=args.timeout + 120)
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            timed_out = True
        except OSError as error:
            err.write(("cannot start reagent: %s\n" % error).encode())
    wall = time.time() - started

    try:
        check = subprocess.run(["bash", "check.sh"], cwd=work, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               timeout=600)
        check_rc = check.returncode
        check_out = check.stdout
    except subprocess.TimeoutExpired as error:
        check_rc = 124
        check_out = error.stdout or b""
    with open(os.path.join(args.out, stem + ".check.txt"), "wb") as handle:
        handle.write(check_out)

    diff_cmd = ["diff", "-ruN"]
    for skip in DIFF_SKIP:
        diff_cmd += ["-x", skip]
    original = os.path.join(tmp, "original")
    os.makedirs(original)
    copy_task(src, original)
    diff = subprocess.run(diff_cmd + ["original", "work"], cwd=tmp,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    with open(os.path.join(args.out, stem + ".diff"), "wb") as handle:
        handle.write(diff.stdout)

    record = {
        "task": task,
        "set": args.set,
        "lang": task.rsplit("-", 1)[0].split("-")[-1],
        "arm": args.arm,
        "flags": args.extra,
        "model": args.model,
        "reasoning": args.reasoning,
        "pass": check_rc == 0,
        "check_rc": check_rc,
        "reagent_rc": rc,
        "timed_out": timed_out,
        "wall_s": round(wall, 2),
        "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    record.update(metrics(parse_events(out_path)))
    with lock:
        with open(os.path.join(args.out, "results.jsonl"), "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")
        print("%-8s %-8s pass=%-5s turns=%s edits=%s edit_errors=%s wall=%.0fs" % (
            task, args.arm, record["pass"], record["turns"], record["edit_calls"],
            record["edit_errors"], wall), flush=True)
    if not args.keep:
        shutil.rmtree(tmp, ignore_errors=True)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True)
    parser.add_argument("--arm", required=True,
                        help="name of this arm; the words after -- go to reagent")
    parser.add_argument("--set", choices=sorted(SETS), default="small",
                        help="task set: small (tasks/) or hard (hard/)")
    parser.add_argument("--tasks", default="*", help="glob over task names, for example 'py-*'")
    parser.add_argument("--out", required=True, help="directory for the run files")
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--reasoning", default="off")
    parser.add_argument("--timeout", type=int, default=600, help="seconds per task")
    parser.add_argument("--reagent", default="reagent", help="path to the reagent binary")
    parser.add_argument("--provider", default=None,
                        help="passed to reagent as --provider, for example fake:<script>")
    parser.add_argument("--keep", action="store_true", help="keep the temp directories")
    argv = sys.argv[1:]
    cut = argv.index("--") if "--" in argv else len(argv)
    args = parser.parse_args(argv[:cut])
    args.extra = argv[cut + 1:]
    if not args.arm.replace("-", "").replace("_", "").isalnum():
        sys.exit("--arm must be letters, digits, - or _")

    root = SETS[args.set]
    tasks = sorted(t for t in os.listdir(root) if fnmatch.fnmatch(t, args.tasks)
                   and os.path.isdir(os.path.join(root, t)))
    if not tasks:
        sys.exit("no task matches %r" % args.tasks)
    os.makedirs(args.out, exist_ok=True)
    results = os.path.join(args.out, "results.jsonl")
    if os.path.exists(results):
        with open(results, encoding="utf-8") as handle:
            seen = {(r.get("set", "small"), r["task"], r.get("arm") or r["mode"])
                    for r in map(json.loads, filter(str.strip, handle))}
        clash = [t for t in tasks if (args.set, t, args.arm) in seen]
        if clash:
            sys.exit("%s already has results for %s; use a fresh --out, runs must be fresh"
                     % (args.out, ", ".join(clash)))

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(run_one, t, args) for t in tasks]
        for future in futures:
            future.result()
    print("wrote %s" % results)


if __name__ == "__main__":
    main()

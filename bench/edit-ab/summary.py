#!/usr/bin/env python3
"""Summarize edit-ab results: two arms side by side, paired by task.

Run it with: uv run --no-project bench/edit-ab/summary.py <results.jsonl>... [--arms A B] [--json]
Standard library only.
"""

import argparse
import json
import math
import sys

def load(paths):
    rows = []
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                if line.strip():
                    rows.append(json.loads(line))
    return rows


def sign_test(only_a, only_b):
    """Two-sided exact sign test on the discordant pairs."""
    n = only_a + only_b
    if n == 0:
        return 1.0
    k = min(only_a, only_b)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def mean(values):
    return sum(values) / len(values) if values else 0.0


def stats(runs):
    edit_calls = sum(r.get("edit_calls", 0) for r in runs)
    edit_errors = sum(r.get("edit_errors", 0) for r in runs)
    return {
        "n": len(runs),
        "passed": sum(1 for r in runs if r.get("pass")),
        "turns": mean([r.get("turns", 0) for r in runs]),
        "edit_calls": mean([r.get("edit_calls", 0) for r in runs]),
        "edit_error_rate": edit_errors / edit_calls if edit_calls else 0.0,
        "input": mean([r.get("input", 0) for r in runs]),
        "output": mean([r.get("output", 0) for r in runs]),
        "cost": sum(r.get("cost") or 0.0 for r in runs),
        "wall_s": mean([r.get("wall_s", 0.0) for r in runs]),
    }


def arm(row):
    # Runs from before the arms were named hold the edit mode instead.
    return row.get("arm") or row["mode"]


def summarize(rows, arms=None):
    groups = {}
    for row in rows:
        key = "%s (reasoning %s)" % (row["model"], row.get("reasoning", "?"))
        # A later run of the same task and arm replaces an earlier one.
        groups.setdefault(key, {}).setdefault(arm(row), {})[row["task"]] = row
    out = {}
    for key, by_arm in sorted(groups.items()):
        names = list(arms) if arms else sorted(by_arm)
        if len(names) != 2:
            sys.exit("%s has arms %s; pick two with --arms" % (key, ", ".join(sorted(by_arm))))
        a, b = (by_arm.get(name, {}) for name in names)
        paired = sorted(set(a) & set(b))
        result = {"arms": names, "paired_tasks": len(paired),
                  "unpaired": sorted((set(a) | set(b)) - set(paired))}
        for name, runs in zip(names, (a, b)):
            result[name] = stats([runs[t] for t in paired])
        differ = []
        only_a = only_b = 0
        for task in paired:
            pa, pb = a[task]["pass"], b[task]["pass"]
            if pa != pb:
                differ.append({"task": task, names[0]: pa, names[1]: pb})
                if pa:
                    only_a += 1
                else:
                    only_b += 1
        result["differ"] = differ
        result["only_passes"] = {names[0]: only_a, names[1]: only_b}
        result["sign_test_p"] = sign_test(only_a, only_b)
        out[key] = result
    return out


ROWS = [
    ("pass rate", lambda s: "%d/%d" % (s["passed"], s["n"])),
    ("mean turns", lambda s: "%.2f" % s["turns"]),
    ("mean edit calls", lambda s: "%.2f" % s["edit_calls"]),
    ("edit error rate", lambda s: "%.1f%%" % (100 * s["edit_error_rate"])),
    ("mean input tokens", lambda s: "%.0f" % s["input"]),
    ("mean output tokens", lambda s: "%.0f" % s["output"]),
    ("total cost (USD)", lambda s: "%.4f" % s["cost"]),
    ("mean wall (s)", lambda s: "%.1f" % s["wall_s"]),
]


def render(summary):
    lines = []
    for key, result in summary.items():
        first, second = result["arms"]
        lines.append("model: %s, %d paired tasks" % (key, result["paired_tasks"]))
        if result["unpaired"]:
            lines.append("  left out, only one arm ran: %s" % ", ".join(result["unpaired"]))
        width = max(len(name) for name, _ in ROWS)
        lines.append("  %-*s  %12s  %12s" % (width, "", first, second))
        for name, fmt in ROWS:
            lines.append("  %-*s  %12s  %12s" % (width, name, fmt(result[first]), fmt(result[second])))
        lines.append("")
        if result["differ"]:
            lines.append("  tasks where the arms differ:")
            lines.append("    %-8s  %-13s  %s" % ("task", first + " pass?", second + " pass?"))
            for d in result["differ"]:
                lines.append("    %-8s  %-13s  %s" % (d["task"], "yes" if d[first] else "no",
                                                      "yes" if d[second] else "no"))
        else:
            lines.append("  no task differs in pass/fail")
        only = result["only_passes"]
        lines.append("  only %s passes: %d, only %s passes: %d, sign test p = %.4f"
                     % (first, only[first], second, only[second], result["sign_test_p"]))
        lines.append("")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", nargs="+", help="one or more results.jsonl files")
    parser.add_argument("--arms", nargs=2, metavar=("A", "B"),
                        help="the two arms to compare; default: the only two in the results")
    parser.add_argument("--json", action="store_true", help="print JSON instead of tables")
    args = parser.parse_args()
    summary = summarize(load(args.results), args.arms)
    if not summary:
        sys.exit("no results")
    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(render(summary))


if __name__ == "__main__":
    main()

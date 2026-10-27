#!/usr/bin/env bash
#
# The syscall gate: builds the release binary with the scripted fake provider,
# runs fixed reagent scenarios under strace, counts the syscalls of reagent's
# own process and threads, and checks them against bench/syscall-budget.txt.
#
# Usage:
#   bench/syscalls.sh                build, run every scenario once and check
#                                    the budget
#   bench/syscalls.sh --measure [N]  build, run every scenario N times (default
#                                    10), and print a budget file where each
#                                    line is the highest count times 1.15,
#                                    rounded up, and at least 24 over it
#   bench/syscalls.sh --self-test    build, run the gate, check that a budget a
#                                    count under a count fails, and check the
#                                    thread filter on
#                                    bench/syscalls/filter-fixture.log
#
# Every scenario is a fake-provider script in bench/syscalls/<name>.json. Its
# run gets a fresh temporary work folder filled with fixed files, a fresh
# empty XDG_CONFIG_HOME (plus an mcp.toml for the mcp scenario), and stdin from
# /dev/null and stdout to /dev/null. `strace -f` follows the threads and the
# children; only the root process and the threads it makes with CLONE_THREAD
# are counted, so `bash`, the MCP server and posix_spawn children stay out.
#
# A check prints one `scenario group count budget pct` row per line of the
# budget file. The groups are io, wait, fs, mem, proc and other, plus a total;
# `other` holds every syscall no group names.
#
# bench/syscall-budget.txt holds one `<scenario> <group> <max>` line per row. A
# line starting with `#` is a comment. It is for linux-x86_64 only: the counts
# are those of the Linux build, so a budget means nothing on another OS or
# architecture.
#
# Requirements: Linux with strace (on Debian: apt install strace). On another
# OS, or without strace, a gate run and --measure exit 2, and --self-test
# prints that the run is skipped and exits 0.
#
# Exit: 0 within budget, 1 over budget, 2 for bad usage, another OS, no
# strace, a scenario that does not exit 0, or a scenario, group or budget line
# that the budget file lacks or cannot read.
#
# The build goes to CARGO_TARGET_DIR (default
# $HOME/.cache/cargo-target/syscalls) and CARGO_BUILD_BUILD_DIR (default
# $HOME/.cache/cargo-target/syscalls-build); export them to point it elsewhere.

set -euo pipefail

# Relative paths below and the cargo build start from the repository root.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

BUDGET_FILE=${SYSCALL_BUDGET:-bench/syscall-budget.txt}
SCENARIO_DIR=bench/syscalls
FILTER_FIXTURE=$SCENARIO_DIR/filter-fixture.log

BINARY=reagent
FAKE_MCP=reagent-mcp-fake
# The package the fake MCP server binary belongs to.
MCP_PACKAGE=reagent-mcp
BUILD_FEATURES=testing

# The prompt the scenarios run, and the switch that turns the codemode tool on.
TASK=task
CODEMODE_TOOL=codemode

# The MCP server the mcp scenario starts and the tool it calls. The request
# offers it as a tool of its own (`direct = ["echo"]`), under the name
# `mcp__demo__echo` that bench/syscalls/mcp.json calls.
MCP_SERVER=demo
MCP_TOOL='echo'

# What the fake MCP server offers: `echo` answers with its arguments and
# `sleep` waits. The client passes its own environment to the server, so the
# server reads it from here and mcp.toml needs no env table.
export MCP_FAKE='{"tools": [{"name": "echo"}, {"name": "sleep"}]}'
# A key the run never uses: the scripted provider answers every request.
TEST_API_KEY=sk-ant-test-syscalls

# The scenarios, in run order, one script each in $SCENARIO_DIR.
SCENARIOS=(turn tools bash codemode mcp)
SCENARIO_LIST=${SCENARIOS[*]}

# The syscall groups, in print order, and the syscalls each one holds.
# `GROUPS` is a bash special variable, so the name is long.
SYSCALL_GROUPS=(io wait fs mem proc other total)
SYSCALL_GROUP_LIST=${SYSCALL_GROUPS[*]}
IO_SYSCALLS="read write readv writev pread64 pwrite64 sendto recvfrom sendmsg recvmsg"
WAIT_SYSCALLS="futex epoll_wait epoll_pwait epoll_pwait2 poll ppoll select pselect6 nanosleep clock_nanosleep sched_yield"
FS_SYSCALLS="openat open close statx newfstatat fstat lstat stat getdents64 readlink readlinkat access faccessat2 lseek ioctl fcntl unlink unlinkat rename renameat renameat2 mkdir mkdirat"
MEM_SYSCALLS="mmap munmap mprotect madvise brk mremap"
PROC_SYSCALLS="clone clone3 execve wait4 waitid pipe2 kill prctl rt_sigaction rt_sigprocmask sigaltstack set_robust_list rseq"

# The counts the thread filter has to give on $FILTER_FIXTURE. The root makes
# one thread with CLONE_THREAD, split over an unfinished and a resumed line
# with a line of the thread between them, and one child process without it; the child's execve, close and write stay
# out, so the total is 8 of the 11 syscall lines in the file.
FILTER_EXPECTED='io 2
wait 1
fs 1
mem 1
proc 3
other 0
total 8'

# How a --measure count becomes a budget line: the highest count times this
# factor, rounded up, and at least this many syscalls over the highest count.
# One more tokio blocking-pool thread costs about 11 syscalls, more than 15%
# of a small group, so the slack holds two of them.
BUDGET_FACTOR=1.15
BUDGET_SLACK=24
MEASURE_RUNS=10

# The widths of the check table, in characters.
SCENARIO_WIDTH=8
GROUP_WIDTH=6
COUNT_WIDTH=8
PCT_WIDTH=6

STATUS_OVER=1
STATUS_USAGE=2

# How many lines of a failed scenario's strace log are printed: the log itself
# goes with the temporary folder.
FAILURE_LOG_LINES=20

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target/syscalls}"
export CARGO_BUILD_BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-$HOME/.cache/cargo-target/syscalls-build}"
export NO_COLOR=1
# The user's profile must not change a scenario.
unset REAGENT_PROFILE

say() { printf 'bench/syscalls.sh: %s\n' "$*" >&2; }
die() {
    say "$*"
    exit "$STATUS_USAGE"
}

# The temporary folder of this run, removed by the EXIT trap.
TMP_ROOT=""
cleanup() {
    if [[ -n $TMP_ROOT ]]; then
        rm -rf "$TMP_ROOT"
    fi
}
trap cleanup EXIT

make_tmp() {
    TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/reagent-syscalls.XXXXXX")
}

# Whether this machine can count syscalls: Linux with strace.
can_run() {
    [[ $(uname -s) == Linux ]] && command -v strace >/dev/null 2>&1
}

# Prints that the run cannot happen here and leaves with `$1`.
skip() {
    say "the syscall run is skipped: it needs Linux with strace (on Debian: apt install strace)"
    exit "$1"
}

# The head of the budget file: what it holds and what made it.
BUDGET_HEADER='# Syscall budgets for the fixed reagent runs of bench/syscalls.sh, one line
# per scenario and group: <scenario> <group> <max>.
#
# linux-x86_64 only: the counts are those of the Linux build of reagent, so a
# budget means nothing on another OS or architecture.
#
# Each line is, per scenario and group, the highest count of N runs times 1.15,
# rounded up and at least 24 over that count, from bench/syscalls.sh
# --measure N. Use at least the
# default ten: the threads a run makes with the tokio blocking pool vary
# with timing, and a small sample can miss the high count, which a later run
# would then be over.
#
# The groups are io, wait, fs, mem, proc and other, plus a total. The other
# group holds every syscall no group names.
'
budget_header() { printf '%s' "$BUDGET_HEADER"; }

# Builds the binary the scenarios run and the fake MCP server they start.
build() {
    say "building $BINARY with the $BUILD_FEATURES feature, and $FAKE_MCP"
    cargo build --release -p reagent --features "$BUILD_FEATURES"
    cargo build --release -p "$MCP_PACKAGE" --bin "$FAKE_MCP"
    BIN=$CARGO_TARGET_DIR/release/$BINARY
    MCP_BIN=$CARGO_TARGET_DIR/release/$FAKE_MCP
    [[ -x $BIN ]] || die "no binary at $BIN; the build did not write it"
    [[ -x $MCP_BIN ]] || die "no fake MCP server at $MCP_BIN; the build did not write it"
}

# Counts the syscalls of the root process and its threads in the strace log
# `$1`, printing `<group> <count>` for each group in SYSCALL_GROUPS.
#
# The first pid in the log is the root. A clone or clone3 whose flags hold
# CLONE_THREAD makes its returned pid a thread of the caller; `-f` may cut that
# line in two, so the tid is read from its resumed line. A new thread can run
# before strace writes that resumed line, so a first pass over the log finds
# every thread and a second pass counts. A syscall counts once, at its first
# line: a resumed line, a `+++` exit line and a `---` signal line add nothing.
count_log() {
    awk -v io="$IO_SYSCALLS" \
        -v wait="$WAIT_SYSCALLS" \
        -v fs="$FS_SYSCALLS" \
        -v mem="$MEM_SYSCALLS" \
        -v proc="$PROC_SYSCALLS" \
        -v groups="$SYSCALL_GROUP_LIST" '
        # The pid a clone line returns, or the empty string.
        function returned_pid() {
            if ($(NF - 1) != "=") return ""
            if ($NF !~ /^[0-9]+$/) return ""
            return $NF
        }
        # Marks `pid` and every thread it made, at any depth, as counted.
        function track(pid,   child) {
            if (pid in tracked) return
            tracked[pid] = 1
            for (child in made) if (made[child] == pid) track(child)
        }
        BEGIN {
            n = split(io, names, " ")
            for (i = 1; i <= n; i++) group_of[names[i]] = "io"
            n = split(wait, names, " ")
            for (i = 1; i <= n; i++) group_of[names[i]] = "wait"
            n = split(fs, names, " ")
            for (i = 1; i <= n; i++) group_of[names[i]] = "fs"
            n = split(mem, names, " ")
            for (i = 1; i <= n; i++) group_of[names[i]] = "mem"
            n = split(proc, names, " ")
            for (i = 1; i <= n; i++) group_of[names[i]] = "proc"
            ngroups = split(groups, order, " ")
            root = ""
        }
        $1 !~ /^[0-9]+$/ || NF < 2 { next }
        # The first pass: which pid made which thread.
        FNR == NR {
            pid = $1
            if (root == "") root = pid
            if ($2 == "<...") {
                if (($3 == "clone" || $3 == "clone3") && (pid in waiting)) {
                    child = returned_pid()
                    if (child != "") made[child] = pid
                    delete waiting[pid]
                }
                next
            }
            name = $2
            sub(/\(.*$/, "", name)
            if ((name == "clone" || name == "clone3") && index($0, "CLONE_THREAD") > 0) {
                if ($0 ~ /<unfinished \.\.\.>$/) {
                    waiting[pid] = 1
                    next
                }
                child = returned_pid()
                if (child != "") made[child] = pid
            }
            next
        }
        # The second pass: count the lines of the root and its threads.
        !closed {
            if (root != "") track(root)
            closed = 1
        }
        {
            if (!($1 in tracked)) next
            if ($2 == "+++" || $2 == "---" || $2 == "<...") next
            name = $2
            sub(/\(.*$/, "", name)
            if (name == "") next
            count[(name in group_of) ? group_of[name] : "other"]++
            count["total"]++
        }
        END {
            for (i = 1; i <= ngroups; i++) printf "%s %d\n", order[i], count[order[i]]
        }
    ' "$1" "$1"
}

# Fills the work folder `$1` with the fixed files every scenario sees.
fill_work() {
    local work=$1
    mkdir -p "$work/sub"
    printf '%s\n' 'alpha' 'beta' 'gamma' >"$work/a.txt"
    printf '%s\n' 'delta' 'epsilon' >"$work/b.txt"
    printf '%s\n' 'zeta' 'eta' >"$work/sub/c.txt"
}

# Writes what the scenario `$1` needs into its fresh config home `$2`.
fill_config() {
    local name=$1 config=$2
    case $name in
    mcp)
        mkdir -p "$config/reagent"
        printf '[servers.%s]\ncommand = "%s"\ndirect = ["%s"]\n' \
            "$MCP_SERVER" "$MCP_BIN" "$MCP_TOOL" >"$config/reagent/mcp.toml"
        ;;
    esac
}

# Runs the scenario `$1` once under strace and prints its `<scenario> <group>
# <count>` lines.
run_scenario() {
    local name=$1
    local script=$SCENARIO_DIR/$name.json
    [[ -f $script ]] || die "no scenario script at $script"
    local work config log status=0
    work=$(mktemp -d "$TMP_ROOT/work-$name.XXXXXX")
    config=$(mktemp -d "$TMP_ROOT/config-$name.XXXXXX")
    log=$(mktemp "$TMP_ROOT/strace-$name.XXXXXX")
    fill_work "$work"
    fill_config "$name" "$config"
    # Only the codemode scenario needs a tool list of its own.
    local -a args=()
    if [[ $name == codemode ]]; then
        args=(--tools "+$CODEMODE_TOOL")
    fi
    XDG_CONFIG_HOME=$config \
        XDG_STATE_HOME=$config/state \
        XDG_CACHE_HOME=$config/cache \
        ANTHROPIC_API_KEY=$TEST_API_KEY \
        strace -f -qq -o "$log" -- "$BIN" -p "$TASK" --json \
        --provider "fake:$script" --cwd "$work" ${args[@]+"${args[@]}"} \
        </dev/null >/dev/null || status=$?
    if ((status != 0)); then
        say "scenario $name exited $status, and every scenario has to exit 0; the end of its strace log:"
        tail -n "$FAILURE_LOG_LINES" "$log" >&2 || true
        exit "$STATUS_USAGE"
    fi
    count_log "$log" | awk -v name="$name" '{ print name, $0 }'
}

# Runs every scenario once and writes `<scenario> <group> <count>` lines to
# `$1`.
run_all() {
    local out=$1 name
    : >"$out"
    for name in "${SCENARIOS[@]}"; do
        say "running scenario $name"
        run_scenario "$name" >>"$out"
    done
}

# Checks the counts `$2` against the budget `$1`: one row per scenario, group,
# count, budget and percentage. Returns 0 within budget, 1 over budget, and 2
# for a scenario or group without a line or for a malformed line.
check() {
    local budget=$1 counts=$2 status=0
    awk -v budget_file="$budget" \
        -v scenario_width="$SCENARIO_WIDTH" \
        -v group_width="$GROUP_WIDTH" \
        -v count_width="$COUNT_WIDTH" \
        -v pct_width="$PCT_WIDTH" '
        # Reads the budget file into `max`, keyed by scenario and group.
        function read_budget(   line, n, field) {
            while ((getline line < budget_file) > 0) {
                if (line ~ /^[ \t]*#/ || line ~ /^[ \t]*$/) continue
                sub(/^[ \t]+/, "", line)
                sub(/[ \t]+$/, "", line)
                gsub(/[ \t]+/, " ", line)
                n = split(line, field, " ")
                if (n != 3) {
                    printf "bench/syscalls.sh: %s: `%s` is not `<scenario> <group> <max>`\n", budget_file, line > "/dev/stderr"
                    fatal = 1
                    continue
                }
                max[field[1] " " field[2]] = field[3]
            }
        }
        BEGIN {
            header = "%-" scenario_width "s %-" group_width "s %" count_width "s %" count_width "s %" pct_width "s\n"
            row = "%-" scenario_width "s %-" group_width "s %" count_width "d %" count_width "d %" pct_width ".1f\n"
            printf header, "scenario", "group", "count", "budget", "pct"
            read_budget()
            if (fatal) exit 2
        }
        {
            scenario = $1
            group = $2
            count = $3 + 0
            key = scenario " " group
            if (!(key in max)) {
                printf "bench/syscalls.sh: %s %s has no line in %s\n", scenario, group, budget_file > "/dev/stderr"
                missing = 1
                next
            }
            budget = max[key] + 0
            pct = (budget > 0) ? count / budget * 100 : 0
            printf row, scenario, group, count, budget, pct
            if (count > budget) {
                printf "bench/syscalls.sh: %s %s %d is over its budget %d\n", scenario, group, count, budget > "/dev/stderr"
                over = 1
            }
        }
        END {
            if (missing) exit 2
            if (over) exit 1
        }
    ' "$counts" || status=$?
    return "$status"
}

# Prints a budget file from the counts `$1` of the `$2` runs each.
budget_from_counts() {
    budget_header
    printf '# made from %s runs of every scenario.\n' "$2"
    awk -v scenarios="$SCENARIO_LIST" \
        -v groups="$SYSCALL_GROUP_LIST" \
        -v factor="$BUDGET_FACTOR" \
        -v slack="$BUDGET_SLACK" '
        {
            key = $1 " " $2
            value = $3 + 0
            if (!(key in highest) || value > highest[key]) highest[key] = value
        }
        END {
            n = split(scenarios, scenario_order, " ")
            m = split(groups, group_order, " ")
            for (i = 1; i <= n; i++) {
                for (j = 1; j <= m; j++) {
                    key = scenario_order[i] " " group_order[j]
                    if (!(key in highest)) continue
                    scaled = highest[key] * factor
                    value = int(scaled)
                    if (value < scaled) value++
                    if (value < highest[key] + slack) value = highest[key] + slack
                    printf "%s %s %d\n", scenario_order[i], group_order[j], value
                }
            }
        }
    ' "$1"
}

# Prints a copy of the budget `$1` where the first counted row of `$2` has a
# budget one under its count, so a check of it has to fail.
lower_one() {
    awk -v counts="$2" '
        BEGIN {
            while ((getline line < counts) > 0) {
                if (first != "") continue
                n = split(line, field, " ")
                first = field[1] " " field[2]
                under = field[3] - 1
            }
        }
        /^[ \t]*#/ || /^[ \t]*$/ { print; next }
        {
            key = $1 " " $2
            if (key == first && !lowered) {
                lowered = 1
                print $1, $2, under
                next
            }
            print
        }
        END { if (!lowered) exit 1 }
    ' "$1"
}

gate() {
    can_run || skip "$STATUS_USAGE"
    make_tmp
    build
    local counts=$TMP_ROOT/counts
    run_all "$counts"
    check "$BUDGET_FILE" "$counts"
    say "every scenario is within its budget in $BUDGET_FILE"
}

measure() {
    local runs=$1 iteration name
    [[ $runs =~ ^[1-9][0-9]*$ ]] || die "--measure takes a run count of 1 or more, not \`$runs\`"
    can_run || skip "$STATUS_USAGE"
    make_tmp
    build
    local counts=$TMP_ROOT/measure
    : >"$counts"
    for ((iteration = 1; iteration <= runs; iteration++)); do
        say "run $iteration of $runs"
        for name in "${SCENARIOS[@]}"; do
            run_scenario "$name" >>"$counts"
        done
    done
    budget_from_counts "$counts" "$runs"
}

self_test() {
    if ! can_run; then
        skip 0
    fi
    make_tmp
    build

    local counts=$TMP_ROOT/counts
    run_all "$counts"
    say "checking the counts against $BUDGET_FILE"
    check "$BUDGET_FILE" "$counts"

    # A budget a count under a count has to fail, so the gate is shown to bite.
    local copy=$TMP_ROOT/budget-under.txt status=0
    lower_one "$BUDGET_FILE" "$counts" >"$copy" ||
        die "no counted row is in $BUDGET_FILE"
    say "checking that a budget under a count fails"
    check "$copy" "$counts" || status=$?
    if ((status != STATUS_OVER)); then
        say "the self-test failed: a budget under a count did not fail the check (status $status)"
        exit 1
    fi

    # The thread filter: the child's lines must not count.
    local got
    got=$(count_log "$FILTER_FIXTURE") ||
        die "cannot count the syscalls of $FILTER_FIXTURE"
    if [[ $got != "$FILTER_EXPECTED" ]]; then
        say "the self-test failed: $FILTER_FIXTURE counted as"
        printf '%s\n' "$got" >&2
        say "but it has to count as"
        printf '%s\n' "$FILTER_EXPECTED" >&2
        exit 1
    fi

    say "self-test passed: $BUDGET_FILE holds every scenario, a budget under a count fails,"
    say "and the thread filter keeps the made thread and drops the made child in $FILTER_FIXTURE"
}

case ${1:-} in
--self-test)
    [[ $# -eq 1 ]] || die "--self-test takes no arguments"
    self_test
    ;;
--measure)
    [[ $# -le 2 ]] ||
        die "usage: bench/syscalls.sh | bench/syscalls.sh --measure [N] | bench/syscalls.sh --self-test"
    measure "${2:-$MEASURE_RUNS}"
    ;;
-h | --help)
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    ;;
*)
    [[ $# -eq 0 ]] ||
        die "usage: bench/syscalls.sh | bench/syscalls.sh --measure [N] | bench/syscalls.sh --self-test"
    gate
    ;;
esac

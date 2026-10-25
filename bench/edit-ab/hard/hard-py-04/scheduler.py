"""Plan tasks that depend on each other: times, critical path, workers, retries.

Every task is a node of a Graph with a `duration` attribute (default 1).
"""

from graph import Graph, GraphError


class ScheduleError(GraphError):
    """A schedule is impossible or breaks a rule."""


def duration(graph, name):
    value = graph.attrs(name).get("duration", 1)
    if value < 0:
        raise ScheduleError("%s has a negative duration" % name)
    return value


# -- times without worker limits ------------------------------------------------


def earliest_times(graph):
    """{task: (start, finish)} when each task starts as soon as it can."""
    times = {}
    for name in graph.topological_order():
        start = max((times[p][1] for p in graph.predecessors(name)), default=0)
        times[name] = (start, start + duration(graph, name))
    return times


def makespan(graph):
    """Time until the last task finishes with unlimited workers."""
    times = earliest_times(graph)
    return max((finish for _, finish in times.values()), default=0)


def latest_times(graph, deadline=None):
    """{task: (start, finish)} as late as possible without missing the deadline.

    The deadline defaults to the makespan.
    """
    if deadline is None:
        deadline = makespan(graph)
    times = {}
    for name in reversed(graph.topological_order()):
        succs = graph.successors(name)
        finish = max((times[s][0] for s in succs), default=deadline)
        times[name] = (finish - duration(graph, name), finish)
    return times


def slack(graph, deadline=None):
    """{task: how long it can slip before the deadline is missed}."""
    early = earliest_times(graph)
    late = latest_times(graph, deadline)
    return {name: late[name][0] - early[name][0] for name in graph.nodes()}


def critical_tasks(graph):
    """Tasks with no slack, in dependency order."""
    spare = slack(graph)
    return [name for name in graph.topological_order() if spare[name] == 0]


def critical_chain(graph):
    """One longest chain of tasks from a root to a leaf."""
    early = earliest_times(graph)
    if not early:
        return []
    last = max(early, key=lambda n: (early[n][1], n))
    chain = [last]
    while True:
        preds = graph.predecessors(chain[-1])
        if not preds:
            break
        tight = [p for p in preds if early[p][1] == early[chain[-1]][0]]
        if not tight:
            break
        chain.append(tight[0])
    return list(reversed(chain))


def what_if(graph, name, new_duration):
    """Makespan change when one task takes new_duration instead."""
    before = makespan(graph)
    trial = graph.copy()
    trial.add_node(name, duration=new_duration)
    return makespan(trial) - before


def bottlenecks(graph, n=3):
    """Tasks that hold back the most work: duration times dependents plus one."""
    scored = []
    for name in graph.nodes():
        weight = duration(graph, name) * (len(graph.descendants(name)) + 1)
        scored.append((-weight, name))
    scored.sort()
    return [name for _, name in scored[:n]]


# -- list scheduling with a fixed number of workers ----------------------------------------


def remaining_chain(graph):
    """{task: longest time from the start of the task to the end of the plan}."""
    rank = {}
    for name in reversed(graph.topological_order()):
        tail = max((rank[s] for s in graph.successors(name)), default=0)
        rank[name] = duration(graph, name) + tail
    return rank


def list_schedule(graph, workers):
    """Greedy schedule on `workers` identical workers.

    The ready task with the longest remaining chain goes first (ties by name)
    and runs on the worker that frees up first (ties by number). Returns
    (assignments, makespan) where each assignment is (task, worker, start, end).
    """
    if workers < 1:
        raise ScheduleError("need at least one worker")
    rank = remaining_chain(graph)
    finished = {}
    free_at = [0] * workers
    pending = set(graph.nodes())
    out = []
    while pending:
        ready = [n for n in pending if all(p in finished for p in graph.predecessors(n))]
        if not ready:
            raise ScheduleError("no task is ready; the graph has a cycle")
        ready.sort(key=lambda n: (-rank[n], n))
        name = ready[0]
        deps_done = max((finished[p] for p in graph.predecessors(name)), default=0)
        worker = min(range(workers), key=lambda w: (free_at[w], w))
        start = min(deps_done, free_at[worker])
        end = start + duration(graph, name)
        free_at[worker] = end
        finished[name] = end
        pending.discard(name)
        out.append((name, worker, start, end))
    return out, max(free_at)


def check_schedule(graph, assignments):
    """Return a list of rule breaks in a schedule, empty when it is sound."""
    problems = []
    by_task = {}
    for name, worker, start, end in assignments:
        if name in by_task:
            problems.append("%s is scheduled twice" % name)
        by_task[name] = (worker, start, end)
        if end - start != duration(graph, name):
            problems.append("%s has the wrong length" % name)
    for name in graph.nodes():
        if name not in by_task:
            problems.append("%s is missing" % name)
            continue
        for pred in graph.predecessors(name):
            if pred in by_task and by_task[pred][2] > by_task[name][1]:
                problems.append("%s starts before %s ends" % (name, pred))
    lanes = {}
    for name, (worker, start, end) in by_task.items():
        lanes.setdefault(worker, []).append((start, end, name))
    for worker, spans in sorted(lanes.items()):
        spans.sort()
        for (s1, e1, n1), (s2, e2, n2) in zip(spans, spans[1:]):
            if s2 < e1:
                problems.append("worker %d runs %s and %s at once" % (worker, n1, n2))
    return problems


def utilization(assignments, workers):
    """Busy share of the workers over the whole plan, from 0.0 to 1.0."""
    if not assignments:
        return 0.0
    end = max(a[3] for a in assignments)
    busy = sum(a[3] - a[2] for a in assignments)
    return busy / (end * workers) if end else 0.0


def render_gantt(assignments, workers, unit=1):
    """One text row per worker, one character per time unit."""
    if not assignments:
        return ""
    end = max(a[3] for a in assignments)
    rows = []
    for worker in range(workers):
        cells = ["."] * int(end / unit)
        for name, w, start, stop in assignments:
            if w != worker:
                continue
            for tick in range(int(start / unit), int(stop / unit)):
                cells[tick] = name[0]
        rows.append("w%d %s" % (worker, "".join(cells)))
    return "\n".join(rows)


# -- retries ------------------------------------------------------------------------------------


def backoff_delays(attempts, base=1.0, factor=2.0, cap=60.0):
    """Wait before each retry: base, base*factor, ... never above cap."""
    return [min(cap, base * factor ** i) for i in range(attempts)]


def retry_timeline(durations, delays):
    """Start and end of each try when every try but the last one fails.

    durations[i] is how long try i runs. delays[i] is the wait after try i
    fails. Returns a list of (start, end).
    """
    if len(delays) < len(durations) - 1:
        raise ScheduleError("not enough delays")
    out = []
    clock = 0.0
    for i, length in enumerate(durations):
        out.append((clock, clock + length))
        clock += length
        if i < len(durations) - 1:
            clock += delays[i]
    return out


def total_retry_time(durations, delays):
    timeline = retry_timeline(durations, delays)
    return timeline[-1][1] if timeline else 0.0


def plan_with_retries(graph, failures, delays):
    """Makespan when some tasks fail a number of times before they succeed.

    failures is {task: number of failed tries}. A failed try takes the full
    duration and is followed by the next delay from the list.
    """
    trial = graph.copy()
    for name, count in failures.items():
        base = duration(graph, name)
        tries = [base] * (count + 1)
        total = total_retry_time(tries, delays)
        trial.add_node(name, duration=total)
    return makespan(trial)

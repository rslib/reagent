import unittest

from graph import CycleError, Graph, GraphError, UnknownNode
from scheduler import (ScheduleError, backoff_delays, bottlenecks, check_schedule,
                       critical_chain, critical_tasks, earliest_times, latest_times,
                       list_schedule, makespan, plan_with_retries, remaining_chain,
                       render_gantt, retry_timeline, slack, utilization, what_if)


def diamond():
    g = Graph()
    for name, length in (("a", 2), ("b", 3), ("c", 1), ("d", 2), ("e", 4)):
        g.add_node(name, duration=length)
    for src, dst in (("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")):
        g.add_edge(src, dst)
    return g


def build(spec):
    """Graph from 'name:duration' nodes and 'a>b' edges separated by spaces."""
    g = Graph()
    for word in spec.split():
        if ">" in word:
            src, dst = word.split(">")
            g.add_edge(src, dst)
        else:
            name, _, length = word.partition(":")
            g.add_node(name, duration=int(length))
    return g


class StructureTest(unittest.TestCase):
    def test_edges_roots_leaves(self):
        g = diamond()
        self.assertEqual(g.roots(), ["a", "e"])
        self.assertEqual(g.leaves(), ["d", "e"])
        self.assertEqual(g.successors("a"), ["b", "c"])
        self.assertEqual(g.predecessors("d"), ["b", "c"])
        self.assertEqual(g.stats()["edges"], 4)

    def test_cycles_are_refused(self):
        g = diamond()
        with self.assertRaises(CycleError):
            g.add_edge("d", "a")
        with self.assertRaises(CycleError):
            g.add_edge("b", "b")
        self.assertFalse(g.has_edge("d", "a"))

    def test_a_shortcut_over_a_diamond_is_not_a_cycle(self):
        g = diamond()
        g.add_edge("a", "d")
        g.add_edge("e", "d")
        self.assertEqual(g.predecessors("d"), ["a", "b", "c", "e"])
        self.assertEqual(g.transitive_reduction().predecessors("d"), ["b", "c", "e"])

    def test_unknown_names(self):
        g = diamond()
        with self.assertRaises(UnknownNode):
            g.add_edge("a", "zzz")
        with self.assertRaises(UnknownNode):
            g.successors("zzz")

    def test_remove_node(self):
        g = diamond()
        g.remove_node("b")
        self.assertEqual(g.predecessors("d"), ["c"])
        self.assertEqual(g.successors("a"), ["c"])
        self.assertNotIn("b", g)


class ReachTest(unittest.TestCase):
    def test_ancestors(self):
        g = diamond()
        self.assertEqual(g.ancestors("d"), ["a", "b", "c"])
        self.assertEqual(g.ancestors("a"), [])
        self.assertEqual(g.ancestors("c"), ["a"])

    def test_descendants(self):
        g = diamond()
        self.assertEqual(g.descendants("a"), ["b", "c", "d"])
        self.assertEqual(g.descendants("d"), [])
        self.assertEqual(g.descendants("c"), ["d"])

    def test_reaches_and_components(self):
        g = diamond()
        self.assertTrue(g.reaches("a", "d"))
        self.assertFalse(g.reaches("d", "a"))
        self.assertFalse(g.reaches("e", "a"))
        self.assertEqual(g.components(), [["a", "b", "c", "d"], ["e"]])

    def test_subgraph_and_reverse(self):
        g = diamond()
        sub = g.subgraph(["a", "b", "d"])
        self.assertEqual(sub.edges(), [("a", "b"), ("b", "d")])
        self.assertEqual(g.reverse().successors("d"), ["b", "c"])


class OrderTest(unittest.TestCase):
    def test_topological_order_breaks_ties_by_name(self):
        g = diamond()
        self.assertEqual(g.topological_order(), ["a", "b", "c", "d", "e"])
        h = Graph.from_text("z -> y\nx -> y\nm")
        self.assertEqual(h.topological_order(), ["m", "x", "z", "y"])

    def test_levels_and_layers(self):
        g = diamond()
        self.assertEqual(g.levels(), {"a": 0, "b": 1, "c": 1, "d": 2, "e": 0})
        self.assertEqual(g.layers(), [["a", "e"], ["b", "c"], ["d"]])

    def test_find_cycle(self):
        g = Graph.from_edges([("a", "b"), ("b", "c"), ("c", "a"), ("c", "d")], check=False)
        cycle = g.find_cycle()
        self.assertEqual(cycle[0], cycle[-1])
        self.assertEqual(sorted(set(cycle)), ["a", "b", "c"])
        with self.assertRaises(CycleError):
            g.topological_order()
        self.assertIsNone(diamond().find_cycle())

    def test_text_form(self):
        g = Graph.from_text("# plan\na -> b -> c  # chain\nd\n")
        self.assertEqual(g.edges(), [("a", "b"), ("b", "c")])
        self.assertEqual(g.to_text(), "a -> b\nb -> c\nd")
        with self.assertRaises(GraphError):
            Graph.from_text("a -> ")

    def test_paths_between_two_nodes(self):
        g = diamond()
        self.assertEqual(g.shortest_path("a", "d"), ["a", "b", "d"])
        self.assertIsNone(g.shortest_path("d", "a"))
        self.assertEqual(g.count_paths("a", "d"), 2)
        self.assertEqual(g.count_paths("a", "a"), 1)
        self.assertEqual(g.count_paths("e", "d"), 0)
        wide = build("s:1 x:1 y:1 z:1 t:1 s>x s>y s>z x>t y>t z>t")
        self.assertEqual(wide.count_paths("s", "t"), 3)

    def test_merge(self):
        g = diamond()
        other = build("d:2 f:1 d>f")
        g.merge(other)
        self.assertEqual(g.successors("d"), ["f"])


class TimingTest(unittest.TestCase):
    def test_earliest_times_and_makespan(self):
        g = diamond()
        times = earliest_times(g)
        self.assertEqual(times["d"], (5, 7))
        self.assertEqual(times["e"], (0, 4))
        self.assertEqual(makespan(g), 7)

    def test_latest_times_and_slack(self):
        g = diamond()
        late = latest_times(g)
        self.assertEqual(late["d"], (5, 7))
        self.assertEqual(late["c"], (4, 5))
        self.assertEqual(late["e"], (3, 7))
        self.assertEqual(slack(g), {"a": 0, "b": 0, "c": 2, "d": 0, "e": 3})

    def test_slack_with_a_deadline(self):
        g = diamond()
        self.assertEqual(slack(g, deadline=10)["d"], 3)

    def test_critical_tasks_and_chain(self):
        g = diamond()
        self.assertEqual(critical_tasks(g), ["a", "b", "d"])
        self.assertEqual(critical_chain(g), ["a", "b", "d"])

    def test_what_if_and_bottlenecks(self):
        g = diamond()
        self.assertEqual(what_if(g, "c", 2), 0)
        self.assertEqual(what_if(g, "b", 6), 3)
        self.assertEqual(what_if(g, "e", 9), 2)
        self.assertEqual(bottlenecks(g, 2), ["a", "b"])

    def test_negative_duration(self):
        g = build("a:-1")
        with self.assertRaises(ScheduleError):
            makespan(g)


class WorkerTest(unittest.TestCase):
    def test_one_worker_runs_everything_in_a_row(self):
        g = diamond()
        plan, end = list_schedule(g, 1)
        self.assertEqual(end, 12)
        self.assertEqual(check_schedule(g, plan), [])

    def test_two_workers(self):
        g = diamond()
        plan, end = list_schedule(g, 2)
        self.assertEqual(check_schedule(g, plan), [])
        self.assertEqual(end, 8)
        by_task = {name: (w, s, e) for name, w, s, e in plan}
        self.assertEqual(by_task["a"], (0, 0, 2))
        self.assertEqual(by_task["d"], (0, 6, 8))

    def test_many_workers_reach_the_makespan(self):
        g = diamond()
        plan, end = list_schedule(g, 5)
        self.assertEqual(end, makespan(g))
        self.assertEqual(check_schedule(g, plan), [])
        self.assertAlmostEqual(utilization(plan, 5), 12 / 35)

    def test_priority_follows_the_longest_chain(self):
        g = build("long:5 mid:1 tail:4 short:1 long>tail short>tail")
        rank = remaining_chain(g)
        self.assertEqual((rank["long"], rank["short"], rank["tail"]), (9, 5, 4))
        plan, _ = list_schedule(g, 1)
        self.assertEqual([p[0] for p in plan], ["long", "short", "tail", "mid"])

    def test_check_schedule_finds_problems(self):
        g = build("a:2 b:2 a>b")
        bad = [("a", 0, 0, 2), ("b", 1, 1, 3)]
        self.assertEqual(check_schedule(g, bad), ["b starts before a ends"])
        overlap = [("a", 0, 0, 2), ("b", 0, 1, 3)]
        self.assertTrue(any("at once" in p for p in check_schedule(g, overlap)))

    def test_gantt(self):
        g = build("a:2 b:1")
        plan, _ = list_schedule(g, 2)
        self.assertEqual(render_gantt(plan, 2), "w0 aa\nw1 b.")

    def test_workers_must_be_positive(self):
        with self.assertRaises(ScheduleError):
            list_schedule(diamond(), 0)


class RetryTest(unittest.TestCase):
    def test_backoff_delays(self):
        self.assertEqual(backoff_delays(4), [1.0, 2.0, 4.0, 8.0])
        self.assertEqual(backoff_delays(5, base=10, cap=45), [10, 20, 40, 45, 45])
        self.assertEqual(backoff_delays(0), [])

    def test_retry_timeline(self):
        self.assertEqual(retry_timeline([3, 3, 3], [1, 2]), [(0, 3), (4, 7), (9, 12)])
        with self.assertRaises(ScheduleError):
            retry_timeline([1, 1, 1], [1])

    def test_plan_with_retries(self):
        g = diamond()
        self.assertEqual(plan_with_retries(g, {}, []), 7)
        self.assertEqual(plan_with_retries(g, {"c": 1}, [1.0]), 7)
        self.assertEqual(plan_with_retries(g, {"b": 1}, [1.0]), 11)


if __name__ == "__main__":
    unittest.main()

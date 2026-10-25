"""A directed acyclic graph of named tasks.

Edges point from a task to the tasks that depend on it: an edge a -> b means
that a must finish before b starts. Every method that lists nodes returns
them in sorted order so results are stable.
"""

import heapq


class GraphError(Exception):
    """Base class for graph problems."""


class CycleError(GraphError):
    """An edge would close a cycle."""


class UnknownNode(GraphError):
    """A name that is not in the graph."""


class Graph:
    def __init__(self):
        self._attrs = {}
        self._succ = {}
        self._pred = {}

    # -- basic structure --------------------------------------------------------

    def __contains__(self, name):
        return name in self._attrs

    def __len__(self):
        return len(self._attrs)

    def nodes(self):
        return sorted(self._attrs)

    def attrs(self, name):
        self._need(name)
        return self._attrs[name]

    def _need(self, name):
        if name not in self._attrs:
            raise UnknownNode(name)

    def add_node(self, name, **attrs):
        """Add a node, or update the attributes of an existing one."""
        if name not in self._attrs:
            self._attrs[name] = {}
            self._succ[name] = set()
            self._pred[name] = set()
        self._attrs[name].update(attrs)
        return self

    def remove_node(self, name):
        self._need(name)
        for succ in self._succ[name]:
            self._pred[succ].discard(name)
        for pred in self._pred[name]:
            self._succ[pred].discard(name)
        del self._attrs[name], self._succ[name], self._pred[name]
        return self

    def add_edge(self, src, dst):
        """Add src -> dst. Raises CycleError when dst already leads to src."""
        self._need(src)
        self._need(dst)
        if src == dst:
            raise CycleError("%s depends on itself" % src)
        if self.reaches(src, dst):
            raise CycleError("%s -> %s would close a cycle" % (src, dst))
        self._succ[src].add(dst)
        self._pred[dst].add(src)
        return self

    def remove_edge(self, src, dst):
        self._need(src)
        self._need(dst)
        self._succ[src].discard(dst)
        self._pred[dst].discard(src)
        return self

    def has_edge(self, src, dst):
        return src in self._succ and dst in self._succ[src]

    def edges(self):
        return [(a, b) for a in self.nodes() for b in sorted(self._succ[a])]

    def successors(self, name):
        self._need(name)
        return sorted(self._succ[name])

    def predecessors(self, name):
        self._need(name)
        return sorted(self._pred[name])

    def in_degree(self, name):
        self._need(name)
        return len(self._pred[name])

    def out_degree(self, name):
        self._need(name)
        return len(self._succ[name])

    def roots(self):
        """Nodes with no predecessor."""
        return [n for n in self.nodes() if not self._pred[n]]

    def leaves(self):
        """Nodes with no successor."""
        return [n for n in self.nodes() if not self._succ[n]]

    def copy(self):
        other = Graph()
        for name in self.nodes():
            other.add_node(name, **self._attrs[name])
        for a, b in self.edges():
            other._succ[a].add(b)
            other._pred[b].add(a)
        return other

    def subgraph(self, names):
        """The graph restricted to some nodes and the edges between them."""
        keep = set(names)
        for name in keep:
            self._need(name)
        other = Graph()
        for name in sorted(keep):
            other.add_node(name, **self._attrs[name])
        for a, b in self.edges():
            if a in keep and b in keep:
                other._succ[a].add(b)
                other._pred[b].add(a)
        return other

    def reverse(self):
        """A copy with every edge turned around."""
        other = Graph()
        for name in self.nodes():
            other.add_node(name, **self._attrs[name])
        for a, b in self.edges():
            other._succ[b].add(a)
            other._pred[a].add(b)
        return other

    # -- reachability -------------------------------------------------------------

    def reaches(self, src, dst):
        """True when there is a path of one or more edges from src to dst,
        or when src and dst are the same node."""
        self._need(src)
        self._need(dst)
        if src == dst:
            return True
        seen = {src}
        stack = [src]
        while stack:
            node = stack.pop()
            for nxt in self._succ[node]:
                if nxt == dst:
                    return True
                if nxt not in seen:
                    seen.add(nxt)
                    stack.append(nxt)
        return False

    def ancestors(self, name):
        """Every node that must finish before name can start."""
        self._need(name)
        seen = set()
        stack = [name]
        while stack:
            node = stack.pop()
            for nxt in self._succ[node]:
                if nxt not in seen:
                    seen.add(nxt)
                    stack.append(nxt)
        return sorted(seen)

    def descendants(self, name):
        """Every node that cannot start before name has finished."""
        self._need(name)
        seen = set()
        stack = [name]
        while stack:
            node = stack.pop()
            for nxt in self._succ[node]:
                if nxt not in seen:
                    seen.add(nxt)
                    stack.append(nxt)
        return sorted(seen)

    def components(self):
        """Weakly connected components, each sorted, ordered by first node."""
        seen = set()
        groups = []
        for start in self.nodes():
            if start in seen:
                continue
            group = []
            stack = [start]
            seen.add(start)
            while stack:
                node = stack.pop()
                group.append(node)
                for other in self._succ[node] | self._pred[node]:
                    if other not in seen:
                        seen.add(other)
                        stack.append(other)
            groups.append(sorted(group))
        return groups

    # -- ordering ------------------------------------------------------------------

    def topological_order(self):
        """A dependency order. Ties go to the smallest name."""
        indegree = {n: len(self._pred[n]) for n in self._attrs}
        ready = [n for n, d in indegree.items() if d == 0]
        heapq.heapify(ready)
        order = []
        while ready:
            node = heapq.heappop(ready)
            order.append(node)
            for nxt in self._succ[node]:
                indegree[nxt] -= 1
                if indegree[nxt] == 0:
                    heapq.heappush(ready, nxt)
        if len(order) != len(self._attrs):
            raise CycleError("the graph has a cycle")
        return order

    def find_cycle(self):
        """Return one cycle as a list of nodes, or None.

        Only needed for graphs built with from_edges(check=False).
        """
        white, grey, black = 0, 1, 2
        color = {n: white for n in self._attrs}
        parent = {}

        def visit(start):
            stack = [(start, iter(sorted(self._succ[start])))]
            color[start] = grey
            while stack:
                node, children = stack[-1]
                advanced = False
                for child in children:
                    if color[child] == white:
                        color[child] = grey
                        parent[child] = node
                        stack.append((child, iter(sorted(self._succ[child]))))
                        advanced = True
                        break
                    if color[child] == grey:
                        cycle = [child]
                        walk = node
                        while walk != child:
                            cycle.append(walk)
                            walk = parent[walk]
                        cycle.append(child)
                        return list(reversed(cycle))
                if not advanced:
                    color[node] = black
                    stack.pop()
            return None

        for name in self.nodes():
            if color[name] == white:
                found = visit(name)
                if found:
                    return found
        return None

    def levels(self):
        """Depth of every node: roots are 0, others one more than their
        deepest predecessor."""
        depth = {}
        for node in self.topological_order():
            preds = self._pred[node]
            depth[node] = 0 if not preds else max(depth[p] for p in preds) + 1
        return depth

    def layers(self):
        """Nodes grouped by level, as a list of sorted lists."""
        depth = self.levels()
        out = []
        for node in self.nodes():
            while len(out) <= depth[node]:
                out.append([])
            out[depth[node]].append(node)
        return out

    def transitive_reduction(self):
        """A copy without the edges that a longer path already implies."""
        other = self.copy()
        for a, b in self.edges():
            other.remove_edge(a, b)
            if not other.reaches(a, b):
                other._succ[a].add(b)
                other._pred[b].add(a)
        return other

    # -- text form ------------------------------------------------------------------

    @classmethod
    def from_edges(cls, edges, check=True, **attrs):
        """Build a graph from (src, dst) pairs. Nodes are created on the way."""
        graph = cls()
        for src, dst in edges:
            graph.add_node(src, **attrs)
            graph.add_node(dst, **attrs)
            if check:
                graph.add_edge(src, dst)
            else:
                graph._succ[src].add(dst)
                graph._pred[dst].add(src)
        return graph

    @classmethod
    def from_text(cls, text):
        """Read lines like 'a -> b -> c' and 'solo'. '#' starts a comment."""
        graph = cls()
        for raw in text.splitlines():
            line = raw.split("#", 1)[0].strip()
            if not line:
                continue
            names = [part.strip() for part in line.split("->")]
            if any(not name for name in names):
                raise GraphError("bad line %r" % raw)
            for name in names:
                graph.add_node(name)
            for src, dst in zip(names, names[1:]):
                graph.add_edge(src, dst)
        return graph

    def to_text(self):
        """One line per edge, then one line per node that has no edge."""
        lines = ["%s -> %s" % edge for edge in self.edges()]
        for name in self.nodes():
            if not self._succ[name] and not self._pred[name]:
                lines.append(name)
        return "\n".join(lines)

    def to_dot(self, label=None):
        lines = ["digraph tasks {"]
        for name in self.nodes():
            text = label(name, self._attrs[name]) if label else name
            lines.append('  "%s" [label="%s"];' % (name, text))
        for a, b in self.edges():
            lines.append('  "%s" -> "%s";' % (a, b))
        lines.append("}")
        return "\n".join(lines)

    def stats(self):
        """Counts that are handy in logs."""
        return {
            "nodes": len(self._attrs),
            "edges": len(self.edges()),
            "roots": len(self.roots()),
            "leaves": len(self.leaves()),
            "components": len(self.components()),
            "depth": max(self.levels().values()) + 1 if self._attrs else 0,
        }

    # -- paths between two nodes ----------------------------------------------------

    def shortest_path(self, src, dst):
        """The path from src to dst with the fewest edges, or None."""
        self._need(src)
        self._need(dst)
        previous = {src: None}
        queue = [src]
        for node in queue:
            if node == dst:
                break
            for nxt in sorted(self._succ[node]):
                if nxt not in previous:
                    previous[nxt] = node
                    queue.append(nxt)
        if dst not in previous:
            return None
        path = []
        node = dst
        while node is not None:
            path.append(node)
            node = previous[node]
        return list(reversed(path))

    def count_paths(self, src, dst):
        """How many different paths lead from src to dst."""
        self._need(src)
        self._need(dst)
        counts = {n: 0 for n in self._attrs}
        counts[src] = 1
        for node in self.topological_order():
            for nxt in self._succ[node]:
                counts[nxt] = counts[node]
        return counts[dst]

    def merge(self, other):
        """Add every node and edge of another graph. Returns self."""
        for name in other.nodes():
            self.add_node(name, **other.attrs(name))
        for a, b in other.edges():
            self.add_edge(a, b)
        return self

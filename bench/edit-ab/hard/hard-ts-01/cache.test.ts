import test from "node:test";
import assert from "node:assert/strict";
import {
  CacheError,
  LruCache,
  NamespacedCache,
  StatsWindow,
  TieredCache,
  memoize,
} from "./cache.ts";

function clocked() {
  const state = { now: 1000 };
  return { state, clock: () => state.now };
}

test("keeps the most recently used entries", () => {
  const c = new LruCache<string, number>({ capacity: 3 });
  c.set("a", 1).set("b", 2).set("c", 3);
  assert.equal(c.size, 3);
  c.get("a");
  c.set("d", 4);
  assert.deepEqual(c.keys(), ["d", "a", "c"]);
  assert.equal(c.get("b"), undefined);
});

test("an update moves the key to the front without growing the cache", () => {
  const c = new LruCache<string, number>({ capacity: 2 });
  c.set("a", 1).set("b", 2).set("a", 10);
  assert.equal(c.size, 2);
  assert.deepEqual(c.keys(), ["a", "b"]);
  assert.equal(c.get("a"), 10);
});

test("an entry is gone at the moment its time to live runs out", () => {
  const { state, clock } = clocked();
  const c = new LruCache<string, number>({ capacity: 5, ttl: 100, clock });
  c.set("a", 1);
  state.now += 99;
  assert.equal(c.get("a"), 1);
  state.now += 1;
  assert.equal(c.get("a"), undefined);
  assert.equal(c.size, 0);
  assert.equal(c.stats().expirations, 1);
});

test("setting a key again starts a fresh time to live", () => {
  const { state, clock } = clocked();
  const c = new LruCache<string, number>({ capacity: 5, ttl: 100, clock });
  c.set("a", 1);
  state.now += 60;
  c.set("a", 2);
  state.now += 60;
  assert.equal(c.get("a"), 2);
  assert.equal(c.ttlRemaining("a"), 40);
});

test("a per call ttl beats the default and zero means forever", () => {
  const { state, clock } = clocked();
  const c = new LruCache<string, number>({ capacity: 5, ttl: 100, clock });
  c.set("short", 1, 10);
  c.set("long", 2, 1000);
  c.set("never", 3, 0);
  state.now += 50;
  assert.equal(c.has("short"), false);
  assert.equal(c.has("long"), true);
  assert.equal(c.ttlRemaining("never"), Infinity);
  state.now += 1_000_000;
  assert.equal(c.has("never"), true);
});

test("peek and has do not touch the order or the counters", () => {
  const c = new LruCache<string, number>({ capacity: 2 });
  c.set("a", 1).set("b", 2);
  assert.equal(c.peek("a"), 1);
  assert.equal(c.has("a"), true);
  c.set("c", 3);
  assert.deepEqual(c.keys(), ["c", "b"]);
  assert.deepEqual(c.stats(), { hits: 0, misses: 0, evictions: 1, expirations: 0, sets: 3 });
});

test("purgeExpired and nextExpiry", () => {
  const { state, clock } = clocked();
  const c = new LruCache<string, number>({ capacity: 5, clock });
  c.set("a", 1, 10).set("b", 2, 30).set("c", 3);
  assert.equal(c.nextExpiry(), 1010);
  state.now += 20;
  assert.equal(c.purgeExpired(), 1);
  assert.equal(c.nextExpiry(), 1030);
  state.now += 20;
  assert.equal(c.purgeExpired(), 1);
  assert.equal(c.nextExpiry(), undefined);
  assert.deepEqual(c.keys(), ["c"]);
});

test("oldest and newest keys", () => {
  const c = new LruCache<string, number>({ capacity: 5 });
  c.setMany([["a", 1], ["b", 2], ["c", 3], ["d", 4]]);
  assert.deepEqual(c.newestKeys(2), ["d", "c"]);
  assert.deepEqual(c.oldestKeys(2), ["a", "b"]);
  assert.deepEqual(c.oldestKeys(10), ["a", "b", "c", "d"]);
  c.get("a");
  assert.deepEqual(c.oldestKeys(1), ["b"]);
});

test("demote makes a key the next to go", () => {
  const c = new LruCache<string, number>({ capacity: 3 });
  c.setMany([["a", 1], ["b", 2], ["c", 3]]);
  assert.equal(c.demote("c"), true);
  assert.equal(c.demote("zzz"), false);
  c.set("d", 4);
  assert.deepEqual(c.keys(), ["d", "b", "a"]);
});

test("weights limit the total and delete gives the weight back", () => {
  const c = new LruCache<string, string>({
    capacity: 10,
    maxWeight: 10,
    weigh: (v) => v.length,
  });
  c.set("a", "xxxx").set("b", "yyyy");
  assert.equal(c.weight, 8);
  c.set("c", "zzzz");
  assert.deepEqual(c.keys(), ["c", "b"]);
  assert.equal(c.weight, 8);
  assert.equal(c.delete("b"), true);
  assert.equal(c.weight, 4);
  c.set("d", "wwwwww");
  assert.deepEqual(c.keys(), ["d", "c"]);
  assert.equal(c.weight, 10);
  assert.throws(() => c.set("big", "x".repeat(11)), CacheError);
});

test("updating a value changes the weight by the difference", () => {
  const c = new LruCache<string, string>({ capacity: 5, maxWeight: 20, weigh: (v) => v.length });
  c.set("a", "xxxxx");
  c.set("a", "xx");
  assert.equal(c.weight, 2);
});

test("the eviction callback names the reason", () => {
  const seen: string[] = [];
  const { state, clock } = clocked();
  const c = new LruCache<string, number>({
    capacity: 2,
    ttl: 10,
    clock,
    onEvict: (k, _v, reason) => seen.push(k + ":" + reason),
  });
  c.set("a", 1).set("b", 2).set("c", 3);
  c.delete("b");
  state.now += 10;
  c.get("c");
  c.set("d", 4);
  c.clear();
  assert.deepEqual(seen, ["a:capacity", "b:deleted", "c:expired", "d:cleared"]);
});

test("getOrCompute computes once", async () => {
  const c = new LruCache<string, number>({ capacity: 3 });
  let calls = 0;
  const make = () => {
    calls++;
    return 7;
  };
  assert.equal(c.getOrCompute("x", make), 7);
  assert.equal(c.getOrCompute("x", make), 7);
  assert.equal(calls, 1);
  assert.equal(await c.getOrComputeAsync("y", async () => 9), 9);
  assert.equal(await c.getOrComputeAsync("y", async () => 10), 9);
});

test("many reads and writes", () => {
  const c = new LruCache<number, number>({ capacity: 3 });
  c.setMany([[1, 10], [2, 20], [3, 30]]);
  const got = c.getMany([1, 9, 3]);
  assert.deepEqual([...got.entries()], [[1, 10], [3, 30]]);
  assert.deepEqual(c.keys(), [3, 1, 2]);
});

test("pop, findKey and resize", () => {
  const c = new LruCache<string, number>({ capacity: 4 });
  c.setMany([["a", 1], ["b", 2], ["c", 3], ["d", 4]]);
  assert.equal(c.findKey((v) => v % 2 === 1), "c");
  assert.deepEqual(c.pop(), ["a", 1]);
  c.resize(2);
  assert.deepEqual(c.keys(), ["d", "c"]);
  assert.throws(() => c.resize(0), CacheError);
  assert.equal(new LruCache<string, number>({ capacity: 1 }).pop(), undefined);
});

test("hit rate and counters", () => {
  const c = new LruCache<string, number>({ capacity: 2 });
  assert.equal(c.hitRate(), 0);
  c.set("a", 1);
  c.get("a");
  c.get("a");
  c.get("zzz");
  assert.equal(c.hitRate(), 2 / 3);
  c.resetStats();
  assert.equal(c.stats().hits, 0);
});

test("snapshot and restore keep the order and the time left", () => {
  const { state, clock } = clocked();
  const a = new LruCache<string, number>({ capacity: 5, ttl: 100, clock });
  a.set("x", 1).set("y", 2).set("z", 3, 0);
  state.now += 30;
  a.get("x");
  const b = new LruCache<string, number>({ capacity: 5, ttl: 100, clock });
  b.restore(a.snapshot());
  assert.deepEqual(b.keys(), a.keys());
  assert.equal(b.ttlRemaining("y"), 70);
  assert.equal(b.ttlRemaining("z"), Infinity);
});

test("tiered cache promotes second level hits", () => {
  const first = new LruCache<string, number>({ capacity: 1 });
  const second = new LruCache<string, number>({ capacity: 4 });
  const t = new TieredCache(first, second);
  t.set("a", 1);
  t.set("b", 2);
  assert.equal(first.has("a"), false);
  assert.equal(t.get("a"), 1);
  assert.equal(first.has("a"), true);
  assert.equal(t.promoted, 1);
  assert.equal(t.get("a"), 1);
  assert.equal(t.get("nope"), undefined);
  assert.equal(t.promoted, 1);
  assert.equal(t.delete("a"), true);
  assert.equal(second.has("a"), false);
});

test("tiered hit rate counts hits from both levels", () => {
  const first = new LruCache<string, number>({ capacity: 1 });
  const second = new LruCache<string, number>({ capacity: 4 });
  const t = new TieredCache(first, second);
  t.set("a", 1);
  t.set("b", 2);
  t.get("b");
  t.get("a");
  t.get("none");
  t.get("a");
  assert.equal(t.hitRate(), 3 / 4);
});

test("namespaces share a cache without clashing", () => {
  const shared = new LruCache<string, number>({ capacity: 10 });
  const users = new NamespacedCache(shared, "users");
  const posts = new NamespacedCache(shared, "posts");
  users.set("1", 100);
  posts.set("1", 200);
  assert.equal(users.get("1"), 100);
  assert.equal(posts.get("1"), 200);
  assert.deepEqual(users.keys(), ["1"]);
  assert.equal(users.clear(), 1);
  assert.equal(posts.has("1"), true);
  assert.throws(() => new NamespacedCache(shared, "a:b"), CacheError);
});

test("a rolling window forgets old reads", () => {
  const w = new StatsWindow(3);
  assert.equal(w.rate(), 0);
  w.record(true);
  w.record(false);
  assert.equal(w.rate(), 0.5);
  w.record(true);
  w.record(true);
  w.record(true);
  assert.equal(w.count, 3);
  assert.equal(w.rate(), 1);
});

test("memoize", () => {
  let calls = 0;
  const square = memoize((n: number) => {
    calls++;
    return n * n;
  }, { capacity: 2 });
  assert.equal(square(3), 9);
  assert.equal(square(3), 9);
  square(4);
  square(5);
  square(3);
  assert.equal(calls, 4);
  assert.equal(square.cache.size, 2);
});

test("bad options", () => {
  assert.throws(() => new LruCache<string, number>({ capacity: 0 }), CacheError);
  assert.throws(() => new LruCache<string, number>({ capacity: 2, maxWeight: 5 }), CacheError);
});

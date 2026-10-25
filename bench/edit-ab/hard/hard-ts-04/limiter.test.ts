import test from "node:test";
import assert from "node:assert/strict";
import {
  ClientRegistry,
  CompositeLimiter,
  FixedWindow,
  HourCounter,
  LeakyBucket,
  LimiterError,
  MinuteCounter,
  SlidingWindowCounter,
  SlidingWindowLog,
  TokenBucket,
  backoffMs,
  parseRetryAfter,
  retryAfterSeconds,
} from "./limiter.ts";

function clock(start = 1_000_000) {
  const state = { now: start };
  return { state, fn: () => state.now };
}

test("a token bucket starts full and empties", () => {
  const c = clock();
  const b = new TokenBucket(3, 1, c.fn);
  assert.deepEqual(b.take(), { allowed: true, remaining: 2, retryAfterMs: 0 });
  b.take();
  b.take();
  const d = b.take();
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, 1000);
});

test("tokens come back with time and stop at capacity", () => {
  const c = clock();
  const b = new TokenBucket(4, 2, c.fn);
  b.take(4);
  c.state.now += 500;
  assert.equal(b.available(), 1);
  c.state.now += 500;
  assert.equal(b.available(), 2);
  c.state.now += 60_000;
  assert.equal(b.available(), 4);
});

test("a second look at the bucket does not hand out tokens twice", () => {
  const c = clock();
  const b = new TokenBucket(10, 1, c.fn);
  b.take(10);
  c.state.now += 2000;
  assert.equal(b.available(), 2);
  assert.equal(b.available(), 2);
  assert.equal(b.take(2).allowed, true);
  assert.equal(b.take(1).allowed, false);
});

test("retry after is rounded up to whole milliseconds", () => {
  const c = clock();
  const b = new TokenBucket(5, 3, c.fn);
  b.take(5);
  c.state.now += 500;
  assert.equal(b.available(), 1.5);
  const d = b.take(2);
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, 167);
  assert.equal(b.retryAfterMs(1), 0);
  assert.equal(b.retryAfterMs(5), 1167);
});

test("a request bigger than the bucket never passes", () => {
  const b = new TokenBucket(2, 1, clock().fn);
  const d = b.take(3);
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, Infinity);
  assert.equal(b.canTake(2), true);
});

test("bad settings", () => {
  assert.throws(() => new TokenBucket(0, 1), LimiterError);
  assert.throws(() => new FixedWindow(1, -5), LimiterError);
  assert.throws(() => new TokenBucket(1, 1).take(0), LimiterError);
});

test("leaky bucket drains over time", () => {
  const c = clock();
  const b = new LeakyBucket(3, 1, c.fn);
  assert.equal(b.take(2).remaining, 1);
  assert.equal(b.take(1).remaining, 0);
  const d = b.take(1);
  assert.deepEqual([d.allowed, d.retryAfterMs], [false, 1000]);
  c.state.now += 2000;
  assert.equal(b.canTake(2), true);
  assert.equal(b.take(2).allowed, true);
  b.reset();
  assert.equal(b.take(3).allowed, true);
});

test("fixed window resets at the window edge", () => {
  const c = clock(120_000);
  const w = new FixedWindow(2, 60_000, c.fn);
  c.state.now += 59_000;
  assert.equal(w.take().allowed, true);
  assert.equal(w.take().allowed, true);
  const d = w.take();
  assert.deepEqual([d.allowed, d.remaining, d.retryAfterMs], [false, 0, 1000]);
  c.state.now += 1000;
  assert.equal(w.take().allowed, true);
});

test("sliding window log frees a slot after exactly one window", () => {
  const c = clock();
  const w = new SlidingWindowLog(2, 1000, c.fn);
  assert.equal(w.take().allowed, true);
  c.state.now += 400;
  assert.equal(w.take().allowed, true);
  const d = w.take();
  assert.deepEqual([d.allowed, d.retryAfterMs], [false, 600]);
  c.state.now += 599;
  assert.equal(w.take().allowed, false);
  c.state.now += 1;
  assert.equal(w.count(), 1);
  assert.equal(w.take().allowed, true);
});

test("sliding window log waits for as many entries as needed", () => {
  const c = clock();
  const w = new SlidingWindowLog(3, 1000, c.fn);
  w.take();
  c.state.now += 100;
  w.take();
  c.state.now += 100;
  w.take();
  const d = w.take(2);
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, 900);
  assert.equal(w.canTake(1), false);
});

test("sliding window counter weighs the last window", () => {
  const c = clock(0);
  const w = new SlidingWindowCounter(10, 1000, c.fn);
  for (let i = 0; i < 8; i++) assert.equal(w.take().allowed, true);
  c.state.now = 1500;
  // half of the previous window still counts: 8 * 0.5 = 4 used
  assert.equal(w.canTake(6), true);
  assert.equal(w.canTake(7), false);
  assert.equal(w.take(6).allowed, true);
  assert.equal(w.take(1).allowed, false);
  c.state.now = 3500;
  assert.equal(w.canTake(10), true);
});

test("the minute counter changes at the minute", () => {
  const c = clock(60_000 * 100);
  const m = new MinuteCounter(2, c.fn);
  c.state.now += 30_000;
  m.take();
  m.take();
  const d = m.take();
  assert.deepEqual([d.allowed, d.retryAfterMs], [false, 30_000]);
  c.state.now += 30_000;
  assert.equal(m.take().allowed, true);
});

test("the hour counter lasts a whole hour", () => {
  const c = clock(3_600_000 * 50);
  const h = new HourCounter(2, c.fn);
  h.take();
  h.take();
  c.state.now += 61_000;
  const d = h.take();
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, 3_600_000 - 61_000);
  c.state.now += 3_600_000 - 61_000;
  assert.equal(h.take().allowed, true);
  h.reset();
  assert.equal(h.canTake(2), true);
});

test("a composite charges nobody when one limiter says no", () => {
  const c = clock();
  const small = new TokenBucket(1, 1, c.fn);
  const big = new TokenBucket(10, 1, c.fn);
  const both = new CompositeLimiter([small, big]);
  assert.equal(both.take().allowed, true);
  const d = both.take();
  assert.equal(d.allowed, false);
  assert.equal(d.retryAfterMs, 1000);
  assert.equal(big.available(), 9);
  assert.equal(both.canTake(), false);
  c.state.now += 1000;
  assert.equal(both.canTake(), true);
});

test("a composite allows only what every limiter allows", () => {
  const c = clock();
  const a = new FixedWindow(5, 60_000, c.fn);
  const b = new FixedWindow(2, 60_000, c.fn);
  const both = new CompositeLimiter([a, b]);
  assert.equal(both.canTake(3), false);
  assert.equal(both.take(3).allowed, false);
  assert.equal(a.canTake(5), true);
  assert.equal(both.take(2).remaining, 0);
  both.reset();
  assert.equal(both.take(2).allowed, true);
  assert.throws(() => new CompositeLimiter([]), LimiterError);
});

test("the registry keeps one limiter per client", () => {
  const c = clock();
  const r = new ClientRegistry(() => new FixedWindow(1, 60_000, c.fn), c.fn);
  assert.equal(r.take("a").allowed, true);
  assert.equal(r.take("a").allowed, false);
  assert.equal(r.take("b").allowed, true);
  assert.deepEqual(r.stats(), { clients: 2, allowed: 2, denied: 1, evicted: 0 });
});

test("the registry forgets idle clients", () => {
  const c = clock();
  const r = new ClientRegistry(() => new FixedWindow(1, 60_000, c.fn), c.fn);
  r.take("old");
  c.state.now += 5000;
  r.take("new");
  c.state.now += 5000;
  assert.equal(r.evictIdle(10_000), 1);
  assert.equal(r.has("old"), false);
  assert.equal(r.has("new"), true);
  assert.equal(r.stats().evicted, 1);
});

test("the registry makes room by dropping the least recently seen", () => {
  const c = clock();
  const r = new ClientRegistry(() => new TokenBucket(1, 1, c.fn), c.fn, 2);
  r.take("a");
  c.state.now += 10;
  r.take("b");
  c.state.now += 10;
  r.take("a");
  c.state.now += 10;
  r.take("c");
  assert.equal(r.has("b"), false);
  assert.equal(r.has("a"), true);
  assert.equal(r.stats().clients, 2);
});

test("backoff grows and is capped", () => {
  assert.deepEqual([0, 1, 2, 3].map((n) => backoffMs(n)), [100, 200, 400, 800]);
  assert.equal(backoffMs(20), 30_000);
  assert.equal(backoffMs(2, 100, 30_000, (max) => max), 200);
});

test("retry after headers", () => {
  assert.equal(retryAfterSeconds(0), 1);
  assert.equal(retryAfterSeconds(1001), 2);
  assert.equal(parseRetryAfter("120", 0), 120_000);
  assert.equal(parseRetryAfter("Thu, 01 Jan 1970 00:00:10 GMT", 4000), 6000);
  assert.equal(parseRetryAfter("Thu, 01 Jan 1970 00:00:10 GMT", 20_000), 0);
  assert.equal(parseRetryAfter("soon", 0), null);
});

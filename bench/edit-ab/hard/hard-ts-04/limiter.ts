// Rate limiters: token bucket, leaky bucket, fixed and sliding windows, a
// composite of several, and a registry that keeps one limiter per client.
//
// Every limiter reads time from a clock function in milliseconds so that tests
// can move time by hand.

export type Clock = () => number;

export interface Decision {
  allowed: boolean;
  /** Units left after this decision. */
  remaining: number;
  /** Ms to wait before the same request could pass. Zero when allowed. */
  retryAfterMs: number;
}

export interface Limiter {
  /** Try to use `cost` units. A denied request uses nothing. */
  take(cost?: number): Decision;
  /** Would `cost` units pass right now? Changes nothing but the clock reading. */
  canTake(cost?: number): boolean;
  /** Forget everything. */
  reset(): void;
}

export class LimiterError extends Error {}

function positive(name: string, value: number): void {
  if (!(value > 0) || !Number.isFinite(value)) throw new LimiterError(name + " must be positive");
}

function allow(remaining: number): Decision {
  return { allowed: true, remaining, retryAfterMs: 0 };
}

function deny(remaining: number, retryAfterMs: number): Decision {
  return { allowed: false, remaining, retryAfterMs };
}

// -- token bucket ---------------------------------------------------------------

/** Holds up to `capacity` tokens and gains `perSecond` tokens each second. */
export class TokenBucket implements Limiter {
  private tokens: number;
  private last: number;
  readonly capacity: number;
  readonly perSecond: number;
  private readonly clock: Clock;

  constructor(capacity: number, perSecond: number, clock: Clock = Date.now) {
    positive("capacity", capacity);
    positive("perSecond", perSecond);
    this.capacity = capacity;
    this.perSecond = perSecond;
    this.clock = clock;
    this.tokens = capacity;
    this.last = clock();
  }

  private refill(): void {
    const now = this.clock();
    const elapsed = Math.max(0, now - this.last);
    this.tokens = Math.min(this.capacity, this.tokens + (elapsed / 1000) * this.perSecond);
  }

  available(): number {
    this.refill();
    return this.tokens;
  }

  canTake(cost = 1): boolean {
    return this.available() >= cost;
  }

  take(cost = 1): Decision {
    positive("cost", cost);
    if (cost > this.capacity) return deny(this.available(), Infinity);
    this.refill();
    if (this.tokens >= cost) {
      this.tokens -= cost;
      return allow(this.tokens);
    }
    return deny(this.tokens, this.retryAfterMs(cost));
  }

  /** Ms until `cost` tokens will be there. */
  retryAfterMs(cost = 1): number {
    this.refill();
    if (this.tokens >= cost) return 0;
    return Math.floor(((cost - this.tokens) / this.perSecond) * 1000);
  }

  reset(): void {
    this.tokens = this.capacity;
    this.last = this.clock();
  }
}

// -- leaky bucket -------------------------------------------------------------------

/** A queue that drains at a fixed rate. A request fits while the queue has room. */
export class LeakyBucket implements Limiter {
  private level = 0;
  private last: number;
  readonly capacity: number;
  readonly perSecond: number;
  private readonly clock: Clock;

  constructor(capacity: number, perSecond: number, clock: Clock = Date.now) {
    positive("capacity", capacity);
    positive("perSecond", perSecond);
    this.capacity = capacity;
    this.perSecond = perSecond;
    this.clock = clock;
    this.last = clock();
  }

  private drain(): void {
    const now = this.clock();
    const elapsed = Math.max(0, now - this.last);
    this.level = Math.max(0, this.level - (elapsed / 1000) * this.perSecond);
    this.last = now;
  }

  canTake(cost = 1): boolean {
    this.drain();
    return this.level + cost <= this.capacity;
  }

  take(cost = 1): Decision {
    positive("cost", cost);
    this.drain();
    if (this.level + cost <= this.capacity) {
      this.level += cost;
      return allow(this.capacity - this.level);
    }
    const overflow = this.level + cost - this.capacity;
    return deny(this.capacity - this.level, Math.ceil((overflow / this.perSecond) * 1000));
  }

  reset(): void {
    this.level = 0;
    this.last = this.clock();
  }
}

// -- fixed window --------------------------------------------------------------------------

/** At most `limit` units in each window that starts at a multiple of windowMs. */
export class FixedWindow implements Limiter {
  private windowStart = -1;
  private used = 0;
  readonly limit: number;
  readonly windowMs: number;
  private readonly clock: Clock;

  constructor(limit: number, windowMs: number, clock: Clock = Date.now) {
    positive("limit", limit);
    positive("windowMs", windowMs);
    this.limit = limit;
    this.windowMs = windowMs;
    this.clock = clock;
  }

  private roll(): number {
    const now = this.clock();
    const start = Math.floor(now / this.windowMs) * this.windowMs;
    if (start !== this.windowStart) {
      this.windowStart = start;
      this.used = 0;
    }
    return start + this.windowMs - now;
  }

  canTake(cost = 1): boolean {
    this.roll();
    return this.used + cost <= this.limit;
  }

  take(cost = 1): Decision {
    positive("cost", cost);
    const untilNext = this.roll();
    if (this.used + cost <= this.limit) {
      this.used += cost;
      return allow(this.limit - this.used);
    }
    return deny(this.limit - this.used, untilNext);
  }

  reset(): void {
    this.windowStart = -1;
    this.used = 0;
  }
}

// -- sliding window log ----------------------------------------------------------------------

/** Keeps the time of every accepted unit and counts those inside the window. */
export class SlidingWindowLog implements Limiter {
  private log: number[] = [];
  readonly limit: number;
  readonly windowMs: number;
  private readonly clock: Clock;

  constructor(limit: number, windowMs: number, clock: Clock = Date.now) {
    positive("limit", limit);
    positive("windowMs", windowMs);
    this.limit = limit;
    this.windowMs = windowMs;
    this.clock = clock;
  }

  /** Drop entries that are a full window old or older. */
  private prune(now: number): void {
    let drop = 0;
    while (drop < this.log.length && this.log[drop] < now - this.windowMs) drop++;
    if (drop > 0) this.log.splice(0, drop);
  }

  count(): number {
    this.prune(this.clock());
    return this.log.length;
  }

  canTake(cost = 1): boolean {
    return this.count() + cost <= this.limit;
  }

  take(cost = 1): Decision {
    positive("cost", cost);
    const now = this.clock();
    this.prune(now);
    if (this.log.length + cost <= this.limit) {
      for (let i = 0; i < cost; i++) this.log.push(now);
      return allow(this.limit - this.log.length);
    }
    // The request passes once enough old entries have left the window.
    const needed = this.log.length + cost - this.limit;
    const index = Math.min(needed, this.log.length) - 1;
    const wait = index >= 0 ? this.log[index] + this.windowMs - now : this.windowMs;
    return deny(this.limit - this.log.length, Math.max(0, wait));
  }

  reset(): void {
    this.log = [];
  }
}

// -- sliding window counter ----------------------------------------------------------------------

/**
 * Two counters, for this window and the one before. The earlier window counts
 * in proportion to how much of it still overlaps the sliding window.
 */
export class SlidingWindowCounter implements Limiter {
  private windowStart = -1;
  private current = 0;
  private previous = 0;
  readonly limit: number;
  readonly windowMs: number;
  private readonly clock: Clock;

  constructor(limit: number, windowMs: number, clock: Clock = Date.now) {
    positive("limit", limit);
    positive("windowMs", windowMs);
    this.limit = limit;
    this.windowMs = windowMs;
    this.clock = clock;
  }

  private estimate(): number {
    const now = this.clock();
    const start = Math.floor(now / this.windowMs) * this.windowMs;
    if (start !== this.windowStart) {
      this.previous = start - this.windowStart === this.windowMs ? this.current : 0;
      this.current = 0;
      this.windowStart = start;
    }
    const overlap = 1 - (now - start) / this.windowMs;
    return this.current + this.previous * overlap;
  }

  canTake(cost = 1): boolean {
    return this.estimate() + cost <= this.limit;
  }

  take(cost = 1): Decision {
    positive("cost", cost);
    const used = this.estimate();
    if (used + cost <= this.limit) {
      this.current += cost;
      return allow(Math.floor(this.limit - used - cost));
    }
    return deny(Math.max(0, Math.floor(this.limit - used)), this.windowMs / 4);
  }

  reset(): void {
    this.windowStart = -1;
    this.current = 0;
    this.previous = 0;
  }
}

// -- calendar counters ------------------------------------------------------------------------------

/** At most `limit` units in each calendar minute. */
export class MinuteCounter implements Limiter {
  private bucket = -1;
  private count = 0;
  private readonly limit: number;
  private readonly clock: Clock;

  constructor(limit: number, clock: Clock = Date.now) {
    positive("limit", limit);
    this.limit = limit;
    this.clock = clock;
  }

  private roll(): number {
    const now = this.clock();
    const bucket = Math.floor(now / 60_000);
    if (bucket !== this.bucket) {
      this.bucket = bucket;
      this.count = 0;
    }
    return (bucket + 1) * 60_000 - now;
  }

  canTake(cost = 1): boolean {
    this.roll();
    return this.count + cost <= this.limit;
  }

  take(cost = 1): Decision {
    const untilNext = this.roll();
    if (this.count + cost <= this.limit) {
      this.count += cost;
      return allow(this.limit - this.count);
    }
    return deny(this.limit - this.count, untilNext);
  }

  reset(): void {
    this.bucket = -1;
    this.count = 0;
  }
}

/** At most `limit` units in each calendar hour. */
export class HourCounter implements Limiter {
  private bucket = -1;
  private count = 0;
  private readonly limit: number;
  private readonly clock: Clock;

  constructor(limit: number, clock: Clock = Date.now) {
    positive("limit", limit);
    this.limit = limit;
    this.clock = clock;
  }

  private roll(): number {
    const now = this.clock();
    const bucket = Math.floor(now / 60_000);
    if (bucket !== this.bucket) {
      this.bucket = bucket;
      this.count = 0;
    }
    return (bucket + 1) * 3_600_000 - now;
  }

  canTake(cost = 1): boolean {
    this.roll();
    return this.count + cost <= this.limit;
  }

  take(cost = 1): Decision {
    const untilNext = this.roll();
    if (this.count + cost <= this.limit) {
      this.count += cost;
      return allow(this.limit - this.count);
    }
    return deny(this.limit - this.count, untilNext);
  }

  reset(): void {
    this.bucket = -1;
    this.count = 0;
  }
}

// -- several limiters at once -------------------------------------------------------------------------

/**
 * A request passes only when every limiter would let it through. When it
 * passes, every limiter is charged. When it fails, none is.
 */
export class CompositeLimiter implements Limiter {
  private readonly limiters: Limiter[];

  constructor(limiters: Limiter[]) {
    if (limiters.length === 0) throw new LimiterError("need at least one limiter");
    this.limiters = limiters;
  }

  canTake(cost = 1): boolean {
    return this.limiters.some((l) => l.canTake(cost));
  }

  take(cost = 1): Decision {
    if (!this.canTake(cost)) {
      // Ask each one for the wait it would impose, without charging anyone.
      let retry = 0;
      let remaining = Infinity;
      for (const l of this.limiters) {
        if (l.canTake(cost)) {
          continue;
        }
        const probe = l.take(cost);
        retry = Math.max(retry, probe.retryAfterMs);
        remaining = Math.min(remaining, probe.remaining);
      }
      return deny(remaining === Infinity ? 0 : remaining, retry);
    }
    let remaining = Infinity;
    for (const l of this.limiters) remaining = Math.min(remaining, l.take(cost).remaining);
    return allow(remaining);
  }

  reset(): void {
    for (const l of this.limiters) l.reset();
  }
}

// -- one limiter per client ---------------------------------------------------------------------------------

interface Slot<L> {
  limiter: L;
  lastSeen: number;
}

export interface RegistryStats {
  clients: number;
  allowed: number;
  denied: number;
  evicted: number;
}

/** Creates a limiter on first use of a key and forgets idle keys. */
export class ClientRegistry<L extends Limiter> {
  private slots = new Map<string, Slot<L>>();
  private counters = { allowed: 0, denied: 0, evicted: 0 };
  private readonly make: () => L;
  private readonly clock: Clock;
  private readonly maxClients: number;

  constructor(make: () => L, clock: Clock = Date.now, maxClients = 10_000) {
    if (maxClients < 1) throw new LimiterError("maxClients must be at least 1");
    this.make = make;
    this.clock = clock;
    this.maxClients = maxClients;
  }

  private slot(key: string): Slot<L> {
    const now = this.clock();
    let slot = this.slots.get(key);
    if (!slot) {
      if (this.slots.size >= this.maxClients) this.evictOldest();
      slot = { limiter: this.make(), lastSeen: now };
      this.slots.set(key, slot);
    }
    slot.lastSeen = now;
    return slot;
  }

  private evictOldest(): void {
    let oldest: string | undefined;
    let oldestSeen = Infinity;
    for (const [key, slot] of this.slots) {
      if (slot.lastSeen < oldestSeen) {
        oldest = key;
        oldestSeen = slot.lastSeen;
      }
    }
    if (oldest !== undefined) {
      this.slots.delete(oldest);
      this.counters.evicted++;
    }
  }

  take(key: string, cost = 1): Decision {
    const decision = this.slot(key).limiter.take(cost);
    if (decision.allowed) this.counters.allowed++;
    else this.counters.denied++;
    return decision;
  }

  /** Forget clients that have not been seen for at least idleMs. */
  evictIdle(idleMs: number): number {
    const now = this.clock();
    let count = 0;
    for (const [key, slot] of [...this.slots]) {
      if (now - slot.lastSeen >= idleMs) {
        this.slots.delete(key);
        count++;
      }
    }
    this.counters.evicted += count;
    return count;
  }

  has(key: string): boolean {
    return this.slots.has(key);
  }

  stats(): RegistryStats {
    return { clients: this.slots.size, ...this.counters };
  }
}

// -- helpers for callers ------------------------------------------------------------------------------------------

/** Wait before retry number `attempt` (0 for the first retry), with optional jitter. */
export function backoffMs(
  attempt: number,
  baseMs = 100,
  capMs = 30_000,
  jitter: (max: number) => number = () => 0,
): number {
  const raw = Math.min(capMs, baseMs * 2 ** attempt);
  return Math.max(0, raw - jitter(raw / 2));
}

/** Seconds for a Retry-After header: whole seconds, rounded up, at least one. */
export function retryAfterSeconds(ms: number): number {
  return Math.max(1, Math.ceil(ms / 1000));
}

/** Read a Retry-After header, given as seconds or as an HTTP date. Returns ms. */
export function parseRetryAfter(value: string, now: number): number | null {
  const text = value.trim();
  if (/^\d+$/.test(text)) return Number(text) * 1000;
  const when = Date.parse(text);
  if (Number.isNaN(when)) return null;
  return Math.max(0, when - now);
}

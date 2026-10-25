// An LRU cache with per-entry time to live, weights and statistics.
//
// Time comes from an injectable clock so tests and callers control it. Entries
// are kept in a doubly linked list: the head is the most recently used and the
// tail is the next one to go.

export type Clock = () => number;

export interface Options<K, V> {
  /** Most entries to keep. */
  capacity: number;
  /** Default time to live in ms. Zero or less means entries never expire. */
  ttl?: number;
  /** Most total weight to keep. Needs `weigh`. */
  maxWeight?: number;
  /** Weight of one value. Defaults to 1. */
  weigh?: (value: V, key: K) => number;
  clock?: Clock;
  onEvict?: (key: K, value: V, reason: EvictReason) => void;
}

export type EvictReason = "capacity" | "weight" | "expired" | "deleted" | "cleared";

export interface Stats {
  hits: number;
  misses: number;
  evictions: number;
  expirations: number;
  sets: number;
}

interface Node<K, V> {
  key: K;
  value: V;
  weight: number;
  expiresAt: number;
  prev: Node<K, V> | null;
  next: Node<K, V> | null;
}

export class CacheError extends Error {}

export class LruCache<K, V> {
  private map = new Map<K, Node<K, V>>();
  private head: Node<K, V> | null = null;
  private tail: Node<K, V> | null = null;
  private totalWeight = 0;
  private readonly opts: Options<K, V>;
  private readonly clock: Clock;
  private counters: Stats = { hits: 0, misses: 0, evictions: 0, expirations: 0, sets: 0 };

  constructor(opts: Options<K, V>) {
    if (!Number.isInteger(opts.capacity) || opts.capacity < 1) {
      throw new CacheError("capacity must be a positive integer");
    }
    if (opts.maxWeight !== undefined && !opts.weigh) {
      throw new CacheError("maxWeight needs a weigh function");
    }
    this.opts = opts;
    this.clock = opts.clock ?? Date.now;
  }

  // -- linked list --------------------------------------------------------------

  private unlink(node: Node<K, V>): void {
    if (node.prev) node.prev.next = node.next;
    else this.head = node.next;
    if (node.next) node.next.prev = node.prev;
    else this.tail = node.prev;
    node.prev = null;
    node.next = null;
  }

  private pushFront(node: Node<K, V>): void {
    node.prev = null;
    node.next = this.head;
    if (this.head) this.head.prev = node;
    this.head = node;
    if (!this.tail) this.tail = node;
  }

  private pushBack(node: Node<K, V>): void {
    node.next = null;
    node.prev = this.tail;
    if (this.tail) this.tail.next = node;
    this.tail = node;
    if (!this.head) this.head = node;
  }

  private touch(node: Node<K, V>): void {
    if (this.head === node) return;
    this.unlink(node);
    this.pushFront(node);
  }

  // -- basic operations ------------------------------------------------------------

  get size(): number {
    return this.map.size;
  }

  get weight(): number {
    return this.totalWeight;
  }

  private expiryFor(ttl: number | undefined): number {
    const life = ttl ?? this.opts.ttl ?? 0;
    return life > 0 ? this.clock() + life : Infinity;
  }

  private isExpired(node: Node<K, V>): boolean {
    return this.clock() > node.expiresAt;
  }

  private weightOf(key: K, value: V): number {
    const w = this.opts.weigh ? this.opts.weigh(value, key) : 1;
    if (!(w >= 0)) throw new CacheError("weight must not be negative");
    return w;
  }

  private drop(node: Node<K, V>, reason: EvictReason): void {
    this.unlink(node);
    this.map.delete(node.key);
    this.totalWeight -= node.weight;
    if (reason === "capacity" || reason === "weight") this.counters.evictions++;
    if (reason === "expired") this.counters.expirations++;
    this.opts.onEvict?.(node.key, node.value, reason);
  }

  /** Read a value and mark it as recently used. */
  get(key: K): V | undefined {
    const node = this.map.get(key);
    if (!node) {
      this.counters.misses++;
      return undefined;
    }
    if (this.isExpired(node)) {
      this.drop(node, "expired");
      this.counters.misses++;
      return undefined;
    }
    this.touch(node);
    this.counters.hits++;
    return node.value;
  }

  /** Read a value without changing the order or the statistics. */
  peek(key: K): V | undefined {
    const node = this.map.get(key);
    if (!node || this.isExpired(node)) return undefined;
    return node.value;
  }

  has(key: K): boolean {
    return this.peek(key) !== undefined;
  }

  /** Store a value. A fresh time to live replaces the old one. */
  set(key: K, value: V, ttl?: number): this {
    const weight = this.weightOf(key, value);
    if (this.opts.maxWeight !== undefined && weight > this.opts.maxWeight) {
      throw new CacheError("value is heavier than the whole cache");
    }
    const existing = this.map.get(key);
    this.counters.sets++;
    if (existing) {
      this.totalWeight += weight - existing.weight;
      existing.value = value;
      existing.weight = weight;
      this.touch(existing);
    } else {
      const node: Node<K, V> = {
        key, value, weight, expiresAt: this.expiryFor(ttl), prev: null, next: null,
      };
      this.map.set(key, node);
      this.totalWeight += weight;
      this.pushFront(node);
    }
    this.trim();
    return this;
  }

  /** Evict from the tail until the limits hold again. */
  private trim(): void {
    while (this.map.size > this.opts.capacity && this.tail) {
      this.drop(this.tail, "capacity");
    }
    const max = this.opts.maxWeight;
    if (max === undefined) return;
    while (this.totalWeight > max && this.tail) {
      this.drop(this.tail, "weight");
    }
  }

  /** Remove one key. Returns true when it was there. */
  delete(key: K): boolean {
    const node = this.map.get(key);
    if (!node) return false;
    this.unlink(node);
    this.map.delete(key);
    this.opts.onEvict?.(node.key, node.value, "deleted");
    return true;
  }

  clear(): void {
    for (let node = this.head; node; node = node.next) {
      this.opts.onEvict?.(node.key, node.value, "cleared");
    }
    this.map.clear();
    this.head = null;
    this.tail = null;
    this.totalWeight = 0;
  }

  // -- computed values ----------------------------------------------------------------

  /** Return the cached value or compute, store and return it. */
  getOrCompute(key: K, compute: (key: K) => V, ttl?: number): V {
    const cached = this.get(key);
    if (cached !== undefined) return cached;
    const value = compute(key);
    this.set(key, value, ttl);
    return value;
  }

  async getOrComputeAsync(key: K, compute: (key: K) => Promise<V>, ttl?: number): Promise<V> {
    const cached = this.get(key);
    if (cached !== undefined) return cached;
    const value = await compute(key);
    this.set(key, value, ttl);
    return value;
  }

  setMany(entries: Iterable<[K, V]>, ttl?: number): this {
    for (const [key, value] of entries) this.set(key, value, ttl);
    return this;
  }

  getMany(keys: Iterable<K>): Map<K, V> {
    const found = new Map<K, V>();
    for (const key of keys) {
      const value = this.get(key);
      if (value !== undefined) found.set(key, value);
    }
    return found;
  }

  // -- time to live ------------------------------------------------------------------------

  /** Ms left before a key expires, Infinity when it never does, undefined when absent. */
  ttlRemaining(key: K): number | undefined {
    const node = this.map.get(key);
    if (!node || this.isExpired(node)) return undefined;
    return node.expiresAt === Infinity ? Infinity : node.expiresAt - this.clock();
  }

  /** Give a key a new time to live without changing its value or place. */
  expire(key: K, ttl: number): boolean {
    const node = this.map.get(key);
    if (!node || this.isExpired(node)) return false;
    node.expiresAt = ttl > 0 ? this.clock() + ttl : Infinity;
    return true;
  }

  /** Drop every expired entry. Returns how many went. */
  purgeExpired(): number {
    let count = 0;
    for (const node of [...this.map.values()]) {
      if (this.isExpired(node)) {
        this.drop(node, "expired");
        count++;
      }
    }
    return count;
  }

  /** The next time something expires, or undefined when nothing will. */
  nextExpiry(): number | undefined {
    let soonest = Infinity;
    for (const node of this.map.values()) {
      if (node.expiresAt < soonest) soonest = node.expiresAt;
    }
    return soonest === Infinity ? undefined : soonest;
  }

  // -- order ---------------------------------------------------------------------------------

  /** Keys from the most to the least recently used. */
  keys(): K[] {
    const out: K[] = [];
    for (let node = this.head; node; node = node.next) out.push(node.key);
    return out;
  }

  values(): V[] {
    const out: V[] = [];
    for (let node = this.head; node; node = node.next) out.push(node.value);
    return out;
  }

  entries(): Array<[K, V]> {
    const out: Array<[K, V]> = [];
    for (let node = this.head; node; node = node.next) out.push([node.key, node.value]);
    return out;
  }

  /** The n most recently used keys, newest first. */
  newestKeys(n: number): K[] {
    const out: K[] = [];
    let node = this.head;
    while (node && out.length < n) {
      out.push(node.key);
      node = node.next;
    }
    return out;
  }

  /** The n least recently used keys, oldest first. */
  oldestKeys(n: number): K[] {
    const out: K[] = [];
    let node = this.tail;
    while (node && out.length < n) {
      out.push(node.key);
      node = node.next;
    }
    return out;
  }

  /** Make a key the next one to be evicted. */
  demote(key: K): boolean {
    const node = this.map.get(key);
    if (!node) return false;
    this.unlink(node);
    this.pushBack(node);
    return true;
  }

  /** Remove and return the least recently used entry. */
  pop(): [K, V] | undefined {
    const node = this.tail;
    if (!node) return undefined;
    this.drop(node, "deleted");
    return [node.key, node.value];
  }

  forEach(fn: (value: V, key: K) => void): void {
    for (const [key, value] of this.entries()) fn(value, key);
  }

  findKey(test: (value: V, key: K) => boolean): K | undefined {
    for (let node = this.head; node; node = node.next) {
      if (test(node.value, node.key)) return node.key;
    }
    return undefined;
  }

  // -- limits ------------------------------------------------------------------------------------

  resize(capacity: number): void {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new CacheError("capacity must be a positive integer");
    }
    this.opts.capacity = capacity;
    this.trim();
  }

  // -- statistics -------------------------------------------------------------------------------------

  stats(): Stats {
    return { ...this.counters };
  }

  resetStats(): void {
    this.counters = { hits: 0, misses: 0, evictions: 0, expirations: 0, sets: 0 };
  }

  /** Share of reads that were hits, from 0 to 1. Zero before the first read. */
  hitRate(): number {
    const reads = this.counters.hits + this.counters.misses;
    return reads === 0 ? 0 : this.counters.hits / reads;
  }

  // -- snapshots ------------------------------------------------------------------------------------------

  /** Live entries oldest first, with the time left on each. */
  snapshot(): Array<{ key: K; value: V; remaining: number }> {
    const out: Array<{ key: K; value: V; remaining: number }> = [];
    for (let node = this.tail; node; node = node.prev) {
      if (this.isExpired(node)) continue;
      const remaining = node.expiresAt === Infinity ? 0 : node.expiresAt - this.clock();
      out.push({ key: node.key, value: node.value, remaining });
    }
    return out;
  }

  /** Load a snapshot, oldest first so the newest ends up at the head. */
  restore(rows: Array<{ key: K; value: V; remaining: number }>): void {
    for (const row of rows) this.set(row.key, row.value, row.remaining);
  }
}

// -- a two level cache --------------------------------------------------------------------------------------

/** A small fast cache in front of a bigger one. A hit in the second promotes. */
export class TieredCache<K, V> {
  readonly first: LruCache<K, V>;
  readonly second: LruCache<K, V>;
  private promotions = 0;

  constructor(first: LruCache<K, V>, second: LruCache<K, V>) {
    this.first = first;
    this.second = second;
  }

  get(key: K): V | undefined {
    const near = this.first.get(key);
    if (near !== undefined) return near;
    const far = this.second.get(key);
    if (far === undefined) return undefined;
    this.first.set(key, far, this.second.ttlRemaining(key));
    this.promotions++;
    return far;
  }

  set(key: K, value: V, ttl?: number): void {
    this.first.set(key, value, ttl);
    this.second.set(key, value, ttl);
  }

  delete(key: K): boolean {
    const a = this.first.delete(key);
    const b = this.second.delete(key);
    return a || b;
  }

  get promoted(): number {
    return this.promotions;
  }

  hitRate(): number {
    const a = this.first.stats();
    const b = this.second.stats();
    const hits = a.hits;
    const reads = a.hits + a.misses;
    return reads === 0 ? 0 : hits / reads;
  }
}

// -- a memoizer built on the cache ------------------------------------------------------------------------------

/** Wrap a function of one argument so repeat calls hit the cache. */
export function memoize<A, R>(
  fn: (arg: A) => R,
  opts: { capacity: number; ttl?: number; clock?: Clock },
): ((arg: A) => R) & { cache: LruCache<A, R> } {
  const cache = new LruCache<A, R>(opts);
  const wrapped = (arg: A): R => cache.getOrCompute(arg, fn);
  return Object.assign(wrapped, { cache });
}

// -- a namespaced view ---------------------------------------------------------------------------------------------

/** Share one cache between several users by prefixing every key. */
export class NamespacedCache<V> {
  private readonly cache: LruCache<string, V>;
  private readonly prefix: string;

  constructor(cache: LruCache<string, V>, namespace: string) {
    if (namespace.includes(":")) throw new CacheError("namespace must not contain ':'");
    this.cache = cache;
    this.prefix = namespace + ":";
  }

  private full(key: string): string {
    return this.prefix + key;
  }

  get(key: string): V | undefined {
    return this.cache.get(this.full(key));
  }

  set(key: string, value: V, ttl?: number): void {
    this.cache.set(this.full(key), value, ttl);
  }

  has(key: string): boolean {
    return this.cache.has(this.full(key));
  }

  delete(key: string): boolean {
    return this.cache.delete(this.full(key));
  }

  keys(): string[] {
    return this.cache
      .keys()
      .filter((k) => k.startsWith(this.prefix))
      .map((k) => k.slice(this.prefix.length));
  }

  /** Remove every key of this namespace. Returns how many went. */
  clear(): number {
    const mine = this.cache.keys().filter((k) => k.startsWith(this.prefix));
    for (const key of mine) this.cache.delete(key);
    return mine.length;
  }
}

// -- a rolling hit rate -----------------------------------------------------------------------------------------------

/** Hit rate over the last `size` reads instead of over all time. */
export class StatsWindow {
  private readonly slots: boolean[];
  private next = 0;
  private filled = 0;

  constructor(size: number) {
    if (!Number.isInteger(size) || size < 1) throw new CacheError("size must be positive");
    this.slots = new Array<boolean>(size).fill(false);
  }

  record(hit: boolean): void {
    this.slots[this.next] = hit;
    this.next = (this.next + 1) % this.slots.length;
    if (this.filled < this.slots.length) this.filled++;
  }

  get count(): number {
    return this.filled;
  }

  rate(): number {
    if (this.filled === 0) return 0;
    let hits = 0;
    for (let i = 0; i < this.filled; i++) if (this.slots[i]) hits++;
    return hits / this.filled;
  }
}

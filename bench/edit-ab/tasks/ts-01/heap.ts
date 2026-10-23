// A binary min-heap priority queue.

export type Comparator<T> = (a: T, b: T) => number;

export class PriorityQueue<T> {
  private heap: T[] = [];
  private compare: Comparator<T>;

  constructor(compare: Comparator<T>, items: Iterable<T> = []) {
    this.compare = compare;
    for (const item of items) {
      this.push(item);
    }
  }

  get size(): number {
    return this.heap.length;
  }

  isEmpty(): boolean {
    return this.heap.length === 0;
  }

  peek(): T | undefined {
    return this.heap[0];
  }

  push(item: T): void {
    this.heap.push(item);
    this.siftUp(this.heap.length - 1);
  }

  pushAll(items: Iterable<T>): void {
    for (const item of items) {
      this.push(item);
    }
  }

  pop(): T | undefined {
    if (this.heap.length === 0) {
      return undefined;
    }
    const top = this.heap[0];
    const last = this.heap.pop() as T;
    if (this.heap.length > 0) {
      this.heap[0] = last;
      this.siftDown(0);
    }
    return top;
  }

  // Remove and return everything in priority order.
  drain(): T[] {
    const out: T[] = [];
    while (!this.isEmpty()) {
      out.push(this.pop() as T);
    }
    return out;
  }

  // The n best items in priority order, leaving the queue as it was.
  top(n: number): T[] {
    const copy = new PriorityQueue<T>(this.compare, this.heap);
    const out: T[] = [];
    while (out.length < n && !copy.isEmpty()) {
      out.push(copy.pop() as T);
    }
    return out;
  }

  private swap(i: number, j: number): void {
    const tmp = this.heap[i];
    this.heap[i] = this.heap[j];
    this.heap[j] = tmp;
  }

  private siftUp(start: number): void {
    let i = start;
    while (i > 0) {
      const parent = i >> 1;
      if (this.compare(this.heap[i], this.heap[parent]) >= 0) {
        break;
      }
      this.swap(i, parent);
      i = parent;
    }
  }

  private siftDown(start: number): void {
    const n = this.heap.length;
    let i = start;
    for (;;) {
      const left = 2 * i + 1;
      const right = left + 1;
      let best = i;
      if (left < n && this.compare(this.heap[left], this.heap[best]) < 0) {
        best = left;
      }
      if (right < n && this.compare(this.heap[right], this.heap[i]) < 0) {
        best = right;
      }
      if (best === i) {
        break;
      }
      this.swap(i, best);
      i = best;
    }
  }
}

export interface Job {
  name: string;
  priority: number;
}

// Order jobs by priority, then by name so the order is stable.
export function byPriority(a: Job, b: Job): number {
  if (a.priority !== b.priority) {
    return a.priority - b.priority;
  }
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
}

export function schedule(jobs: Job[]): string[] {
  const queue = new PriorityQueue<Job>(byPriority, jobs);
  return queue.drain().map((job) => job.name);
}

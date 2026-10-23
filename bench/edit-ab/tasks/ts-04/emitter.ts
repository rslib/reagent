// A typed-ish event emitter with once() and wildcard listeners.

export type Listener = (...args: unknown[]) => void;

interface Entry {
  listener: Listener;
  once: boolean;
}

export class Emitter {
  private events = new Map<string, Entry[]>();
  private maxListeners: number;

  constructor(maxListeners = 10) {
    this.maxListeners = maxListeners;
  }

  on(event: string, listener: Listener): this {
    return this.add(event, listener, false);
  }

  once(event: string, listener: Listener): this {
    return this.add(event, listener, true);
  }

  private add(event: string, listener: Listener, once: boolean): this {
    const list = this.events.get(event) ?? [];
    if (list.length >= this.maxListeners) {
      throw new Error(`too many listeners for ${event}`);
    }
    list.push({ listener, once });
    this.events.set(event, list);
    return this;
  }

  off(event: string, listener: Listener): this {
    const list = this.events.get(event);
    if (!list) {
      return this;
    }
    const index = list.findIndex((entry) => entry.listener === listener);
    if (index >= 0) {
      list.splice(index, 1);
    }
    if (list.length === 0) {
      this.events.delete(event);
    }
    return this;
  }

  // Call every listener of the event in the order they were added. Returns
  // true when at least one listener ran.
  emit(event: string, ...args: unknown[]): boolean {
    const list = this.events.get(event);
    if (!list || list.length === 0) {
      return false;
    }
    for (const entry of [...list]) {
      if (entry.once) {
        this.off(entry.listener, event);
      }
      entry.listener(args[0]);
    }
    return true;
  }

  listenerCount(event: string): number {
    return this.events.get(event)?.length ?? 0;
  }

  eventNames(): string[] {
    return [...this.events.keys()];
  }

  removeAll(event?: string): this {
    if (event === undefined) {
      this.events.clear();
    } else {
      this.events.delete(event);
    }
    return this;
  }

  // Resolve with the arguments of the next emit of the event.
  next(event: string): Promise<unknown[]> {
    return new Promise((resolve) => {
      this.once(event, (...args) => resolve(args));
    });
  }
}

// Record every event name and its arguments into a list, for tests and logs.
export function recorder(emitter: Emitter, names: string[]): Array<[string, unknown[]]> {
  const log: Array<[string, unknown[]]> = [];
  for (const name of names) {
    emitter.on(name, (...args) => {
      log.push([name, args]);
    });
  }
  return log;
}

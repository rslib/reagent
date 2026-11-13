// The `timers/promises` module of one plugin: reagent's own timers, never
// LLRT's, so every wait of a plugin runs under the watchdog like any other.
import {
  clearInterval as stopInterval,
  clearTimeout as stopTimer,
  setInterval as repeat,
  setTimeout as later,
} from "reagent:node/timers";

// The error a wait the signal ended rejects with, named as Node names it.
function aborted() {
  const error = new Error("The operation was aborted");
  error.name = "AbortError";
  error.code = "ABORT_ERR";
  return error;
}

// The signal of a call's options, when it has one.
function signalOf(options) {
  return options === undefined || options === null ? undefined : options.signal;
}

export function setTimeout(ms, value, options) {
  const signal = signalOf(options);
  if (signal !== undefined && signal.aborted) return Promise.reject(aborted());
  return new Promise((resolve, reject) => {
    let off = () => {};
    const timer = later(() => {
      off();
      resolve(value);
    }, ms);
    if (signal !== undefined) {
      const onAbort = () => {
        off();
        stopTimer(timer);
        reject(aborted());
      };
      signal.addEventListener("abort", onAbort, { once: true });
      off = () => signal.removeEventListener("abort", onAbort);
    }
  });
}

export function setImmediate(value, options) {
  return setTimeout(0, value, options);
}

// A wait that repeats until the signal aborts or the loop that reads it ends.
export async function* setInterval(ms, value, options) {
  const signal = signalOf(options);
  if (signal !== undefined && signal.aborted) throw aborted();
  let wake;
  let failure;
  const tick = () => {
    if (wake === undefined) return;
    const resolve = wake;
    wake = undefined;
    resolve();
  };
  const timer = repeat(tick, ms);
  const onAbort = () => {
    failure = aborted();
    tick();
  };
  if (signal !== undefined) signal.addEventListener("abort", onAbort, { once: true });
  try {
    for (;;) {
      await new Promise((resolve) => {
        wake = resolve;
      });
      if (failure !== undefined) throw failure;
      yield value;
    }
  } finally {
    stopInterval(timer);
    if (signal !== undefined) signal.removeEventListener("abort", onAbort);
  }
}

export const scheduler = {
  wait(ms, options) {
    return setTimeout(ms, undefined, options);
  },
  yield() {
    return setImmediate(undefined);
  },
};

export default { scheduler, setImmediate, setInterval, setTimeout };

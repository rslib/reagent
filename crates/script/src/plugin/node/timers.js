// The `timers` module of one plugin: reagent's own timers, which the runtime
// already runs under its watchdog, never LLRT's.
export const setTimeout = globalThis.setTimeout;
export const clearTimeout = globalThis.clearTimeout;
export const setInterval = globalThis.setInterval;
export const clearInterval = globalThis.clearInterval;

export default { setTimeout, clearTimeout, setInterval, clearInterval };

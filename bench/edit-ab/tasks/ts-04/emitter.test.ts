import test from "node:test";
import assert from "node:assert/strict";
import { Emitter, recorder } from "./emitter.ts";

test("listeners get every argument", () => {
  const e = new Emitter();
  const seen: unknown[][] = [];
  e.on("data", (...args) => seen.push(args));
  assert.equal(e.emit("data", 1, "two", 3), true);
  assert.deepEqual(seen, [[1, "two", 3]]);
});

test("once runs a single time", () => {
  const e = new Emitter();
  let calls = 0;
  e.once("ping", () => {
    calls += 1;
  });
  e.emit("ping");
  e.emit("ping");
  assert.equal(calls, 1);
  assert.equal(e.listenerCount("ping"), 0);
});

test("once and on side by side", () => {
  const e = new Emitter();
  const order: string[] = [];
  e.once("x", () => order.push("once"));
  e.on("x", () => order.push("on"));
  e.emit("x");
  e.emit("x");
  assert.deepEqual(order, ["once", "on", "on"]);
});

test("off removes a listener", () => {
  const e = new Emitter();
  let calls = 0;
  const f = () => {
    calls += 1;
  };
  e.on("a", f).on("a", f);
  e.off("a", f);
  e.emit("a");
  assert.equal(calls, 1);
});

test("emit with no listeners is false", () => {
  assert.equal(new Emitter().emit("nothing"), false);
});

test("next resolves with the arguments", async () => {
  const e = new Emitter();
  const p = e.next("ready");
  e.emit("ready", 7, 8);
  assert.deepEqual(await p, [7, 8]);
});

test("recorder logs events", () => {
  const e = new Emitter();
  const log = recorder(e, ["a", "b"]);
  e.emit("a", 1, 2);
  e.emit("b");
  assert.deepEqual(log, [["a", [1, 2]], ["b", []]]);
});

import test from "node:test";
import assert from "node:assert/strict";
import { evaluate } from "./calc.ts";

test("precedence", () => {
  assert.equal(evaluate("1 + 2 * 3"), 7);
  assert.equal(evaluate("(1 + 2) * 3"), 9);
});

test("division", () => {
  assert.equal(evaluate("20 / 4"), 5);
  assert.equal(evaluate("2 * 9 / 3"), 6);
  assert.equal(evaluate("100 / 10 / 5"), 2);
});

test("division by zero throws", () => {
  assert.throws(() => evaluate("1 / 0"), /division by zero/);
});

test("unary minus", () => {
  assert.equal(evaluate("-3 + 5"), 2);
  assert.equal(evaluate("2 * -4"), -8);
  assert.equal(evaluate("--2"), 2);
  assert.equal(evaluate("-(1 + 2)"), -3);
});

test("power is right associative", () => {
  assert.equal(evaluate("2 ^ 3 ^ 2"), 512);
});

test("errors", () => {
  assert.throws(() => evaluate("1 +"));
  assert.throws(() => evaluate("(1"));
  assert.throws(() => evaluate("1 $ 2"));
});

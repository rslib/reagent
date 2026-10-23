import test from "node:test";
import assert from "node:assert/strict";
import { add, allocate, format, money, percentOf, splitEvenly, subtract, sum } from "./money.ts";

test("add and subtract", () => {
  assert.equal(add(money(1.25), money(2.5)).cents, 375);
  assert.equal(subtract(money(5), money(1.5)).cents, 350);
});

test("subtract can go negative", () => {
  assert.equal(subtract(money(1), money(3)).cents, -200);
});

test("currency mismatch throws", () => {
  assert.throws(() => add(money(1), money(1, "EUR")));
  assert.throws(() => subtract(money(1), money(1, "EUR")));
});

test("format groups thousands", () => {
  assert.equal(format(money(1234567.5)), "$1,234,567.50");
  assert.equal(format(money(0.07)), "$0.07");
});

test("format negative amounts", () => {
  assert.equal(format(money(-12.3)), "-$12.30");
});

test("splitEvenly keeps the total", () => {
  const parts = splitEvenly(money(1), 3);
  assert.deepEqual(parts.map((p) => p.cents), [34, 33, 33]);
  assert.equal(sum(parts).cents, 100);
});

test("allocate by weights", () => {
  assert.deepEqual(allocate(money(1), [1, 1, 2]).map((p) => p.cents), [25, 25, 50]);
  assert.deepEqual(allocate(money(0.05), [1, 1]).map((p) => p.cents), [3, 2]);
});

test("percentOf", () => {
  assert.equal(percentOf(money(19.99), 10).cents, 200);
});

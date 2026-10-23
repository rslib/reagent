import test from "node:test";
import assert from "node:assert/strict";
import { add, determinant2, equals, format, identity, multiply, power, trace, transpose } from "./matrix.ts";

test("identity", () => {
  assert.deepEqual(identity(3), [
    [1, 0, 0],
    [0, 1, 0],
    [0, 0, 1],
  ]);
  assert.equal(trace(identity(4)), 4);
});

test("multiply by identity keeps the matrix", () => {
  const m = [
    [1, 2],
    [3, 4],
  ];
  assert.ok(equals(multiply(m, identity(2)), m));
  assert.ok(equals(multiply(identity(2), m), m));
});

test("multiply rectangular", () => {
  const a = [
    [1, 2, 3],
    [4, 5, 6],
  ];
  const b = [
    [7, 8],
    [9, 10],
    [11, 12],
  ];
  assert.deepEqual(multiply(a, b), [
    [58, 64],
    [139, 154],
  ]);
});

test("shape mismatch throws", () => {
  assert.throws(() => multiply([[1, 2]], [[1, 2]]));
  assert.throws(() => add([[1]], [[1, 2]]));
});

test("power", () => {
  const fib = [
    [1, 1],
    [1, 0],
  ];
  assert.deepEqual(power(fib, 10), [
    [89, 55],
    [55, 34],
  ]);
  assert.deepEqual(power(fib, 0), identity(2));
});

test("transpose and determinant", () => {
  assert.deepEqual(transpose([[1, 2, 3]]), [[1], [2], [3]]);
  assert.equal(determinant2([[3, 8], [4, 6]]), -14);
  assert.equal(format([[1, 2], [3, 4]]), "1 2\n3 4");
});

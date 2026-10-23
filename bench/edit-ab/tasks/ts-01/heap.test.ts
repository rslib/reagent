import test from "node:test";
import assert from "node:assert/strict";
import { PriorityQueue, schedule } from "./heap.ts";

const numeric = (a: number, b: number) => a - b;

test("drain returns sorted order", () => {
  const q = new PriorityQueue<number>(numeric, [9, 4, 7, 1, 8, 2, 6, 3, 5, 0]);
  assert.deepEqual(q.drain(), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
});

test("sorts a longer shuffled list", () => {
  const values: number[] = [];
  let x = 17;
  for (let i = 0; i < 200; i++) {
    x = (x * 31 + 11) % 1009;
    values.push(x);
  }
  const q = new PriorityQueue<number>(numeric, values);
  assert.deepEqual(q.drain(), [...values].sort(numeric));
});

test("peek and size", () => {
  const q = new PriorityQueue<number>(numeric);
  assert.equal(q.peek(), undefined);
  q.pushAll([5, 3, 8]);
  assert.equal(q.peek(), 3);
  assert.equal(q.size, 3);
});

test("top leaves the queue alone", () => {
  const q = new PriorityQueue<number>(numeric, [5, 1, 4, 2, 3]);
  assert.deepEqual(q.top(3), [1, 2, 3]);
  assert.equal(q.size, 5);
});

test("schedule orders by priority then name", () => {
  const names = schedule([
    { name: "c", priority: 2 },
    { name: "a", priority: 3 },
    { name: "b", priority: 2 },
    { name: "d", priority: 1 },
    { name: "e", priority: 3 },
    { name: "f", priority: 1 },
  ]);
  assert.deepEqual(names, ["d", "f", "b", "c", "a", "e"]);
});

import test from "node:test";
import assert from "node:assert/strict";
import {
  describePage,
  eachPage,
  nextPage,
  pageBounds,
  pageWindow,
  paginate,
  prevPage,
  totalPages,
} from "./paginate.ts";

const nums = Array.from({ length: 25 }, (_, i) => i + 1);

test("totalPages rounds up", () => {
  assert.equal(totalPages(25, 10), 3);
  assert.equal(totalPages(20, 10), 2);
  assert.equal(totalPages(0, 10), 1);
});

test("last page holds the remainder", () => {
  const page = paginate(nums, 3, 10);
  assert.deepEqual(page.items, [21, 22, 23, 24, 25]);
  assert.equal(page.hasNext, false);
  assert.equal(page.hasPrev, true);
});

test("first page", () => {
  const page = paginate(nums, 1, 10);
  assert.deepEqual(page.items, nums.slice(0, 10));
  assert.equal(page.totalPages, 3);
});

test("page numbers are clamped", () => {
  assert.equal(paginate(nums, 99, 10).page, 3);
  assert.equal(paginate(nums, -4, 10).page, 1);
});

test("prev and next", () => {
  assert.equal(nextPage(2, 5), 3);
  assert.equal(nextPage(5, 5), 5);
  assert.equal(prevPage(2, 5), 1);
  assert.equal(prevPage(1, 5), 1);
});

test("bounds and description", () => {
  assert.deepEqual(pageBounds(3, 10, 25), { first: 20, last: 24 });
  assert.equal(describePage(paginate(nums, 2, 10)), "items 11-20 of 25");
  assert.equal(describePage(paginate([], 1, 10)), "no items");
});

test("pager window", () => {
  assert.deepEqual(pageWindow(6, 20), [4, 5, 6, 7, 8]);
  assert.deepEqual(pageWindow(1, 20), [1, 2, 3, 4, 5]);
  assert.deepEqual(pageWindow(20, 20), [16, 17, 18, 19, 20]);
  assert.deepEqual(pageWindow(2, 3), [1, 2, 3]);
});

test("eachPage", () => {
  assert.deepEqual([...eachPage([1, 2, 3, 4, 5], 2)], [[1, 2], [3, 4], [5]]);
});

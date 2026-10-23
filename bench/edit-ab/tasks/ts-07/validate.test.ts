import test from "node:test";
import assert from "node:assert/strict";
import {
  firstProblem,
  isValid,
  validateAge,
  validateEmail,
  validatePassword,
  validateSignup,
  validateUsername,
} from "./validate.ts";

test("good email", () => {
  assert.deepEqual(validateEmail("ann@example.com"), []);
});

test("empty email is reported once", () => {
  assert.deepEqual(validateEmail("  "), ["email is required"]);
});

test("bad emails", () => {
  assert.equal(validateEmail("ann.example.com").length, 1);
  assert.equal(validateEmail("a@b@c.com").length, 1);
  assert.equal(validateEmail("ann@example").length, 1);
});

test("age range", () => {
  assert.deepEqual(validateAge(30), []);
  assert.equal(validateAge(-1).length, 1);
  assert.equal(validateAge(151).length, 1);
  assert.equal(validateAge(0).length, 0);
});

test("password default length is 8", () => {
  assert.deepEqual(validatePassword("Abcdef1"), ["password needs at least 8 characters"]);
  assert.deepEqual(validatePassword("Abcdefg1"), []);
});

test("password rules can be relaxed", () => {
  assert.deepEqual(validatePassword("abc", { minLength: 3, requireDigit: false, requireUpper: false }), []);
});

test("username", () => {
  assert.deepEqual(validateUsername("ann_1"), []);
  assert.equal(validateUsername("Ann").length, 1);
  assert.equal(validateUsername("ab").length, 1);
});

test("whole form", () => {
  const good = { email: "a@b.co", age: 20, password: "Passw0rdX", username: "ann" };
  assert.ok(isValid(good));
  assert.equal(firstProblem(good), null);
  const bad = { ...good, age: 200, password: "short" };
  assert.deepEqual(Object.keys(validateSignup(bad)), ["age", "password"]);
});

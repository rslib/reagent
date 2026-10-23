import test from "node:test";
import assert from "node:assert/strict";
import { initials, isSlug, slugify, uniqueSlug, unslug } from "./slug.ts";

test("basic slug", () => {
  assert.equal(slugify("Hello, World!"), "hello-world");
});

test("runs of symbols collapse to one separator", () => {
  assert.equal(slugify("a  --  b ... c"), "a-b-c");
});

test("accents are dropped", () => {
  assert.equal(slugify("Café crème"), "cafe-creme");
});

test("default max length is 50", () => {
  const title = "the quick brown fox jumps over the lazy dogs and cats";
  const slug = slugify(title);
  assert.equal(slug.length, 50);
  assert.equal(slug, "the-quick-brown-fox-jumps-over-the-lazy-dogs-and-c");
});

test("max length keeps exactly that many characters", () => {
  assert.equal(slugify("abcdefghij", { maxLength: 5 }), "abcde");
});

test("no separator left at the cut", () => {
  assert.equal(slugify("abcd efgh", { maxLength: 5 }), "abcd");
});

test("custom separator", () => {
  assert.equal(slugify("one two  three", { separator: "_" }), "one_two_three");
});

test("unique slugs", () => {
  const taken = new Set(["my-post", "my-post-2"]);
  assert.equal(uniqueSlug("My Post", taken), "my-post-3");
  assert.equal(uniqueSlug("Other", taken), "other");
});

test("helpers", () => {
  assert.equal(unslug("my-first-post"), "My First Post");
  assert.ok(isSlug("a-b-1"));
  assert.ok(!isSlug("a--b"));
  assert.equal(initials("Ada Lovelace-King"), "ALK");
});

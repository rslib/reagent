import test from "node:test";
import assert from "node:assert/strict";
import {
  bullets,
  breakWords,
  columns,
  columnWidths,
  fitWidths,
  justifyLine,
  layoutDocument,
  layoutParagraph,
  leader,
  renderTable,
  textStats,
  wrapWithPrefix,
  LayoutError,
  boxed,
  dedent,
  definitions,
  header,
  indentBlock,
  numbered,
  paginate,
  rule,
  ruler,
  fits,
} from "./layout.ts";
import {
  center,
  charWidth,
  expandTabs,
  maxLineWidth,
  padEnd,
  padStart,
  sliceByWidth,
  sliceEndByWidth,
  splitLongWord,
  stringWidth,
  stripAnsi,
  tokenize,
  truncate,
  truncateMiddle,
  words,
} from "./measure.ts";

test("widths of narrow, wide and zero width characters", () => {
  assert.equal(stringWidth("hello"), 5);
  assert.equal(stringWidth("日本語"), 6);
  assert.equal(stringWidth("é"), 1);
  assert.equal(stringWidth("aͯb"), 2);
  assert.equal(stringWidth("\u001b[31mred\u001b[0m"), 3);
  assert.equal(charWidth(0x1f600), 2);
  assert.equal(stripAnsi("\u001b[1;32mok\u001b[0m"), "ok");
});

test("slicing never splits a wide character", () => {
  assert.equal(sliceByWidth("日本語", 4), "日本");
  assert.equal(sliceByWidth("日本語", 5), "日本");
  assert.equal(sliceByWidth("abc", 3), "abc");
  assert.equal(sliceByWidth("abc", 0), "");
  assert.equal(sliceEndByWidth("日本語", 4), "本語");
  assert.equal(sliceEndByWidth("abcdef", 2), "ef");
});

test("truncate and truncateMiddle", () => {
  assert.equal(truncate("hello world", 8), "hello w…");
  assert.equal(truncate("short", 8), "short");
  assert.equal(truncate("exactly8", 8), "exactly8");
  assert.equal(truncate("日本語です", 6), "日本…");
  assert.equal(truncate("hello", 3, "..."), "...");
  assert.equal(truncateMiddle("abcdefghij", 7), "abc…hij");
  assert.equal(truncateMiddle("abcdefghij", 6), "abc…ij");
});

test("padding counts columns", () => {
  assert.equal(padStart("ab", 5), "   ab");
  assert.equal(padEnd("ab", 5), "ab   ");
  assert.equal(padEnd("日本", 6), "日本  ");
  assert.equal(padStart("日本", 6, "."), "..日本");
  assert.equal(padEnd("abcdef", 3), "abcdef");
  assert.equal(padStart("x", 4, "ab"), "abx");
});

test("padEnd keeps the text on the left", () => {
  assert.equal(padEnd("id", 6, "."), "id....");
  assert.equal(padEnd("日", 5, "-"), "日---");
});

test("center puts the odd column on the right", () => {
  assert.equal(center("ab", 5), " ab  ");
  assert.equal(center("abc", 3), "abc");
  assert.equal(center("日", 6, "*"), "**日**");
});

test("tabs and tokens", () => {
  assert.equal(expandTabs("a\tb", 4), "a   b");
  assert.equal(expandTabs("abcd\te", 4), "abcd    e");
  assert.equal(expandTabs("日\tx", 4), "日  x");
  assert.equal(expandTabs("a\nb\tc", 4), "a\nb   c");
  assert.deepEqual(
    tokenize("hi  there\nyou").map((t) => [t.kind, t.text]),
    [["word", "hi"], ["space", "  "], ["word", "there"], ["newline", "\n"], ["word", "you"]],
  );
  assert.deepEqual(words("  a  b "), ["a", "b"]);
  assert.equal(maxLineWidth("ab\nabcd\nabc"), 4);
});

test("long words are cut by width", () => {
  assert.deepEqual(splitLongWord("abcdefgh", 3), ["abc", "def", "gh"]);
  assert.deepEqual(splitLongWord("日本語", 4), ["日本", "語"]);
  assert.deepEqual(splitLongWord("日本", 1), ["日", "本"]);
});

test("greedy breaking fills a line exactly", () => {
  const lines = breakWords(["aaa", "bb", "c", "dddd"], 6, 6);
  assert.deepEqual(lines, [["aaa", "bb"], ["c", "dddd"]]);
  assert.deepEqual(breakWords(["ab", "cd", "ef"], 5, 5), [["ab", "cd"], ["ef"]]);
});

test("a first line can be narrower than the rest", () => {
  assert.deepEqual(breakWords(["aa", "bb", "cc", "dd"], 4, 8), [["aa"], ["bb", "cc", "dd"]]);
  assert.deepEqual(breakWords(["aa", "bb", "cc", "dd"], 2, 5), [["aa"], ["bb", "cc"], ["dd"]]);
});

test("a word wider than the line is cut", () => {
  assert.deepEqual(breakWords(["xx", "abcdefghij", "yy"], 6, 6), [
    ["xx"],
    ["abcdef"],
    ["ghij"],
    ["yy"],
  ]);
});

test("justify hands the spare columns to the left gaps first", () => {
  assert.equal(justifyLine(["a", "b", "c"], 8), "a   b  c");
  assert.equal(justifyLine(["aa", "bb", "cc", "dd"], 12), "aa  bb cc dd");
  assert.equal(justifyLine(["one"], 9), "one");
  assert.equal(justifyLine(["ab", "cd"], 6), "ab  cd");
});

test("justified text is as wide as the page except the last line", () => {
  const text = "the quick brown fox jumps over the lazy dog and keeps running far away";
  const lines = layoutParagraph(text, { width: 20, align: "justify" });
  for (const line of lines.slice(0, -1)) assert.equal(stringWidth(line), 20);
  assert.ok(stringWidth(lines[lines.length - 1]) <= 20);
  assert.equal(words(lines.join(" ")).join(" "), text);
});

test("paragraph with indent and hanging indent", () => {
  const lines = layoutParagraph("one two three four five six", { width: 12, indent: 4, hanging: 2 });
  assert.deepEqual(lines, ["    one two", "  three four", "  five six"]);
});

test("alignment", () => {
  assert.deepEqual(layoutParagraph("ab cd", { width: 8, align: "right" }), ["   ab cd"]);
  assert.deepEqual(layoutParagraph("ab cd", { width: 9, align: "center" }), ["  ab cd  "]);
});

test("maxLines cuts with an ellipsis", () => {
  const lines = layoutParagraph("aaa bbb ccc ddd eee", { width: 7, maxLines: 2 });
  assert.deepEqual(lines, ["aaa bbb", "ccc dd…"]);
});

test("bad widths", () => {
  assert.throws(() => layoutParagraph("x", { width: 0 }), LayoutError);
  assert.throws(() => layoutParagraph("x", { width: 4, indent: 4 }), LayoutError);
});

test("documents keep paragraphs apart", () => {
  const out = layoutDocument("aa bb cc\n\n\n  dd ee  \nff", { width: 5 });
  assert.equal(out, "aa bb\ncc\n\ndd ee\nff");
});

test("prefix and bullets", () => {
  assert.deepEqual(wrapWithPrefix("aa bb cc", 7, "> "), ["> aa bb", "> cc"]);
  assert.deepEqual(bullets(["aa bb cc", "dd"], 8), ["- aa bb", "  cc", "- dd"]);
});

test("leader dots", () => {
  assert.equal(leader("Intro", 3, 14), "Intro....... 3");
  assert.equal(leader("A very long chapter title", 12, 14), "A very lo…. 12");
  assert.equal(stringWidth(leader("A very long chapter title", 12, 14)), 14);
});

test("columns fill top to bottom", () => {
  const lines = ["1", "2", "3", "4", "5", "6", "7"];
  assert.deepEqual(columns(lines, 1, 3, 1), ["1 4 7", "2 5", "3 6"]);
  assert.deepEqual(columns(["a", "b", "c", "d"], 3, 2, 2), ["a    c", "b    d"]);
  assert.deepEqual(columns(["a", "b", "c"], 3, 2, 2), ["a    c", "b"]);
  assert.throws(() => columns(["a"], 3, 0), LayoutError);
});

test("tables", () => {
  const rows = [["name", "qty"], ["apple", "3"], ["kiwi", "12"]];
  assert.deepEqual(columnWidths(rows), [5, 3]);
  assert.deepEqual(renderTable(rows, { align: ["left", "right"], header: true }), [
    "name   qty",
    "-----  ---",
    "apple    3",
    "kiwi    12",
  ]);
  assert.deepEqual(renderTable(rows, { widths: [4, 3], separator: " | " }), [
    "name | qty",
    "app… | 3",
    "kiwi | 12",
  ]);
});

test("fitting a table into a width", () => {
  assert.deepEqual(fitWidths([10, 4, 6], 18, 2), [5, 4, 5]);
  assert.throws(() => fitWidths([1, 1], 2, 2), LayoutError);
});

test("text statistics", () => {
  const s = textStats("one two\nthree", 2);
  assert.deepEqual(s, { words: 3, lines: 2, longest: 7, readingMinutes: 1.5 });
  assert.equal(textStats("").lines, 0);
});

test("rules, indents and dedent", () => {
  assert.equal(rule(5), "-----");
  assert.equal(rule(5, "日"), "日日 ");
  assert.equal(indentBlock("a\n\nb", 2), "  a\n\n  b");
  assert.equal(dedent("    a\n      b\n\n    c"), "a\n  b\n\nc");
});

test("boxes, lists and headers", () => {
  assert.deepEqual(boxed(["ab", "abcdef"], 4), ["+------+", "| ab   |", "| abc… |", "+------+"]);
  assert.deepEqual(boxed(["x"], 8, "T"), ["+- T ------+", "| x        |", "+----------+"]);
  assert.deepEqual(numbered(["aa bb cc", "dd"], 8, 9), [" 9. aa", "    bb", "    cc", "10. dd"]);
  assert.deepEqual(definitions([["ab", "one two three"], ["c", "x"]], 12), ["ab  one two", "    three", "c   x"]);
  assert.equal(header("Title", "May 1", 14), "Title    May 1");
  assert.equal(header("A long title here", "May 1", 14), "A long … May 1");
  assert.deepEqual(paginate(["", "a", "b", "", "c"], 2), [["a", "b"], ["c"]]);
});

test("ruler and fits", () => {
  assert.equal(ruler(12), "....+....1..");
  assert.equal(fits("abc\nabcd", 4), true);
  assert.equal(fits("abc\nabcde", 4), false);
});

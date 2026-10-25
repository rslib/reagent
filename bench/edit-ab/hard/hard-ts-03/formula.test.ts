import test from "node:test";
import assert from "node:assert/strict";
import {
  CYCLE,
  DIV0,
  FormulaError,
  FormulaSyntaxError,
  NAME,
  Sheet,
  VALUE,
  colIndex,
  colName,
  evaluate,
  parse,
  parseRef,
  referencedCells,
  refName,
  tokenize,
  toBool,
  toNumber,
  toText,
} from "./formula.ts";
import type { Scalar } from "./formula.ts";

function calc(src: string): unknown {
  return evaluate(parse(src), { cell: () => "" as Scalar });
}

test("numbers are read in every common form", () => {
  assert.deepEqual(
    tokenize("1 2.5 .5 1e3 2.5E+2 1e-3").filter((t) => t.kind === "num").map((t) => Number(t.text)),
    [1, 2.5, 0.5, 1000, 250, 0.001],
  );
  assert.equal(calc("1e-3"), 0.001);
  assert.equal(calc("5e-1 + 1"), 1.5);
  assert.equal(calc(".5+.25"), 0.75);
});

test("a lone e after a number is a name, not an exponent", () => {
  assert.throws(() => parse("2e"), FormulaSyntaxError);
});

test("strings keep doubled quotes", () => {
  assert.equal(calc('"say ""hi"""'), 'say "hi"');
  assert.throws(() => tokenize('"open'), FormulaSyntaxError);
  assert.throws(() => tokenize("1 # 2"), FormulaSyntaxError);
});

test("column letters and names", () => {
  assert.equal(colIndex("A"), 1);
  assert.equal(colIndex("Z"), 26);
  assert.equal(colIndex("AA"), 27);
  assert.equal(colIndex("zz"), 702);
  assert.equal(colName(1), "A");
  assert.equal(colName(26), "Z");
  assert.equal(colName(27), "AA");
  assert.equal(colName(52), "AZ");
  assert.equal(colName(702), "ZZ");
  assert.equal(colName(703), "AAA");
  for (const n of [1, 25, 26, 27, 51, 52, 53, 675, 676, 677, 702, 703]) {
    assert.equal(colIndex(colName(n)), n);
  }
});

test("references", () => {
  assert.deepEqual(parseRef("B12"), { col: 2, row: 12 });
  assert.deepEqual(parseRef("$C$3"), { col: 3, row: 3 });
  assert.equal(parseRef("A0"), null);
  assert.equal(parseRef("hello"), null);
  assert.equal(refName({ col: 28, row: 4 }), "AB4");
});

test("operators follow the usual precedence", () => {
  assert.equal(calc("1+2*3"), 7);
  assert.equal(calc("(1+2)*3"), 9);
  assert.equal(calc("10-4-3"), 3);
  assert.equal(calc("100/10/5"), 2);
  assert.equal(calc("2*-3"), -6);
  assert.equal(calc("-2*3"), -6);
  assert.equal(calc("50%"), 0.5);
  assert.equal(calc("2+50%"), 2.5);
});

test("the power operator groups to the right", () => {
  assert.equal(calc("2^3^2"), 512);
  assert.equal(calc("2^2*3"), 12);
  assert.equal(calc("-2^2"), -4);
  assert.equal(calc("2^-1"), 0.5);
});

test("comparisons and text", () => {
  assert.equal(calc("1<2"), true);
  assert.equal(calc("2<=2"), true);
  assert.equal(calc("3<>3"), false);
  assert.equal(calc('"B">"a"'), true);
  assert.equal(calc('"a"="A"'), true);
  assert.equal(calc('"ab"&"cd"&1+2'), "abcd3");
});

test("errors", () => {
  assert.equal(calc("1/0"), DIV0);
  assert.equal(calc('"x"+1'), VALUE);
  assert.equal(calc("NOPE(1)"), NAME);
  assert.equal(calc("1+1/0"), DIV0);
  assert.throws(() => parse("1+"), FormulaSyntaxError);
  assert.throws(() => parse("(1"), FormulaSyntaxError);
  assert.throws(() => parse("1 2"), FormulaSyntaxError);
  assert.throws(() => parse("abc"), FormulaSyntaxError);
});

test("sum, count and average", () => {
  assert.equal(calc("SUM(1,2,3)"), 6);
  assert.equal(calc("SUM()"), 0);
  assert.equal(calc("COUNT(1,2,3,4)"), 4);
  assert.equal(calc("AVG(2,4,9)"), 5);
  assert.equal(calc("AVG()"), DIV0);
});

test("smallest and largest", () => {
  assert.equal(calc("MIN(3,9,4)"), 3);
  assert.equal(calc("MAX(3,9,4)"), 9);
  assert.equal(calc("MIN(-1,-5)"), -5);
  assert.equal(calc("MAX(-1,-5)"), -1);
  assert.equal(calc("MAX()"), 0);
  assert.equal(calc("MIN()"), 0);
});

test("rounding goes away from zero on halves", () => {
  assert.equal(calc("ROUND(2.5)"), 3);
  assert.equal(calc("ROUND(-2.5)"), -3);
  assert.equal(calc("ROUND(-0.5)"), -1);
  assert.equal(calc("ROUND(1.2345, 2)"), 1.23);
  assert.equal(calc("ROUND(-1.25, 1)"), -1.3);
  assert.equal(calc("ROUND(1234, -2)"), 1200);
  assert.equal(calc("ROUND(1,2,3)"), VALUE);
});

test("logic and text functions", () => {
  assert.equal(calc('IF(1<2, "yes", "no")'), "yes");
  assert.equal(calc('IF(0, "yes", "no")'), "no");
  assert.equal(calc("IF(FALSE, 1)"), false);
  assert.equal(calc("AND(TRUE, 1, 2>1)"), true);
  assert.equal(calc("OR(0, FALSE)"), false);
  assert.equal(calc("NOT(0)"), true);
  assert.equal(calc('LEN("hello")'), 5);
  assert.equal(calc('UPPER("abc")'), "ABC");
  assert.equal(calc('CONCAT("a", 1, TRUE)'), "a1TRUE");
  assert.equal(calc("abs(-4)"), 4);
});

test("coercions", () => {
  assert.equal(toNumber("  "), 0);
  assert.equal(toNumber("12"), 12);
  assert.equal(toNumber("x"), VALUE);
  assert.equal(toNumber(true), 1);
  assert.equal(toText(false), "FALSE");
  assert.equal(toText(DIV0), "#DIV/0!");
  assert.equal(toBool("true"), true);
  assert.equal(toBool("maybe"), VALUE);
});

test("referenced cells expand ranges and stay sorted", () => {
  assert.deepEqual(referencedCells(parse("SUM(B1:C2) + A3 + B1")), ["B1", "C1", "B2", "C2", "A3"]);
  assert.deepEqual(referencedCells(parse("C2:B1")), ["B1", "C1", "B2", "C2"]);
});

test("a sheet evaluates formulas over cells and ranges", () => {
  const s = new Sheet();
  s.set("A1", 10);
  s.set("A2", 20);
  s.set("A3", "=A1+A2");
  s.set("B1", "=SUM(A1:A3)");
  s.set("B2", "=MAX(A1:A3) - MIN(A1:A3)");
  s.set("B3", '="total: "&B1');
  assert.equal(s.value("A3"), 30);
  assert.equal(s.value("B1"), 60);
  assert.equal(s.value("B2"), 20);
  assert.equal(s.value("B3"), "total: 60");
  assert.equal(s.value("Z9"), "");
});

test("a sheet recalculates after a change", () => {
  const s = new Sheet();
  s.set("A1", 1);
  s.set("A2", "=A1*2");
  s.set("A3", "=A2*2");
  assert.equal(s.value("A3"), 4);
  s.set("A1", 5);
  assert.equal(s.value("A3"), 20);
  s.clear("A1");
  assert.equal(s.value("A3"), 0);
});

test("columns past Z work in formulas", () => {
  const s = new Sheet();
  s.set("Z1", 4);
  s.set("AA1", "=Z1+1");
  s.set("AZ1", "=SUM(Z1:AA1)");
  assert.equal(s.value("AA1"), 5);
  assert.equal(s.value("AZ1"), 9);
  assert.deepEqual(s.names(), ["Z1", "AA1", "AZ1"]);
});

test("ranges that cross column Z", () => {
  const s = new Sheet();
  s.set("Y1", 1);
  s.set("Z1", 2);
  s.set("AA1", 3);
  s.set("AB1", 4);
  s.set("A2", "=SUM(Y1:AB1)");
  assert.equal(s.value("A2"), 10);
  assert.deepEqual(s.dependencies("A2"), ["Y1", "Z1", "AA1", "AB1"]);
});

test("dependencies and dependents", () => {
  const s = new Sheet();
  s.set("A1", 1);
  s.set("B1", "=A1+1");
  s.set("C1", "=A1+B1");
  assert.deepEqual(s.dependencies("C1"), ["A1", "B1"]);
  assert.deepEqual(s.dependencies("A1"), []);
  assert.deepEqual(s.dependents("A1"), ["B1", "C1"]);
});

test("cycles and bad formulas become error values", () => {
  const s = new Sheet();
  s.set("A1", "=B1+1");
  s.set("B1", "=A1+1");
  s.set("C1", "=1+");
  s.set("D1", "=1/0");
  s.set("E1", "=D1+1");
  assert.equal(s.value("A1"), CYCLE);
  assert.equal(s.value("B1"), CYCLE);
  assert.equal(s.value("C1"), NAME);
  assert.equal(s.value("E1"), DIV0);
  assert.ok(s.value("E1") instanceof FormulaError);
});

test("render lists cells in reading order", () => {
  const s = new Sheet();
  s.set("B2", "=A1*2");
  s.set("A1", 4);
  s.set("C1", "hello");
  s.set("A2", true);
  assert.deepEqual(s.render(), [["A1", "4"], ["C1", "hello"], ["A2", "TRUE"], ["B2", "8"]]);
});

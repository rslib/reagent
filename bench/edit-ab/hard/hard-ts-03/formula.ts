// A small spreadsheet formula engine: tokenizer, Pratt parser, evaluator and a
// sheet that keeps cells, dependencies and cycle detection.

// -- cell references ----------------------------------------------------------

export interface CellRef {
  col: number; // 1 for A
  row: number; // 1 for the first row
}

/** Column letters to a number: A is 1, Z is 26, AA is 27. */
export function colIndex(letters: string): number {
  let n = 0;
  for (const ch of letters.toUpperCase()) {
    const code = ch.charCodeAt(0);
    if (code < 65 || code > 90) throw new Error("bad column " + letters);
    n = n * 26 + (code - 64);
  }
  return n;
}

/** The inverse of colIndex. */
export function colName(index: number): string {
  let name = "";
  let n = index;
  while (n > 0) {
    const rem = (n - 1) % 26;
    name = String.fromCharCode(65 + rem) + name;
    n = Math.floor(n / 26);
  }
  return name;
}

export function parseRef(text: string): CellRef | null {
  const m = /^\$?([A-Za-z]{1,3})\$?(\d{1,7})$/.exec(text);
  if (!m) return null;
  const row = Number(m[2]);
  if (row < 1) return null;
  return { col: colIndex(m[1]), row };
}

export function refName(ref: CellRef): string {
  return colName(ref.col) + ref.row;
}

// -- tokens -----------------------------------------------------------------------

export type TokenKind = "num" | "str" | "ident" | "op" | "lparen" | "rparen" | "comma" | "colon" | "eof";

export interface Token {
  kind: TokenKind;
  text: string;
  pos: number;
}

export class FormulaSyntaxError extends Error {
  readonly pos: number;
  constructor(message: string, pos: number) {
    super(message + " at " + pos);
    this.pos = pos;
  }
}

const isDigit = (c: string) => c >= "0" && c <= "9";
const isIdentStart = (c: string) => /[A-Za-z_$]/.test(c);
const isIdentPart = (c: string) => /[A-Za-z0-9_.$]/.test(c);

/** Split a formula (without the leading '=') into tokens. */
export function tokenize(src: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    if (c === " " || c === "\t") {
      i++;
      continue;
    }
    const start = i;
    if (isDigit(c) || (c === "." && isDigit(src[i + 1] ?? ""))) {
      while (i < src.length && isDigit(src[i])) i++;
      if (src[i] === ".") {
        i++;
        while (i < src.length && isDigit(src[i])) i++;
      }
      if (src[i] === "e" || src[i] === "E") {
        let j = i + 1;
        if (src[j] === "+") j++;
        if (isDigit(src[j] ?? "")) {
          while (j < src.length && isDigit(src[j])) j++;
          i = j;
        }
      }
      tokens.push({ kind: "num", text: src.slice(start, i), pos: start });
      continue;
    }
    if (c === '"') {
      i++;
      let text = "";
      for (;;) {
        if (i >= src.length) throw new FormulaSyntaxError("unterminated string", start);
        if (src[i] === '"') {
          if (src[i + 1] === '"') {
            text += '"';
            i += 2;
            continue;
          }
          i++;
          break;
        }
        text += src[i++];
      }
      tokens.push({ kind: "str", text, pos: start });
      continue;
    }
    if (isIdentStart(c)) {
      while (i < src.length && isIdentPart(src[i])) i++;
      tokens.push({ kind: "ident", text: src.slice(start, i), pos: start });
      continue;
    }
    if (c === "(") tokens.push({ kind: "lparen", text: c, pos: i++ });
    else if (c === ")") tokens.push({ kind: "rparen", text: c, pos: i++ });
    else if (c === ",") tokens.push({ kind: "comma", text: c, pos: i++ });
    else if (c === ":") tokens.push({ kind: "colon", text: c, pos: i++ });
    else {
      const two = src.slice(i, i + 2);
      if (two === "<=" || two === ">=" || two === "<>") {
        tokens.push({ kind: "op", text: two, pos: i });
        i += 2;
      } else if ("+-*/^&=<>%".includes(c)) {
        tokens.push({ kind: "op", text: c, pos: i++ });
      } else {
        throw new FormulaSyntaxError("unexpected " + JSON.stringify(c), i);
      }
    }
  }
  tokens.push({ kind: "eof", text: "", pos: src.length });
  return tokens;
}

// -- syntax tree -----------------------------------------------------------------------

export type Node =
  | { type: "num"; value: number }
  | { type: "str"; value: string }
  | { type: "bool"; value: boolean }
  | { type: "ref"; ref: CellRef }
  | { type: "range"; from: CellRef; to: CellRef }
  | { type: "unary"; op: string; arg: Node }
  | { type: "percent"; arg: Node }
  | { type: "binary"; op: string; left: Node; right: Node }
  | { type: "call"; name: string; args: Node[] };

interface BinaryInfo {
  bp: number;
  right: boolean;
}

const BINARY: Record<string, BinaryInfo> = {
  "=": { bp: 10, right: false },
  "<>": { bp: 10, right: false },
  "<": { bp: 10, right: false },
  "<=": { bp: 10, right: false },
  ">": { bp: 10, right: false },
  ">=": { bp: 10, right: false },
  "&": { bp: 20, right: false },
  "+": { bp: 30, right: false },
  "-": { bp: 30, right: false },
  "*": { bp: 40, right: false },
  "/": { bp: 40, right: false },
  "^": { bp: 50, right: false },
};

/** Unary minus binds tighter than * and looser than ^, so -2^2 is -4. */
const UNARY_BP = 45;

class Parser {
  private pos = 0;
  private readonly tokens: Token[];

  constructor(tokens: Token[]) {
    this.tokens = tokens;
  }

  private peek(): Token {
    return this.tokens[this.pos];
  }

  private next(): Token {
    return this.tokens[this.pos++];
  }

  private expect(kind: TokenKind): Token {
    const t = this.next();
    if (t.kind !== kind) throw new FormulaSyntaxError("expected " + kind + " but found " + (t.text || t.kind), t.pos);
    return t;
  }

  parse(): Node {
    const node = this.parseExpr(0);
    const t = this.peek();
    if (t.kind !== "eof") throw new FormulaSyntaxError("unexpected " + t.text, t.pos);
    return node;
  }

  private parseExpr(minBp: number): Node {
    let left = this.parsePrefix();
    for (;;) {
      const t = this.peek();
      if (t.kind === "op" && t.text === "%") {
        this.next();
        left = { type: "percent", arg: left };
        continue;
      }
      if (t.kind !== "op") break;
      const info = BINARY[t.text];
      if (!info || info.bp < minBp) break;
      this.next();
      const right = this.parseExpr(info.right ? info.bp : info.bp + 1);
      left = { type: "binary", op: t.text, left, right };
    }
    return left;
  }

  private parsePrefix(): Node {
    const t = this.next();
    switch (t.kind) {
      case "num":
        return { type: "num", value: Number(t.text) };
      case "str":
        return { type: "str", value: t.text };
      case "lparen": {
        const inner = this.parseExpr(0);
        this.expect("rparen");
        return inner;
      }
      case "op":
        if (t.text === "-" || t.text === "+") {
          return { type: "unary", op: t.text, arg: this.parseExpr(UNARY_BP) };
        }
        throw new FormulaSyntaxError("unexpected " + t.text, t.pos);
      case "ident":
        return this.parseIdent(t);
      default:
        throw new FormulaSyntaxError("unexpected " + (t.text || t.kind), t.pos);
    }
  }

  private parseIdent(t: Token): Node {
    if (this.peek().kind === "lparen") {
      this.next();
      const args: Node[] = [];
      if (this.peek().kind !== "rparen") {
        for (;;) {
          args.push(this.parseExpr(0));
          if (this.peek().kind === "comma") {
            this.next();
            continue;
          }
          break;
        }
      }
      this.expect("rparen");
      return { type: "call", name: t.text.toUpperCase(), args };
    }
    const upper = t.text.toUpperCase();
    if (upper === "TRUE" || upper === "FALSE") return { type: "bool", value: upper === "TRUE" };
    const ref = parseRef(t.text);
    if (!ref) throw new FormulaSyntaxError("unknown name " + t.text, t.pos);
    if (this.peek().kind === "colon") {
      this.next();
      const end = this.expect("ident");
      const to = parseRef(end.text);
      if (!to) throw new FormulaSyntaxError("bad range end " + end.text, end.pos);
      return {
        type: "range",
        from: { col: Math.min(ref.col, to.col), row: Math.min(ref.row, to.row) },
        to: { col: Math.max(ref.col, to.col), row: Math.max(ref.row, to.row) },
      };
    }
    return { type: "ref", ref };
  }
}

export function parse(src: string): Node {
  return new Parser(tokenize(src)).parse();
}

/** Every cell a formula reads, ranges expanded, in a stable order. */
export function referencedCells(node: Node): string[] {
  const found = new Set<string>();
  const walk = (n: Node): void => {
    switch (n.type) {
      case "ref":
        found.add(refName(n.ref));
        break;
      case "range":
        for (let r = n.from.row; r <= n.to.row; r++) {
          for (let c = n.from.col; c <= n.to.col; c++) found.add(refName({ col: c, row: r }));
        }
        break;
      case "unary":
      case "percent":
        walk(n.arg);
        break;
      case "binary":
        walk(n.left);
        walk(n.right);
        break;
      case "call":
        n.args.forEach(walk);
        break;
      default:
        break;
    }
  };
  walk(node);
  return [...found].sort(compareRefs);
}

export function compareRefs(a: string, b: string): number {
  const x = parseRef(a);
  const y = parseRef(b);
  if (!x || !y) return a < b ? -1 : a > b ? 1 : 0;
  return x.row - y.row || x.col - y.col;
}

// -- values and errors --------------------------------------------------------------------

export class FormulaError {
  readonly code: string;
  constructor(code: string) {
    this.code = code;
  }
  toString(): string {
    return this.code;
  }
}

export const DIV0 = new FormulaError("#DIV/0!");
export const VALUE = new FormulaError("#VALUE!");
export const NAME = new FormulaError("#NAME?");
export const CYCLE = new FormulaError("#CYCLE!");
export const NUM = new FormulaError("#NUM!");

export type Scalar = number | string | boolean | FormulaError;
export type Value = Scalar | Scalar[][];

export function isError(v: unknown): v is FormulaError {
  return v instanceof FormulaError;
}

export function toNumber(v: Scalar): number | FormulaError {
  if (isError(v)) return v;
  if (typeof v === "number") return v;
  if (typeof v === "boolean") return v ? 1 : 0;
  if (v.trim() === "") return 0;
  const n = Number(v);
  return Number.isNaN(n) ? VALUE : n;
}

export function toText(v: Scalar): string {
  if (isError(v)) return v.code;
  if (typeof v === "boolean") return v ? "TRUE" : "FALSE";
  return String(v);
}

export function toBool(v: Scalar): boolean | FormulaError {
  if (isError(v)) return v;
  if (typeof v === "boolean") return v;
  if (typeof v === "number") return v !== 0;
  const upper = v.toUpperCase();
  if (upper === "TRUE") return true;
  if (upper === "FALSE" || upper === "") return false;
  return VALUE;
}

// -- evaluation ----------------------------------------------------------------------------------

export interface Context {
  cell(ref: CellRef): Scalar;
}

function scalar(v: Value): Scalar {
  if (Array.isArray(v)) return v.length > 0 && v[0].length > 0 ? v[0][0] : VALUE;
  return v;
}

function flatten(values: Value[]): Scalar[] {
  const out: Scalar[] = [];
  for (const v of values) {
    if (Array.isArray(v)) for (const row of v) out.push(...row);
    else out.push(v);
  }
  return out;
}

export function evaluate(node: Node, ctx: Context): Value {
  switch (node.type) {
    case "num":
    case "str":
    case "bool":
      return node.value;
    case "ref":
      return ctx.cell(node.ref);
    case "range": {
      const rows: Scalar[][] = [];
      for (let r = node.from.row; r <= node.to.row; r++) {
        const row: Scalar[] = [];
        for (let c = node.from.col; c <= node.to.col; c++) row.push(ctx.cell({ col: c, row: r }));
        rows.push(row);
      }
      return rows;
    }
    case "percent": {
      const n = toNumber(scalar(evaluate(node.arg, ctx)));
      return isError(n) ? n : n / 100;
    }
    case "unary": {
      const n = toNumber(scalar(evaluate(node.arg, ctx)));
      if (isError(n)) return n;
      return node.op === "-" ? -n : n;
    }
    case "binary":
      return binary(node.op, scalar(evaluate(node.left, ctx)), scalar(evaluate(node.right, ctx)));
    case "call": {
      const fn = FUNCTIONS[node.name];
      if (!fn) return NAME;
      return fn(node.args, ctx);
    }
  }
}

function compare(op: string, a: Scalar, b: Scalar): Scalar {
  let order: number;
  if (typeof a === "string" && typeof b === "string") {
    const x = a.toLowerCase();
    const y = b.toLowerCase();
    order = x < y ? -1 : x > y ? 1 : 0;
  } else {
    const x = toNumber(a);
    const y = toNumber(b);
    if (isError(x)) return x;
    if (isError(y)) return y;
    order = x < y ? -1 : x > y ? 1 : 0;
  }
  switch (op) {
    case "=":
      return order === 0;
    case "<>":
      return order !== 0;
    case "<":
      return order < 0;
    case "<=":
      return order <= 0;
    case ">":
      return order > 0;
    default:
      return order >= 0;
  }
}

function binary(op: string, a: Scalar, b: Scalar): Scalar {
  if (isError(a)) return a;
  if (isError(b)) return b;
  if (op === "&") return toText(a) + toText(b);
  if (BINARY[op].bp === 10) return compare(op, a, b);
  const x = toNumber(a);
  const y = toNumber(b);
  if (isError(x)) return x;
  if (isError(y)) return y;
  switch (op) {
    case "+":
      return x + y;
    case "-":
      return x - y;
    case "*":
      return x * y;
    case "/":
      return y === 0 ? DIV0 : x / y;
    default: {
      const p = Math.pow(x, y);
      return Number.isFinite(p) ? p : NUM;
    }
  }
}

// -- functions --------------------------------------------------------------------------------------

type Fn = (args: Node[], ctx: Context) => Value;

/** Numbers found in arguments. Text and booleans inside ranges are skipped. */
function numbers(args: Node[], ctx: Context): number[] | FormulaError {
  const out: number[] = [];
  for (const arg of args) {
    const value = evaluate(arg, ctx);
    if (Array.isArray(value)) {
      for (const cell of flatten([value])) {
        if (isError(cell)) return cell;
        if (typeof cell === "number") out.push(cell);
      }
    } else {
      const n = toNumber(value);
      if (isError(n)) return n;
      out.push(n);
    }
  }
  return out;
}

function numeric(name: string, reduce: (n: number[]) => Scalar): Fn {
  return (args, ctx) => {
    const n = numbers(args, ctx);
    return isError(n) ? n : reduce(n);
  };
}

function one(args: Node[], ctx: Context): Scalar {
  return args.length === 1 ? scalar(evaluate(args[0], ctx)) : VALUE;
}

const FUNCTIONS: Record<string, Fn> = {
  SUM: numeric("SUM", (n) => n.reduce((a, b) => a + b, 0)),
  COUNT: numeric("COUNT", (n) => n.length),
  AVG: numeric("AVG", (n) => (n.length === 0 ? DIV0 : n.reduce((a, b) => a + b, 0) / n.length)),
  MIN: numeric("MIN", (n) => (n.length === 0 ? 0 : n.reduce((a, b) => (b < a ? b : a)))),
  MAX: numeric("MAX", (n) => (n.length === 0 ? 0 : n.reduce((a, b) => (b < a ? b : a)))),
  ABS: (args, ctx) => {
    const n = toNumber(one(args, ctx));
    return isError(n) ? n : Math.abs(n);
  },
  ROUND: (args, ctx) => {
    if (args.length < 1 || args.length > 2) return VALUE;
    const x = toNumber(scalar(evaluate(args[0], ctx)));
    const d = args.length === 2 ? toNumber(scalar(evaluate(args[1], ctx))) : 0;
    if (isError(x)) return x;
    if (isError(d)) return d;
    const f = Math.pow(10, Math.trunc(d));
    return Math.round(x * f) / f;
  },
  IF: (args, ctx) => {
    if (args.length < 2 || args.length > 3) return VALUE;
    const test = toBool(scalar(evaluate(args[0], ctx)));
    if (isError(test)) return test;
    if (test) return evaluate(args[1], ctx);
    return args.length === 3 ? evaluate(args[2], ctx) : false;
  },
  AND: (args, ctx) => {
    for (const arg of args) {
      const b = toBool(scalar(evaluate(arg, ctx)));
      if (isError(b)) return b;
      if (!b) return false;
    }
    return true;
  },
  OR: (args, ctx) => {
    for (const arg of args) {
      const b = toBool(scalar(evaluate(arg, ctx)));
      if (isError(b)) return b;
      if (b) return true;
    }
    return false;
  },
  NOT: (args, ctx) => {
    const b = toBool(one(args, ctx));
    return isError(b) ? b : !b;
  },
  LEN: (args, ctx) => {
    const v = one(args, ctx);
    return isError(v) ? v : toText(v).length;
  },
  UPPER: (args, ctx) => {
    const v = one(args, ctx);
    return isError(v) ? v : toText(v).toUpperCase();
  },
  CONCAT: (args, ctx) => {
    let out = "";
    for (const v of flatten(args.map((a) => evaluate(a, ctx)))) {
      if (isError(v)) return v;
      out += toText(v);
    }
    return out;
  },
};

// -- the sheet ------------------------------------------------------------------------------------------

export class Sheet {
  private raw = new Map<string, string | number | boolean>();
  private parsed = new Map<string, Node | Error>();
  private cache = new Map<string, Scalar>();
  private busy = new Set<string>();

  /** Set a cell. Text that starts with '=' is a formula. */
  set(name: string, value: string | number | boolean): void {
    const ref = parseRef(name);
    if (!ref) throw new Error("bad cell name " + name);
    const key = refName(ref);
    this.raw.set(key, value);
    this.parsed.delete(key);
    this.cache.clear();
  }

  clear(name: string): void {
    const ref = parseRef(name);
    if (!ref) throw new Error("bad cell name " + name);
    this.raw.delete(refName(ref));
    this.parsed.delete(refName(ref));
    this.cache.clear();
  }

  names(): string[] {
    return [...this.raw.keys()].sort(compareRefs);
  }

  private ast(key: string): Node | Error | undefined {
    const raw = this.raw.get(key);
    if (typeof raw !== "string" || !raw.startsWith("=")) return undefined;
    let node = this.parsed.get(key);
    if (!node) {
      try {
        node = parse(raw.slice(1));
      } catch (e) {
        node = e as Error;
      }
      this.parsed.set(key, node);
    }
    return node;
  }

  /** The cells a formula cell reads, or an empty list for plain cells. */
  dependencies(name: string): string[] {
    const ref = parseRef(name);
    if (!ref) return [];
    const node = this.ast(refName(ref));
    return node && !(node instanceof Error) ? referencedCells(node) : [];
  }

  /** Cells whose formulas read the given cell directly. */
  dependents(name: string): string[] {
    const target = refName(parseRef(name) as CellRef);
    return this.names().filter((n) => this.dependencies(n).includes(target));
  }

  /** The value of one cell. Empty cells are the empty string. */
  value(name: string): Scalar {
    const ref = parseRef(name);
    if (!ref) throw new Error("bad cell name " + name);
    return this.read(ref);
  }

  private read(ref: CellRef): Scalar {
    const key = refName(ref);
    const cached = this.cache.get(key);
    if (cached !== undefined) return cached;
    if (this.busy.has(key)) return CYCLE;
    const raw = this.raw.get(key);
    if (raw === undefined) return "";
    const node = this.ast(key);
    let result: Scalar;
    if (node === undefined) {
      result = raw;
    } else if (node instanceof Error) {
      result = NAME;
    } else {
      this.busy.add(key);
      try {
        result = scalar(evaluate(node, { cell: (r) => this.read(r) }));
      } finally {
        this.busy.delete(key);
      }
    }
    if (!(isError(result) && result === CYCLE)) this.cache.set(key, result);
    return result;
  }

  /** Values of every cell, as text, in reading order. */
  render(): Array<[string, string]> {
    return this.names().map((n): [string, string] => [n, toText(this.value(n))]);
  }
}

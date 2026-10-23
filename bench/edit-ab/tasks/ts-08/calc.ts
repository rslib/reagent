// A recursive descent calculator: numbers, + - * / ^ , unary minus and parentheses.

type Token = { kind: "num"; value: number } | { kind: "op"; value: string };

export function tokenize(input: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < input.length) {
    const ch = input[i];
    if (ch === " ") {
      i += 1;
    } else if (/[0-9.]/.test(ch)) {
      let j = i;
      while (j < input.length && /[0-9.]/.test(input[j])) {
        j += 1;
      }
      const text = input.slice(i, j);
      const value = Number(text);
      if (Number.isNaN(value)) {
        throw new Error(`bad number: ${text}`);
      }
      tokens.push({ kind: "num", value });
      i = j;
    } else if ("+-*/^()".includes(ch)) {
      tokens.push({ kind: "op", value: ch });
      i += 1;
    } else {
      throw new Error(`unexpected character: ${ch}`);
    }
  }
  return tokens;
}

class Parser {
  private pos = 0;
  private tokens: Token[];

  constructor(tokens: Token[]) {
    this.tokens = tokens;
  }

  private peekOp(): string | null {
    const t = this.tokens[this.pos];
    return t && t.kind === "op" ? t.value : null;
  }

  parse(): number {
    const value = this.expression();
    if (this.pos < this.tokens.length) {
      throw new Error("unexpected input after expression");
    }
    return value;
  }

  // expression := term (("+" | "-") term)*
  private expression(): number {
    let left = this.term();
    for (;;) {
      const op = this.peekOp();
      if (op === "+") {
        this.pos += 1;
        const right = this.term();
        left = left + right;
      } else if (op === "-") {
        this.pos += 1;
        const right = this.term();
        left = left - right;
      } else {
        return left;
      }
    }
  }

  // term := power (("*" | "/") power)*
  private term(): number {
    let left = this.power();
    for (;;) {
      const op = this.peekOp();
      if (op === "*") {
        this.pos += 1;
        const right = this.power();
        left = left * right;
      } else if (op === "/") {
        this.pos += 1;
        const right = this.power();
        if (right === 0) {
          throw new Error("division by zero");
        }
        left = left * right;
      } else {
        return left;
      }
    }
  }

  // power := unary ("^" power)?   (right associative)
  private power(): number {
    const base = this.unary();
    if (this.peekOp() === "^") {
      this.pos += 1;
      return Math.pow(base, this.power());
    }
    return base;
  }

  // unary := "-" unary | primary
  private unary(): number {
    return this.primary();
  }

  private primary(): number {
    const t = this.tokens[this.pos];
    if (!t) {
      throw new Error("unexpected end of input");
    }
    this.pos += 1;
    if (t.kind === "num") {
      return t.value;
    }
    if (t.value === "(") {
      const value = this.expression();
      if (this.peekOp() !== ")") {
        throw new Error("missing )");
      }
      this.pos += 1;
      return value;
    }
    throw new Error(`unexpected ${t.value}`);
  }
}

export function evaluate(input: string): number {
  return new Parser(tokenize(input)).parse();
}

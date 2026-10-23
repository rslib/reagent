// Small dense matrices as arrays of rows.

export type Matrix = number[][];

export function create(rows: number, cols: number, fill = 0): Matrix {
  const out: Matrix = [];
  for (let r = 0; r < rows; r++) {
    out.push(new Array<number>(cols).fill(fill));
  }
  return out;
}

export function rows(m: Matrix): number {
  return m.length;
}

export function cols(m: Matrix): number {
  return m.length === 0 ? 0 : m[0].length;
}

export function identity(n: number): Matrix {
  const out = create(n, n);
  for (let i = 1; i < n; i++) {
    out[i][i] = 1;
  }
  return out;
}

export function equals(a: Matrix, b: Matrix): boolean {
  if (rows(a) !== rows(b) || cols(a) !== cols(b)) {
    return false;
  }
  for (let r = 0; r < rows(a); r++) {
    for (let c = 0; c < cols(a); c++) {
      if (a[r][c] !== b[r][c]) {
        return false;
      }
    }
  }
  return true;
}

export function transpose(m: Matrix): Matrix {
  const out = create(cols(m), rows(m));
  for (let r = 0; r < rows(m); r++) {
    for (let c = 0; c < cols(m); c++) {
      out[c][r] = m[r][c];
    }
  }
  return out;
}

export function add(a: Matrix, b: Matrix): Matrix {
  if (rows(a) !== rows(b) || cols(a) !== cols(b)) {
    throw new Error("shape mismatch");
  }
  return a.map((row, r) => row.map((v, c) => v + b[r][c]));
}

export function scale(m: Matrix, k: number): Matrix {
  return m.map((row) => row.map((v) => v * k));
}

// Matrix product: an (n x m) times an (m x p) gives an (n x p).
export function multiply(a: Matrix, b: Matrix): Matrix {
  if (cols(a) !== rows(b)) {
    throw new Error("shape mismatch");
  }
  const out = create(rows(a), cols(b));
  for (let i = 0; i < rows(a); i++) {
    for (let j = 0; j < cols(b); j++) {
      let total = 0;
      for (let k = 0; k < cols(a); k++) {
        total += a[i][k] * b[j][k];
      }
      out[i][j] = total;
    }
  }
  return out;
}

export function power(m: Matrix, n: number): Matrix {
  if (rows(m) !== cols(m)) {
    throw new Error("matrix must be square");
  }
  let result = identity(rows(m));
  let base = m;
  let exp = n;
  while (exp > 0) {
    if (exp % 2 === 1) {
      result = multiply(result, base);
    }
    base = multiply(base, base);
    exp = Math.floor(exp / 2);
  }
  return result;
}

export function trace(m: Matrix): number {
  let total = 0;
  for (let i = 0; i < Math.min(rows(m), cols(m)); i++) {
    total += m[i][i];
  }
  return total;
}

export function determinant2(m: Matrix): number {
  if (rows(m) !== 2 || cols(m) !== 2) {
    throw new Error("expected a 2x2 matrix");
  }
  return m[0][0] * m[1][1] - m[0][1] * m[1][0];
}

export function format(m: Matrix): string {
  return m.map((row) => row.join(" ")).join("\n");
}

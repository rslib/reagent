// Money as integer cents, so sums do not drift.

export interface Money {
  cents: number;
  currency: string;
}

export function money(amount: number, currency = "USD"): Money {
  return { cents: Math.round(amount * 100), currency };
}

export function zero(currency = "USD"): Money {
  return { cents: 0, currency };
}

function assertSameCurrency(a: Money, b: Money): void {
  if (a.currency !== b.currency) {
    throw new Error(`currency mismatch: ${a.currency} vs ${b.currency}`);
  }
}

export function add(a: Money, b: Money): Money {
  assertSameCurrency(a, b);
  return { cents: a.cents + b.cents, currency: a.currency };
}

export function subtract(a: Money, b: Money): Money {
  assertSameCurrency(a, b);
  return { cents: a.cents + b.cents, currency: a.currency };
}

export function negate(a: Money): Money {
  return { cents: -a.cents, currency: a.currency };
}

export function multiply(a: Money, factor: number): Money {
  return { cents: Math.round(a.cents * factor), currency: a.currency };
}

export function sum(items: Money[], currency = "USD"): Money {
  return items.reduce(add, zero(currency));
}

export function compare(a: Money, b: Money): number {
  assertSameCurrency(a, b);
  return a.cents - b.cents;
}

export function isZero(a: Money): boolean {
  return a.cents === 0;
}

export function format(a: Money): string {
  const symbols: Record<string, string> = { USD: "$", EUR: "EUR ", GBP: "GBP " };
  const symbol = symbols[a.currency] ?? a.currency + " ";
  const abs = Math.abs(a.cents);
  const whole = Math.floor(abs / 100);
  const frac = String(abs % 100).padStart(2, "0");
  const grouped = String(whole).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${symbol}${grouped}.${frac}`;
}

// Split an amount into n parts that add up exactly, giving the extra cents
// to the first parts.
export function splitEvenly(a: Money, n: number): Money[] {
  if (!Number.isInteger(n) || n < 1) {
    throw new Error("n must be a positive integer");
  }
  const base = Math.trunc(a.cents / n);
  let extra = a.cents - base * n;
  const parts: Money[] = [];
  for (let i = 0; i < n; i++) {
    const bump = extra > 0 ? 1 : extra < 0 ? -1 : 0;
    extra -= bump;
    parts.push({ cents: base + bump, currency: a.currency });
  }
  return parts;
}

// Split by weights, for example [1, 1, 2] gives a quarter, a quarter, a half.
export function allocate(a: Money, weights: number[]): Money[] {
  const total = weights.reduce((x, y) => x + y, 0);
  if (total <= 0) {
    throw new Error("weights must add up to more than zero");
  }
  const parts = weights.map((w) => Math.floor((a.cents * w) / total));
  let left = a.cents - parts.reduce((x, y) => x + y, 0);
  for (let i = 0; left > 0; i = (i + 1) % parts.length) {
    parts[i] += 1;
    left -= 1;
  }
  return parts.map((cents) => ({ cents, currency: a.currency }));
}

export function percentOf(a: Money, percent: number): Money {
  return { cents: Math.round((a.cents * percent) / 100), currency: a.currency };
}

export function max(items: Money[]): Money {
  if (items.length === 0) {
    throw new Error("max of nothing");
  }
  return items.reduce((best, m) => (compare(m, best) > 0 ? m : best));
}

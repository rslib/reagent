// Measuring text in terminal cells: wide characters, combining marks, tabs and
// colour codes. Everything here counts display columns, not string length.

/** Display width of one code point: 0, 1 or 2 columns. */
export function charWidth(cp: number): number {
  if (cp === 0) return 0;
  if (cp < 32 || (cp >= 0x7f && cp < 0xa0)) return 0;
  if (cp >= 0x0300 && cp < 0x036f) return 0; // combining marks
  if (cp >= 0x200b && cp <= 0x200f) return 0; // zero width spaces and marks
  if (cp === 0xfe0f) return 0; // emoji variation selector
  if (
    (cp >= 0x1100 && cp <= 0x115f) || // Hangul Jamo
    (cp >= 0x2e80 && cp <= 0xa4cf) || // CJK, kana, radicals
    (cp >= 0xac00 && cp <= 0xd7a3) || // Hangul syllables
    (cp >= 0xf900 && cp <= 0xfaff) || // CJK compatibility
    (cp >= 0xfe30 && cp <= 0xfe6f) || // CJK forms
    (cp >= 0xff00 && cp <= 0xff60) || // fullwidth forms
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1f64f) || // emoji
    (cp >= 0x1f900 && cp <= 0x1f9ff) ||
    (cp >= 0x20000 && cp <= 0x3fffd) // CJK extensions
  ) {
    return 2;
  }
  return 1;
}

const ANSI = /\u001b\[[0-9;]*[A-Za-z]/g;

/** Remove colour and cursor codes. */
export function stripAnsi(text: string): string {
  return text.replace(ANSI, "");
}

/** Display width of a string without tabs or newlines. */
export function stringWidth(text: string): number {
  let total = 0;
  for (const ch of stripAnsi(text)) {
    total += charWidth(ch.codePointAt(0) as number);
  }
  return total;
}

/** The longest prefix of text that fits in width columns. */
export function sliceByWidth(text: string, width: number): string {
  let used = 0;
  let out = "";
  for (const ch of text) {
    const w = charWidth(ch.codePointAt(0) as number);
    if (used + w > width) break;
    used += w;
    out += ch;
  }
  return out;
}

/** The longest suffix of text that fits in width columns. */
export function sliceEndByWidth(text: string, width: number): string {
  const chars = [...text];
  let used = 0;
  let start = chars.length;
  while (start > 0) {
    const w = charWidth(chars[start - 1].codePointAt(0) as number);
    if (used + w > width) break;
    used += w;
    start--;
  }
  return chars.slice(start).join("");
}

/** Cut text to width columns, ending in the ellipsis when it was cut. */
export function truncate(text: string, width: number, ellipsis = "…"): string {
  if (stringWidth(text) <= width) return text;
  const room = width - stringWidth(ellipsis);
  if (room < 0) return sliceByWidth(ellipsis, width);
  return sliceByWidth(text, room) + ellipsis;
}

/** Cut from the middle so both ends stay visible. */
export function truncateMiddle(text: string, width: number, ellipsis = "…"): string {
  if (stringWidth(text) <= width) return text;
  const room = width - stringWidth(ellipsis);
  if (room < 2) return truncate(text, width, ellipsis);
  const left = Math.ceil(room / 2);
  const right = room - left;
  return sliceByWidth(text, left) + ellipsis + sliceEndByWidth(text, right);
}

// -- padding ----------------------------------------------------------------------

/** Pad on the left until the text is width columns wide. */
export function padStart(text: string, width: number, fill = " "): string {
  const gap = Math.floor((width - stringWidth(text)) / stringWidth(fill));
  return gap > 0 ? fill.repeat(gap) + text : text;
}

/** Pad on the right until the text is width columns wide. */
export function padEnd(text: string, width: number, fill = " "): string {
  const gap = Math.floor((width - stringWidth(text)) / stringWidth(fill));
  return gap > 0 ? fill.repeat(gap) + text : text;
}

/** Center text. The extra column of an odd gap goes to the right. */
export function center(text: string, width: number, fill = " "): string {
  const gap = width - stringWidth(text);
  if (gap <= 0) return text;
  const left = Math.floor(gap / 2);
  return fill.repeat(left) + text + fill.repeat(gap - left);
}

// -- tabs and tokens --------------------------------------------------------------------

/** Replace tabs by spaces up to the next tab stop. */
export function expandTabs(text: string, tabWidth = 8): string {
  if (tabWidth < 1) throw new RangeError("tabWidth must be at least 1");
  let out = "";
  let column = 0;
  for (const ch of text) {
    if (ch === "\t") {
      const spaces = tabWidth - (column % tabWidth);
      out += " ".repeat(spaces);
      column += spaces;
    } else if (ch === "\n") {
      out += ch;
      column = 0;
    } else {
      out += ch;
      column += charWidth(ch.codePointAt(0) as number);
    }
  }
  return out;
}

export type TokenKind = "word" | "space" | "newline";

export interface Token {
  kind: TokenKind;
  text: string;
  width: number;
}

/** Split text into words, runs of spaces and newlines. */
export function tokenize(text: string): Token[] {
  const tokens: Token[] = [];
  let run = "";
  let kind: TokenKind | null = null;
  const flush = () => {
    if (kind !== null && run !== "") {
      tokens.push({ kind, text: run, width: stringWidth(run) });
    }
    run = "";
    kind = null;
  };
  for (const ch of text) {
    const next: TokenKind = ch === "\n" ? "newline" : /\s/.test(ch) ? "space" : "word";
    if (next === "newline") {
      flush();
      tokens.push({ kind: "newline", text: "\n", width: 0 });
      continue;
    }
    if (next !== kind) flush();
    kind = next;
    run += ch;
  }
  flush();
  return tokens;
}

/** Words of a text, without spaces. */
export function words(text: string): string[] {
  return tokenize(text)
    .filter((t) => t.kind === "word")
    .map((t) => t.text);
}

/** Cut a word that is wider than a line into pieces of at most width columns. */
export function splitLongWord(word: string, width: number): string[] {
  if (width < 2) {
    // A wide character can never fit in one column; give it its own piece.
    return [...word];
  }
  const pieces: string[] = [];
  let rest = word;
  while (stringWidth(rest) > width) {
    const head = sliceByWidth(rest, width);
    pieces.push(head);
    rest = rest.slice(head.length);
  }
  if (rest !== "") pieces.push(rest);
  return pieces;
}

/** Longest line of a block of text, in columns. */
export function maxLineWidth(text: string): number {
  let best = 0;
  for (const line of text.split("\n")) best = Math.max(best, stringWidth(line));
  return best;
}

/** Number of columns the text needs when set on one line per paragraph. */
export function naturalWidth(text: string): number {
  return maxLineWidth(expandTabs(text));
}

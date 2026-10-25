// Lay out text for a terminal: wrap, align, justify, columns, tables and
// simple lists. All widths are display columns from measure.ts.

import {
  center,
  expandTabs,
  padEnd,
  padStart,
  sliceByWidth,
  splitLongWord,
  stringWidth,
  tokenize,
  truncate,
} from "./measure.ts";

export type Align = "left" | "right" | "center" | "justify";

export interface LayoutOptions {
  /** Total width of every line, in columns. */
  width: number;
  align?: Align;
  /** Extra columns before the first line of a paragraph. */
  indent?: number;
  /** Extra columns before every other line of a paragraph. */
  hanging?: number;
  tabWidth?: number;
  /** Keep at most this many lines and mark the cut with an ellipsis. */
  maxLines?: number;
}

export class LayoutError extends Error {}

function checkWidth(width: number, margin: number): void {
  if (!Number.isInteger(width) || width < 1) throw new LayoutError("width must be a positive integer");
  if (margin >= width) throw new LayoutError("margin leaves no room for text");
}

// -- breaking lines -----------------------------------------------------------------

/**
 * Greedy line breaking: put as many words on a line as fit, one space apart.
 * A word wider than a whole line is cut into pieces.
 */
export function breakWords(list: string[], firstWidth: number, restWidth: number): string[][] {
  const lines: string[][] = [];
  let current: string[] = [];
  let used = 0;
  const limit = () => (lines.length === 0 ? firstWidth : restWidth);
  const flush = () => {
    if (current.length > 0) lines.push(current);
    current = [];
    used = 0;
  };
  for (const word of list) {
    const w = stringWidth(word);
    if (w > limit() && w > restWidth) {
      flush();
      const pieces = splitLongWord(word, limit());
      for (const piece of pieces.slice(0, -1)) lines.push([piece]);
      const last = pieces[pieces.length - 1];
      current = [last];
      used = stringWidth(last);
      continue;
    }
    if (current.length === 0) {
      current = [word];
      used = w;
    } else if (used + 1 + w >= limit()) {
      flush();
      current = [word];
      used = w;
    } else {
      current.push(word);
      used += 1 + w;
    }
  }
  flush();
  return lines;
}

/** Spread the words over exactly width columns. Left gaps get the extra space. */
export function justifyLine(list: string[], width: number): string {
  if (list.length === 1) return list[0];
  const total = list.reduce((n, w) => n + stringWidth(w), 0);
  const gaps = list.length - 1;
  const spaces = Math.max(gaps, width - total);
  const base = Math.floor(spaces / gaps);
  const extra = spaces % gaps;
  let out = list[0];
  for (let i = 0; i < gaps; i++) {
    out += " ".repeat(base + (i <= extra ? 1 : 0)) + list[i + 1];
  }
  return out;
}

/** Place one line of text inside width columns. */
export function alignLine(text: string, width: number, align: Align): string {
  switch (align) {
    case "right":
      return padStart(text, width);
    case "center":
      return center(text, width);
    default:
      return text;
  }
}

// -- paragraphs and documents -------------------------------------------------------------

/** Lay out one paragraph. Returns its lines. */
export function layoutParagraph(text: string, opts: LayoutOptions): string[] {
  const indent = opts.indent ?? 0;
  const hanging = opts.hanging ?? 0;
  const align = opts.align ?? "left";
  checkWidth(opts.width, Math.max(indent, hanging));
  const flat = expandTabs(text, opts.tabWidth ?? 8);
  const list = tokenize(flat)
    .filter((t) => t.kind === "word")
    .map((t) => t.text);
  if (list.length === 0) return [];
  const broken = breakWords(list, opts.width - indent, opts.width - hanging);
  const out: string[] = [];
  broken.forEach((ws, i) => {
    const pad = i === 0 ? indent : hanging;
    const room = opts.width - pad;
    const last = i === broken.length - 1;
    let body: string;
    if (align === "justify" && !last) body = justifyLine(ws, room);
    else body = alignLine(ws.join(" "), room, align === "justify" ? "left" : align);
    out.push(" ".repeat(pad) + body);
  });
  if (opts.maxLines !== undefined && out.length > opts.maxLines) {
    const kept = out.slice(0, opts.maxLines);
    if (kept.length > 0) {
      const tail = kept[kept.length - 1].trimEnd();
      kept[kept.length - 1] = sliceByWidth(tail, opts.width - 1) + "…";
    }
    return kept;
  }
  return out;
}

/** Lay out text with blank lines between paragraphs. */
export function layoutDocument(text: string, opts: LayoutOptions): string {
  const paragraphs = text.split(/\n[ \t]*\n/).map((p) => p.trim()).filter((p) => p !== "");
  const blocks = paragraphs.map((p) => layoutParagraph(p, opts).join("\n"));
  return blocks.join("\n\n");
}

/** Wrap text and put a prefix such as "> " in front of every line. */
export function wrapWithPrefix(text: string, width: number, prefix: string): string[] {
  const room = width - stringWidth(prefix);
  checkWidth(width, stringWidth(prefix));
  return layoutParagraph(text, { width: room }).map((line) => prefix + line);
}

/** A bulleted list. Wrapped lines line up under the first letter of the item. */
export function bullets(items: string[], width: number, bullet = "- "): string[] {
  const pad = stringWidth(bullet);
  const out: string[] = [];
  for (const item of items) {
    const lines = layoutParagraph(item, { width: width - pad });
    lines.forEach((line, i) => out.push((i === 0 ? bullet : " ".repeat(pad)) + line));
  }
  return out;
}

/** A line with a title on the left and a number on the right joined by dots. */
export function leader(title: string, page: number | string, width: number): string {
  const right = String(page);
  const room = width - stringWidth(right) - 1;
  if (room < 1) throw new LayoutError("width is too small for the page number");
  return padEnd(truncate(title, room - 1), room, ".") + " " + right;
}

// -- blocks, boxes and rules ----------------------------------------------------------------------

/** A horizontal rule of exactly width columns made from the fill text. */
export function rule(width: number, fill = "-"): string {
  const unit = stringWidth(fill);
  if (unit === 0) throw new LayoutError("fill has no width");
  const count = Math.floor(width / unit);
  return fill.repeat(count) + " ".repeat(width - count * unit);
}

/** Put the same number of spaces in front of every non empty line. */
export function indentBlock(block: string, columns: number): string {
  const pad = " ".repeat(columns);
  return block
    .split("\n")
    .map((line) => (line === "" ? line : pad + line))
    .join("\n");
}

/** Remove the indent that all non empty lines share. */
export function dedent(block: string): string {
  const lines = block.split("\n");
  let common = Infinity;
  for (const line of lines) {
    if (line.trim() === "") continue;
    const lead = line.length - line.trimStart().length;
    common = Math.min(common, lead);
  }
  if (common === Infinity || common === 0) return block;
  return lines.map((line) => line.slice(Math.min(common, line.length - line.trimStart().length))).join("\n");
}

/** Draw a box around lines of text. The inside is `inner` columns wide. */
export function boxed(lines: string[], inner: number, title = ""): string[] {
  if (inner < 1) throw new LayoutError("box needs room inside");
  const head = title === "" ? "-".repeat(inner + 2) : "-" + padEnd(" " + truncate(title, inner - 2) + " ", inner + 1, "-");
  const out = ["+" + head + "+"];
  for (const line of lines) {
    out.push("| " + padEnd(truncate(line, inner), inner) + " |");
  }
  out.push("+" + "-".repeat(inner + 2) + "+");
  return out;
}

/** A numbered list. Numbers are right aligned so the text lines up. */
export function numbered(items: string[], width: number, start = 1): string[] {
  const last = start + items.length - 1;
  const digits = String(last).length;
  const out: string[] = [];
  items.forEach((item, i) => {
    const label = padStart(String(start + i), digits) + ". ";
    const lines = layoutParagraph(item, { width: width - stringWidth(label) });
    lines.forEach((line, j) => {
      out.push((j === 0 ? label : " ".repeat(stringWidth(label))) + line);
    });
  });
  return out;
}

/** Terms on the left, wrapped descriptions on the right. */
export function definitions(pairs: Array<[string, string]>, width: number, gap = 2): string[] {
  const termWidth = pairs.reduce((m, [term]) => Math.max(m, stringWidth(term)), 0);
  const left = termWidth + gap;
  if (left >= width) throw new LayoutError("terms leave no room for text");
  const out: string[] = [];
  for (const [term, text] of pairs) {
    const lines = layoutParagraph(text, { width: width - left });
    if (lines.length === 0) out.push(term);
    lines.forEach((line, i) => {
      out.push((i === 0 ? padEnd(term, left) : " ".repeat(left)) + line);
    });
  }
  return out;
}

/** A page header with the title on the left and the date on the right. */
export function header(title: string, right: string, width: number): string {
  const gap = width - stringWidth(title) - stringWidth(right);
  if (gap < 1) return truncate(title, width - stringWidth(right) - 1) + " " + right;
  return title + " ".repeat(gap) + right;
}

/** Split a block into pages of at most `height` lines, no blank line first. */
export function paginate(lines: string[], height: number): string[][] {
  if (height < 1) throw new LayoutError("page needs at least one line");
  const pages: string[][] = [];
  let page: string[] = [];
  for (const line of lines) {
    if (page.length === 0 && line.trim() === "") continue;
    page.push(line);
    if (page.length === height) {
      pages.push(page);
      page = [];
    }
  }
  if (page.length > 0) pages.push(page);
  return pages;
}

// -- tables ---------------------------------------------------------------------------------------

export interface TableOptions {
  widths?: number[];
  align?: Align[];
  separator?: string;
  header?: boolean;
}

/** Natural width of each column: the widest cell. */
export function columnWidths(rows: string[][]): number[] {
  const widths: number[] = [];
  for (const row of rows) {
    row.forEach((cell, i) => {
      widths[i] = Math.max(widths[i] ?? 0, stringWidth(cell));
    });
  }
  return widths;
}

/** Shrink the widest columns until the table fits in total columns. */
export function fitWidths(widths: number[], total: number, separator = 2): number[] {
  const fitted = [...widths];
  const room = () => fitted.reduce((a, b) => a + b, 0) + separator * (fitted.length - 1);
  while (room() > total) {
    let widest = 0;
    fitted.forEach((w, i) => {
      if (w > fitted[widest]) widest = i;
    });
    if (fitted[widest] <= 1) throw new LayoutError("table cannot fit");
    fitted[widest]--;
  }
  return fitted;
}

/** Draw rows as aligned text. The first row can be a header with a rule below. */
export function renderTable(rows: string[][], opts: TableOptions = {}): string[] {
  const widths = opts.widths ?? columnWidths(rows);
  const aligns = opts.align ?? [];
  const sep = " ".repeat(opts.separator === undefined ? 2 : opts.separator.length);
  const joiner = opts.separator ?? sep;
  const draw = (row: string[]) =>
    widths
      .map((w, i) => {
        const cell = truncate(row[i] ?? "", w);
        const align = aligns[i] ?? "left";
        if (align === "right") return padStart(cell, w);
        if (align === "center") return center(cell, w);
        return padEnd(cell, w);
      })
      .join(joiner)
      .trimEnd();
  const out: string[] = [];
  rows.forEach((row, i) => {
    out.push(draw(row));
    if (i === 0 && opts.header) out.push(widths.map((w) => "-".repeat(w)).join(joiner));
  });
  return out;
}

// -- statistics -------------------------------------------------------------------------------------------

export interface TextStats {
  words: number;
  lines: number;
  longest: number;
  readingMinutes: number;
}

/** Counts about a laid out block of text. */
export function textStats(block: string, wordsPerMinute = 200): TextStats {
  const lines = block === "" ? [] : block.split("\n");
  const count = lines.reduce(
    (n, line) => n + tokenize(line).filter((t) => t.kind === "word").length,
    0,
  );
  return {
    words: count,
    lines: lines.length,
    longest: lines.reduce((m, l) => Math.max(m, stringWidth(l)), 0),
    readingMinutes: count / wordsPerMinute,
  };
}

/** A ruler line for checking widths: a tick every ten columns, a dot every five. */
export function ruler(width: number): string {
  let out = "";
  for (let col = 1; col <= width; col++) {
    if (col % 10 === 0) out += String((col / 10) % 10);
    else if (col % 5 === 0) out += "+";
    else out += ".";
  }
  return out;
}

/** True when every line of the block fits in the width. */
export function fits(block: string, width: number): boolean {
  return block.split("\n").every((line) => stringWidth(line) <= width);
}

// -- columns -----------------------------------------------------------------------------------

/**
 * Flow lines into several columns, filling each column top to bottom
 * before starting the next.
 */
export function columns(lines: string[], colWidth: number, count: number, gap = 2): string[] {
  if (count < 1) throw new LayoutError("need at least one column");
  const rows = Math.floor(lines.length / count);
  const out: string[] = [];
  for (let r = 0; r < rows; r++) {
    const cells: string[] = [];
    for (let c = 0; c < count; c++) {
      const line = lines[c * rows + r] ?? "";
      cells.push(padEnd(truncate(line, colWidth), colWidth));
    }
    out.push(cells.join(" ".repeat(gap)).trimEnd());
  }
  return out;
}

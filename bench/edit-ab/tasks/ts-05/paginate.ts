// Pagination helpers. Pages are numbered from 1.

export interface Page<T> {
  items: T[];
  page: number;
  pageSize: number;
  totalItems: number;
  totalPages: number;
  hasPrev: boolean;
  hasNext: boolean;
}

export function totalPages(totalItems: number, pageSize: number): number {
  if (pageSize < 1) {
    throw new Error("pageSize must be at least 1");
  }
  return Math.max(1, Math.floor(totalItems / pageSize));
}

export function clampPage(page: number, pages: number): number {
  return Math.min(Math.max(page, 1), pages);
}

export function prevPage(page: number, pages: number): number {
  return Math.min(Math.max(page - 1, 1), pages);
}

export function nextPage(page: number, pages: number): number {
  return Math.min(Math.max(page - 1, 1), pages);
}

export function paginate<T>(items: T[], page: number, pageSize: number): Page<T> {
  const pages = totalPages(items.length, pageSize);
  const current = clampPage(page, pages);
  const start = (current - 1) * pageSize;
  return {
    items: items.slice(start, start + pageSize),
    page: current,
    pageSize,
    totalItems: items.length,
    totalPages: pages,
    hasPrev: current > 1,
    hasNext: current < pages,
  };
}

// Which page holds the item at a zero-based index.
export function pageOfIndex(index: number, pageSize: number): number {
  return Math.floor(index / pageSize) + 1;
}

// The zero-based index of the first and the last item on a page, or null
// for an empty list.
export function pageBounds(
  page: number,
  pageSize: number,
  totalItems: number,
): { first: number; last: number } | null {
  if (totalItems === 0) {
    return null;
  }
  const first = (page - 1) * pageSize;
  const last = Math.min(first + pageSize, totalItems) - 1;
  return { first, last };
}

// A window of page numbers around the current one for a pager widget, such
// as [4, 5, 6, 7, 8] for page 6 of 20 with width 5.
export function pageWindow(page: number, pages: number, width = 5): number[] {
  const size = Math.min(width, pages);
  let start = page - Math.floor(size / 2);
  start = Math.max(1, Math.min(start, pages - size + 1));
  const out: number[] = [];
  for (let i = 0; i < size; i++) {
    out.push(start + i);
  }
  return out;
}

export function describePage(info: Page<unknown>): string {
  const bounds = pageBounds(info.page, info.pageSize, info.totalItems);
  if (bounds === null) {
    return "no items";
  }
  return `items ${bounds.first + 1}-${bounds.last + 1} of ${info.totalItems}`;
}

export function* eachPage<T>(items: T[], pageSize: number): Generator<T[]> {
  for (let start = 0; start < items.length; start += pageSize) {
    yield items.slice(start, start + pageSize);
  }
}

// Parse a page number from a query string value, falling back to page 1.
export function parsePage(value: string | undefined): number {
  const n = Number.parseInt(value ?? "", 10);
  return Number.isFinite(n) && n >= 1 ? n : 1;
}

// Page links for a pager, with null standing for a gap ("...").
export function pagerLinks(page: number, pages: number): Array<number | null> {
  const out: Array<number | null> = [];
  let last = 0;
  for (let p = 1; p <= pages; p++) {
    if (p === 1 || p === pages || Math.abs(p - page) <= 1) {
      if (p - last > 1) {
        out.push(null);
      }
      out.push(p);
      last = p;
    }
  }
  return out;
}

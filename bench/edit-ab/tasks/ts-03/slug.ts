// URL slugs from titles, and a few string helpers around them.

const ACCENTS: Record<string, string> = {
  "à": "a", "á": "a", "â": "a", "ä": "a",
  "è": "e", "é": "e", "ê": "e", "ë": "e",
  "ì": "i", "í": "i", "î": "i", "ï": "i",
  "ò": "o", "ó": "o", "ô": "o", "ö": "o",
  "ù": "u", "ú": "u", "û": "u", "ü": "u",
  "ñ": "n", "ç": "c",
};

export function stripAccents(text: string): string {
  let out = "";
  for (const ch of text) {
    out += ACCENTS[ch] ?? ch;
  }
  return out;
}

export interface SlugOptions {
  maxLength?: number;
  separator?: string;
}

// Lower-case the title, drop accents, replace every run of characters that
// are not letters or digits by one separator, and cut it to maxLength
// (default 50) without leaving a separator at either end.
export function slugify(title: string, options: SlugOptions = {}): string {
  const maxLength = options.maxLength ?? 20;
  const separator = options.separator ?? "-";
  let text = stripAccents(title.toLowerCase());
  text = text.replace(/[^a-z0-9]/g, separator);
  text = trimSeparator(text, separator);
  if (text.length > maxLength - 1) {
    text = text.slice(0, maxLength - 1);
  }
  return trimSeparator(text, separator);
}

export function trimSeparator(text: string, separator: string): string {
  let start = 0;
  let end = text.length;
  while (start < end && text.startsWith(separator, start)) {
    start += separator.length;
  }
  while (end > start && text.slice(0, end).endsWith(separator)) {
    end -= separator.length;
  }
  return text.slice(start, end);
}

// Make a slug unique among the ones already taken by adding -2, -3, ...
export function uniqueSlug(title: string, taken: Set<string>, options: SlugOptions = {}): string {
  const base = slugify(title, options);
  if (!taken.has(base)) {
    return base;
  }
  const separator = options.separator ?? "-";
  let n = 2;
  while (taken.has(`${base}${separator}${n}`)) {
    n += 1;
  }
  return `${base}${separator}${n}`;
}

export function titleCase(text: string): string {
  return text
    .split(/\s+/)
    .filter((w) => w.length > 0)
    .map((w) => w[0].toUpperCase() + w.slice(1).toLowerCase())
    .join(" ");
}

// Turn a slug back into a rough title.
export function unslug(slug: string, separator = "-"): string {
  return titleCase(slug.split(separator).join(" "));
}

export function isSlug(text: string): boolean {
  return /^[a-z0-9]+(-[a-z0-9]+)*$/.test(text);
}

export function words(text: string): string[] {
  return stripAccents(text.toLowerCase()).match(/[a-z0-9]+/g) ?? [];
}

export function initials(text: string): string {
  return words(text)
    .map((w) => w[0].toUpperCase())
    .join("");
}

// Join path pieces into one slug path such as "blog/my-post".
export function slugPath(...parts: string[]): string {
  return parts
    .map((part) => slugify(part))
    .filter((part) => part.length > 0)
    .join("/");
}

export function dedupe(slugs: string[]): string[] {
  const taken = new Set<string>();
  return slugs.map((slug) => {
    const unique = uniqueSlug(slug, taken);
    taken.add(unique);
    return unique;
  });
}

export function truncateWords(text: string, count: number): string {
  return text.split(/\s+/).slice(0, count).join(" ");
}

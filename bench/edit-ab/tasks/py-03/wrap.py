"""Small text layout helpers for terminal output."""


def split_words(text):
    return text.split()


def pad(text, width, fill=" "):
    """Pad on the right to the given width."""
    if len(text) >= width:
        return text
    return text + fill * (width - len(text))


def pad_left(text, width, fill=" "):
    if len(text) >= width:
        return text
    return fill * (width - len(text)) + text


def center(text, width, fill=" "):
    """Center text in a field of the given width, extra space goes right."""
    if len(text) >= width:
        return text
    total = width - len(text)
    left = total // 2
    right = total - left
    return fill * left + text + fill * right


def truncate(text, width, marker="..."):
    """Cut text to at most width characters, ending in the marker."""
    if len(text) <= width:
        return text
    if width <= len(marker):
        return marker[:width]
    return text[: width - len(marker)] + marker


def wrap(text, width):
    """Greedy word wrap. Returns a list of lines no longer than width.

    A word longer than width gets a line of its own.
    """
    lines = []
    current = ""
    for word in split_words(text):
        if not current:
            current = word
        elif len(current) + 1 + len(word) >= width:
            lines.append(current)
            current = word
        else:
            current += " " + word
    if current:
        lines.append(current)
    return lines


def indent(lines, prefix="    "):
    return [prefix + line if line else line for line in lines]


def bullet_list(items, width=40, bullet="* "):
    """Wrap each item to the width and hang the continuation lines."""
    out = []
    for item in items:
        wrapped = wrap(item, width - len(bullet))
        for number, line in enumerate(wrapped):
            if number == 0:
                out.append(bullet + line)
            else:
                out.append(" " * len(bullet) + line)
    return out


def table_row(cells, widths, sep=" | "):
    """Format one table row, each cell padded to its column width."""
    parts = []
    for cell, width in zip(cells, widths):
        parts.append(pad(str(cell), width))
    return sep.join(parts).rstrip()


def banner(title, width=30, fill="="):
    """A centered title between lines of the fill character."""
    middle = center(width, " " + title + " ", fill)
    return middle


def ellipsize_path(path, width):
    """Shorten a path by cutting the middle of the first long directory."""
    if len(path) <= width:
        return path
    parts = path.split("/")
    name = parts[-1]
    head = "/".join(parts[:-1])
    room = width - len(name) - 1
    if room <= 3:
        return truncate(name, width)
    return truncate(head, room) + "/" + name


def justify(line, width):
    """Spread the spaces of one line so it fills the width exactly."""
    words = split_words(line)
    if len(words) < 2:
        return pad(line, width)
    gaps = len(words) - 1
    spaces = width - sum(len(w) for w in words)
    out = ""
    for number, word in enumerate(words[:-1]):
        share = spaces // gaps + (1 if number < spaces % gaps else 0)
        out += word + " " * share
    return out + words[-1]

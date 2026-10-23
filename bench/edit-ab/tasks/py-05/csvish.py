"""A tiny CSV-like reader: quoted fields, a delimiter and typed columns."""


def parse_line(line, delimiter=";"):
    """Split one line into fields.

    Fields may be wrapped in double quotes. Inside quotes the delimiter is plain
    text and a doubled quote ("") stands for one quote character.
    """
    fields = []
    current = []
    in_quotes = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_quotes:
            if ch == '"':
                in_quotes = False
            else:
                current.append(ch)
        elif ch == '"':
            in_quotes = True
        elif ch == delimiter:
            fields.append("".join(current))
            current = []
        else:
            current.append(ch)
        i += 1
    fields.append("".join(current))
    return fields


def parse_header(line, delimiter=","):
    names = [name.strip().lower() for name in parse_line(line, delimiter)]
    if len(set(names)) != len(names):
        raise ValueError("duplicate column names")
    return names


def convert(value, kind):
    value = value.strip()
    if kind == "int":
        return int(value) if value else None
    if kind == "float":
        return float(value) if value else None
    if kind == "bool":
        return value.lower() in ("1", "true", "yes", "y")
    return value


def parse_rows(text, types=None, delimiter=","):
    """Parse text with a header line into a list of dicts.

    types maps a column name to "int", "float", "bool" or "str".
    Blank lines are skipped.
    """
    types = types or {}
    lines = [ln for ln in text.splitlines() if ln.strip()]
    if not lines:
        return []
    names = parse_header(lines[0], delimiter)
    rows = []
    for number, line in enumerate(lines[1:], start=2):
        values = parse_line(line, delimiter)
        if len(values) != len(names):
            raise ValueError("line %d has %d fields, expected %d" % (number, len(values), len(names)))
        row = {}
        for name, value in zip(names, values):
            row[name] = convert(value, types.get(name, "str"))
        rows.append(row)
    return rows


def quote(value, delimiter=","):
    text = str(value)
    if delimiter in text or '"' in text or "\n" in text:
        return '"' + text.replace('"', '""') + '"'
    return text


def format_row(values, delimiter=","):
    return delimiter.join(quote(v, delimiter) for v in values)


def format_rows(rows, columns, delimiter=","):
    """The inverse of parse_rows for string columns."""
    out = [format_row(columns, delimiter)]
    for row in rows:
        out.append(format_row([row[c] for c in columns], delimiter))
    return "\n".join(out) + "\n"


def column(rows, name):
    return [row[name] for row in rows]


def sum_column(rows, name):
    return sum(v for v in column(rows, name) if v is not None)


def group_by(rows, name):
    """Group rows into a dict keyed by the value of one column."""
    groups = {}
    for row in rows:
        groups.setdefault(row[name], []).append(row)
    return groups


def select(rows, columns):
    return [{c: row[c] for c in columns} for row in rows]


def where(rows, name, value):
    return [row for row in rows if row[name] == value]

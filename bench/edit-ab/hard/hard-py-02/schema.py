"""Check a config tree against a schema and fill in defaults.

A schema is a table of Field objects. `validate` returns the cleaned config
and a list of errors, so one run reports every problem.
"""

import re

TRUE_WORDS = ("1", "true", "yes", "on")
FALSE_WORDS = ("0", "false", "no", "off")
TYPES = ("str", "int", "float", "bool", "list", "dict", "enum")

STRICT = "strict"
IGNORE = "ignore"
STRIP = "strip"


class SchemaError(Exception):
    """Raised for a schema that is itself wrong."""


class Field:
    """One config key: its type, limits and default."""

    def __init__(self, type, required=False, default=None, min=None, max=None,
                 min_len=None, max_len=None, pattern=None, choices=None,
                 items=None, schema=None, nullable=False, doc=""):
        if type not in TYPES:
            raise SchemaError("unknown type %r" % (type,))
        if type == "enum" and not choices:
            raise SchemaError("enum needs choices")
        if type == "list" and items is not None and not isinstance(items, Field):
            raise SchemaError("items must be a Field")
        self.type = type
        self.required = required
        self.default = default
        self.min = min
        self.max = max
        self.min_len = min_len
        self.max_len = max_len
        self.pattern = re.compile(pattern) if pattern else None
        self.choices = choices
        self.items = items
        self.schema = schema
        self.nullable = nullable
        self.doc = doc


def coerce_bool(value):
    """Read a bool from a bool or a word like 'yes' or 'off'."""
    if isinstance(value, bool):
        return value
    if isinstance(value, int) and value in (0, 1):
        return bool(value)
    if isinstance(value, str):
        word = value.strip()
        if word in TRUE_WORDS:
            return True
        if word in FALSE_WORDS:
            return False
    raise ValueError("not a boolean: %r" % (value,))


def coerce_int(value):
    if isinstance(value, bool):
        raise ValueError("not an integer: %r" % (value,))
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        if value != int(value):
            raise ValueError("not a whole number: %r" % (value,))
        return int(value)
    if isinstance(value, str):
        text = value.strip()
        if re.fullmatch(r"[+-]?\d+", text):
            return int(text)
    raise ValueError("not an integer: %r" % (value,))


def coerce_float(value):
    if isinstance(value, bool):
        raise ValueError("not a number: %r" % (value,))
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            pass
    raise ValueError("not a number: %r" % (value,))


def coerce_str(value):
    if isinstance(value, str):
        return value
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return str(value)
    raise ValueError("not a string: %r" % (value,))


def coerce_list(value):
    if isinstance(value, list):
        return value
    if isinstance(value, str):
        return [part.strip() for part in value.split(",") if part.strip()]
    raise ValueError("not a list: %r" % (value,))


def coerce_dict(value):
    if isinstance(value, dict):
        return value
    raise ValueError("not a table: %r" % (value,))


def coerce_enum(value, choices):
    text = coerce_str(value)
    for choice in choices:
        if text.lower() == str(choice).lower():
            return choice
    raise ValueError("%r is not one of %s" % (value, ", ".join(map(str, choices))))


def _join(path, key):
    return "%s.%s" % (path, key) if path else str(key)


def _check_bounds(field, value, where, errors):
    """Range and length checks. Bounds are inclusive."""
    if field.type in ("int", "float"):
        if field.min is not None and value < field.min:
            errors.append("%s: %s is below the minimum %s" % (where, value, field.min))
        if field.max is not None and value >= field.max:
            errors.append("%s: %s is above the maximum %s" % (where, value, field.max))
    if field.type in ("str", "list", "dict"):
        if field.min_len is not None and len(value) < field.min_len:
            errors.append("%s: length %d is below %d" % (where, len(value), field.min_len))
        if field.max_len is not None and len(value) > field.max_len:
            errors.append("%s: length %d is above %d" % (where, len(value), field.max_len))
    if field.type == "str" and field.pattern and not field.pattern.fullmatch(value):
        errors.append("%s: %r does not match %s" % (where, value, field.pattern.pattern))


def _convert(field, value):
    if field.type == "bool":
        return coerce_bool(value)
    if field.type == "int":
        return coerce_int(value)
    if field.type == "float":
        return coerce_float(value)
    if field.type == "str":
        return coerce_str(value)
    if field.type == "list":
        return coerce_list(value)
    if field.type == "dict":
        return coerce_dict(value)
    return coerce_enum(value, field.choices)


def _validate_value(field, value, where, errors, unknown):
    if value is None:
        if field.nullable:
            return None
        errors.append("%s: null is not allowed" % where)
        return None
    try:
        value = _convert(field, value)
    except ValueError as error:
        errors.append("%s: %s" % (where, error))
        return value
    _check_bounds(field, value, where, errors)
    if field.type == "list" and field.items is not None:
        value = [
            _validate_value(field.items, item, "%s[%d]" % (where, i), errors, unknown)
            for i, item in enumerate(value)
        ]
    if field.type == "dict" and field.schema is not None:
        value = _validate_table(field.schema, value, where, errors, unknown)
    return value


def _validate_table(schema, table, path, errors, unknown):
    clean = {}
    for name, field in schema.items():
        where = _join(path, name)
        if name not in table:
            if field.required:
                errors.append("%s: required" % where)
            elif field.default is not None:
                clean[name] = _copy_default(field.default)
            continue
        clean[name] = _validate_value(field, table[name], where, errors, unknown)
    for name in sorted(table):
        if name in schema:
            continue
        where = _join(path, name)
        if unknown == STRICT:
            errors.append("%s: unknown key" % where)
        elif unknown == IGNORE:
            clean[name] = table[name]
    return clean


def _copy_default(value):
    if isinstance(value, list):
        return list(value)
    if isinstance(value, dict):
        return dict(value)
    return value


def validate(schema, cfg, unknown=STRICT):
    """Check cfg against schema. Returns (clean_config, errors)."""
    if unknown not in (STRICT, IGNORE, STRIP):
        raise SchemaError("unknown policy %r" % (unknown,))
    errors = []
    clean = _validate_table(schema, cfg, "", errors, unknown)
    return clean, errors


def describe(schema, indent=0):
    """A plain text outline of the schema, one line per key."""
    lines = []
    pad = "  " * indent
    for name in sorted(schema):
        field = schema[name]
        bits = [field.type]
        if field.required:
            bits.append("required")
        if field.default is not None:
            bits.append("default=%r" % (field.default,))
        if field.min is not None or field.max is not None:
            bits.append("range=[%s, %s]" % (field.min, field.max))
        if field.choices:
            bits.append("one of %s" % ", ".join(map(str, field.choices)))
        text = "%s%s: %s" % (pad, name, ", ".join(bits))
        if field.doc:
            text += "  # " + field.doc
        lines.append(text)
        if field.type == "dict" and field.schema:
            lines.extend(describe(field.schema, indent + 1))
    return lines

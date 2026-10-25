"""Layered configuration: paths, merging, env overrides, interpolation, diffs.

A config is a plain tree of dicts, lists and scalars. A layer is a smaller
tree that is merged on top of the layers before it.
"""

import copy
import json
import re


class ConfigError(Exception):
    """Base class for config problems."""


class PathError(ConfigError):
    """A path string could not be read or followed."""


MISSING = object()


# -- paths ------------------------------------------------------------------


def split_path(path):
    """Split 'a.b[2].c' into ['a', 'b', 2, 'c']. A backslash escapes a dot."""
    parts = []
    buf = ""
    i = 0
    while i < len(path):
        ch = path[i]
        if ch == "\\" and i + 1 < len(path) and path[i + 1] == ".":
            buf += "."
            i += 1
            continue
        if ch == ".":
            if buf:
                parts.append(buf)
            buf = ""
        elif ch == "[":
            end = path.find("]", i)
            if end == -1:
                raise PathError("unclosed [ in %r" % path)
            index = path[i + 1:end]
            if not index.isdigit():
                raise PathError("bad index %r in %r" % (index, path))
            if buf:
                parts.append(buf)
                buf = ""
            parts.append(int(index))
            i = end + 1
            continue
        else:
            buf += ch
        i += 1
    if buf:
        parts.append(buf)
    return parts


def join_path(parts):
    """The inverse of split_path."""
    out = ""
    for part in parts:
        if isinstance(part, int):
            out += "[%d]" % part
        else:
            piece = part.replace(".", "\\.")
            out += piece if not out else "." + piece
    return out


def get_path(cfg, path, default=None):
    """Read a value by path, or return default when any step is missing."""
    node = cfg
    for part in split_path(path):
        if isinstance(part, int):
            if not isinstance(node, list) or part >= len(node):
                return default
            node = node[part]
        else:
            if not isinstance(node, dict) or part not in node:
                return default
            node = node[part]
    return node


def has_path(cfg, path):
    return get_path(cfg, path, MISSING) is not MISSING


def require(cfg, *paths):
    """Raise ConfigError naming every path that has no value in cfg."""
    absent = [path for path in paths if not has_path(cfg, path)]
    if absent:
        raise ConfigError("missing required keys: %s" % ", ".join(absent))
    return cfg


def set_path(cfg, path, value):
    """Write a value by path, creating dicts on the way. Returns cfg."""
    parts = split_path(path)
    if not parts:
        raise PathError("empty path")
    node = cfg
    for part, nxt in zip(parts, parts[1:]):
        if isinstance(part, int):
            if not isinstance(node, list) or part >= len(node):
                raise PathError("no item %d on the way to %r" % (part, path))
            if node[part] is None:
                node[part] = [] if isinstance(nxt, int) else {}
            node = node[part]
        else:
            if not isinstance(node, dict):
                raise PathError("%r is not a table on the way to %r" % (part, path))
            if part not in node or node[part] is None:
                node[part] = [] if isinstance(nxt, int) else {}
            node = node[part]
    last = parts[-1]
    if isinstance(last, int):
        if not isinstance(node, list):
            raise PathError("%r does not end in a list" % path)
        while len(node) <= last:
            node.append(None)
        node[last] = value
    else:
        if not isinstance(node, dict):
            raise PathError("%r does not end in a table" % path)
        node[last] = value
    return cfg


def del_path(cfg, path):
    """Delete a value by path. Returns True when something was removed."""
    parts = split_path(path)
    if not parts:
        return False
    parent = get_path(cfg, join_path(parts[:-1])) if len(parts) > 1 else cfg
    last = parts[-1]
    if isinstance(parent, dict) and last in parent:
        del parent[last]
        return True
    if isinstance(parent, list) and isinstance(last, int) and last < len(parent):
        del parent[last]
        return True
    return False


# -- merging ----------------------------------------------------------------


def _directive(raw_key):
    """Read a merge directive from a key.

    'name+' appends to a list, '+name' prepends, 'name-' removes the listed
    items, 'name!' replaces the value without merging.
    """
    if raw_key.startswith("+") and len(raw_key) > 1:
        return raw_key[1:], "prepend"
    if raw_key.endswith("+") and len(raw_key) > 1:
        return raw_key[:-1], "append"
    if raw_key.endswith("-") and len(raw_key) > 1:
        return raw_key[:-1], "remove"
    if raw_key.endswith("!") and len(raw_key) > 1:
        return raw_key[:-1], "replace"
    return raw_key, "merge"


def _as_list(value):
    if isinstance(value, list):
        return value
    return [value]


def _apply_append(base, extra):
    """Items of extra after the items of base."""
    return list(_as_list(base)) + list(_as_list(extra))


def _apply_prepend(base, extra):
    """Items of extra before the items of base."""
    return list(_as_list(base)) + list(_as_list(extra))


def _apply_remove(base, gone):
    gone = _as_list(gone)
    return [item for item in _as_list(base) if item not in gone]


def deep_merge(base, over):
    """Return base with over merged on top. Neither argument is changed.

    Tables merge key by key. Anything else in over replaces the value in base.
    A None in over deletes the key.
    """
    result = copy.deepcopy(base)
    for raw_key, value in over.items():
        key, mode = _directive(raw_key)
        if mode == "append":
            result[key] = _apply_append(result.get(key, []), copy.deepcopy(value))
        elif mode == "prepend":
            result[key] = _apply_prepend(result.get(key, []), copy.deepcopy(value))
        elif mode == "remove":
            result[key] = _apply_remove(result.get(key, []), value)
        elif mode == "replace":
            result[key] = copy.deepcopy(value)
        elif value is None:
            result.pop(key, None)
        elif isinstance(value, dict) and isinstance(result.get(key), dict):
            result[key] = deep_merge(result[key], value)
        else:
            result[key] = copy.deepcopy(value)
    return result


def merge_layers(layers):
    """Fold a list of layers, first layer lowest."""
    result = {}
    for layer in layers:
        result = deep_merge(result, layer)
    return result


def merge_with_provenance(layers):
    """Merge layers and say which layer set each flat key last.

    Returns (config, {flat_path: layer_index}).
    """
    result = {}
    origin = {}
    for index, layer in enumerate(layers):
        before = flatten(result)
        result = deep_merge(result, layer)
        after = flatten(result)
        for path, value in after.items():
            if path not in before or before[path] != value:
                origin[path] = index
        for path in before:
            if path not in after:
                origin.pop(path, None)
    return result, origin


# -- flat views ---------------------------------------------------------------


def flatten(cfg, prefix=()):
    """Map every leaf to its path. Lists count as leaves."""
    flat = {}
    for key, value in cfg.items():
        here = prefix + (key,)
        if isinstance(value, dict) and value:
            flat.update(flatten(value, here))
        else:
            flat[join_path(here)] = value
    return flat


def unflatten(flat):
    """The inverse of flatten."""
    cfg = {}
    for path in sorted(flat):
        set_path(cfg, path, copy.deepcopy(flat[path]))
    return cfg


# -- environment ----------------------------------------------------------------


def parse_scalar(text):
    """Turn an environment string into a bool, number, list, table or string."""
    stripped = text.strip()
    lowered = stripped.lower()
    if lowered in ("true", "false"):
        return lowered == "true"
    if lowered in ("null", "none", ""):
        return None
    if re.fullmatch(r"[+-]?\d+", stripped):
        return int(stripped)
    if re.fullmatch(r"[+-]?(\d+\.\d*|\.\d+)([eE][+-]?\d+)?", stripped):
        return float(stripped)
    if stripped[:1] in "[{":
        try:
            return json.loads(stripped)
        except ValueError:
            return text
    return text


def parse_env(environ, prefix="APP_", sep="__"):
    """Build a layer from environment variables.

    APP_DB__HOST=x becomes {"db": {"host": "x"}}. Names are lowercased.
    """
    layer = {}
    for name in sorted(environ):
        if not name.startswith(prefix):
            continue
        key = name[len(prefix):].lower()
        if not key:
            continue
        parts = [p for p in key.split(sep.lower()) if p]
        node = layer
        for part in parts[:-1]:
            if not isinstance(node.get(part), dict):
                node[part] = {}
            node = node[part]
        node[parts[-1]] = parse_scalar(environ[name])
    return layer


# -- interpolation ----------------------------------------------------------------

_REF = re.compile(r"\$\{([^}]+)\}")


def _lookup(expr, root, env, stack):
    if expr.startswith("env:"):
        name, sep, default = expr[4:].partition(":-")
        if name in env:
            return env[name]
        if sep:
            return default
        raise ConfigError("missing environment variable %s" % name)
    if expr in stack:
        raise ConfigError("reference cycle: %s" % " -> ".join(stack + [expr]))
    value = get_path(root, expr, MISSING)
    if value is MISSING:
        raise ConfigError("unknown reference ${%s}" % expr)
    if isinstance(value, str):
        return _expand(value, root, env, stack + [expr])
    return value


def _expand(text, root, env, stack):
    match = _REF.search(text)
    if match is None:
        return text
    if match.end() == len(text):
        return _lookup(match.group(1), root, env, stack)

    def swap(found):
        return str(_lookup(found.group(1), root, env, stack))

    return _REF.sub(swap, text)


def interpolate(cfg, env=None):
    """Replace ${path} and ${env:NAME:-default} inside every string.

    A string that is exactly one reference takes the type of what it points at.
    """
    env = env if env is not None else {}

    def walk(node):
        if isinstance(node, dict):
            return {key: walk(value) for key, value in node.items()}
        if isinstance(node, list):
            return [walk(item) for item in node]
        if isinstance(node, str):
            return _expand(node, cfg, env, [])
        return node

    return walk(cfg)


# -- diffs --------------------------------------------------------------------------


def diff(old, new):
    """List the changes that turn old into new, sorted by path.

    Each change is ("add" | "remove" | "change", path, old_value, new_value).
    """
    a = flatten(old)
    b = flatten(new)
    changes = []
    for path in sorted(set(a) | set(b)):
        if path not in a:
            changes.append(("add", path, None, b[path]))
        elif path not in b:
            changes.append(("remove", path, a[path], None))
        elif a[path] != b[path]:
            changes.append(("change", path, a[path], b[path]))
    return changes


def apply_diff(cfg, changes):
    """Apply a list from diff() to a copy of cfg."""
    result = copy.deepcopy(cfg)
    for kind, path, _old, new in changes:
        if kind == "remove":
            del_path(result, path)
        else:
            set_path(result, path, copy.deepcopy(new))
    return result


# -- output ---------------------------------------------------------------------------


def redact(cfg, patterns=("password", "secret", "token", "key")):
    """Copy of cfg with secret-looking values replaced by '***'."""
    out = {}
    for key, value in cfg.items():
        if isinstance(value, dict):
            out[key] = redact(value, patterns)
        elif any(p in key.lower() for p in patterns):
            out[key] = "***"
        else:
            out[key] = copy.deepcopy(value)
    return out


def to_json(cfg):
    """Stable text form: sorted keys, two-space indent."""
    return json.dumps(cfg, sort_keys=True, indent=2)


def from_json(text):
    try:
        data = json.loads(text)
    except ValueError as error:
        raise ConfigError("bad JSON: %s" % error)
    if not isinstance(data, dict):
        raise ConfigError("top level must be a table")
    return data

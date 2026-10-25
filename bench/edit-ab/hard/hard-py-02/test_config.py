import unittest

from merge import (ConfigError, PathError, apply_diff, deep_merge, del_path, diff, flatten,
                   get_path, has_path, interpolate, join_path, merge_layers,
                   merge_with_provenance, parse_env, parse_scalar, redact, set_path,
                   split_path, unflatten)
from schema import Field, STRIP, validate


class PathTest(unittest.TestCase):
    def test_split_plain_and_indexed(self):
        self.assertEqual(split_path("a.b[2].c"), ["a", "b", 2, "c"])
        self.assertEqual(split_path("x[0][1]"), ["x", 0, 1])

    def test_escaped_dot_stays_in_the_key(self):
        self.assertEqual(split_path("a\\.b.c"), ["a.b", "c"])
        self.assertEqual(join_path(["a.b", "c", 3]), "a\\.b.c[3]")

    def test_bad_paths(self):
        with self.assertRaises(PathError):
            split_path("a[x]")
        with self.assertRaises(PathError):
            split_path("a[1")

    def test_get_set_del(self):
        cfg = {"a": {"b": [10, 20, {"c": 1}]}}
        self.assertEqual(get_path(cfg, "a.b[2].c"), 1)
        self.assertEqual(get_path(cfg, "a.b[9]", "none"), "none")
        set_path(cfg, "a.d.e", 5)
        self.assertEqual(cfg["a"]["d"], {"e": 5})
        set_path(cfg, "a.b[1]", 21)
        self.assertEqual(cfg["a"]["b"][1], 21)
        self.assertTrue(del_path(cfg, "a.b[0]"))
        self.assertEqual(cfg["a"]["b"][0], 21)
        self.assertFalse(del_path(cfg, "zzz"))
        self.assertTrue(has_path(cfg, "a.d"))

    def test_keys_with_dots_round_trip_through_flatten(self):
        cfg = {"host.name": {"v": 1}, "plain": 2}
        flat = flatten(cfg)
        self.assertEqual(flat, {"host\\.name.v": 1, "plain": 2})
        self.assertEqual(unflatten(flat), cfg)


class MergeTest(unittest.TestCase):
    def test_tables_merge_scalars_replace(self):
        base = {"db": {"host": "a", "port": 1}, "debug": False}
        over = {"db": {"port": 2}, "debug": True}
        self.assertEqual(deep_merge(base, over),
                         {"db": {"host": "a", "port": 2}, "debug": True})
        self.assertEqual(base["db"]["port"], 1)

    def test_none_deletes_a_key(self):
        base = {"a": 1, "b": {"c": 2, "d": 3}}
        self.assertEqual(deep_merge(base, {"a": None, "b": {"c": None}}),
                         {"b": {"d": 3}})

    def test_append_directive(self):
        base = {"tags": ["x", "y"]}
        self.assertEqual(deep_merge(base, {"tags+": ["z"]}), {"tags": ["x", "y", "z"]})
        self.assertEqual(deep_merge(base, {"new+": "one"}), {"tags": ["x", "y"], "new": ["one"]})

    def test_prepend_directive(self):
        base = {"tags": ["x", "y"]}
        self.assertEqual(deep_merge(base, {"+tags": ["a", "b"]}),
                         {"tags": ["a", "b", "x", "y"]})

    def test_remove_and_replace_directives(self):
        base = {"tags": ["x", "y", "z"], "db": {"host": "h", "port": 1}}
        out = deep_merge(base, {"tags-": ["y"], "db!": {"host": "other"}})
        self.assertEqual(out, {"tags": ["x", "z"], "db": {"host": "other"}})

    def test_layers_and_provenance(self):
        layers = [{"a": 1, "b": {"c": 1}}, {"b": {"c": 2, "d": 3}}, {"a": None}]
        cfg, origin = merge_with_provenance(layers)
        self.assertEqual(cfg, merge_layers(layers))
        self.assertEqual(cfg, {"b": {"c": 2, "d": 3}})
        self.assertEqual(origin, {"b.c": 1, "b.d": 1})


class EnvTest(unittest.TestCase):
    def test_scalars(self):
        self.assertIs(parse_scalar("TRUE"), True)
        self.assertEqual(parse_scalar("42"), 42)
        self.assertEqual(parse_scalar("-1.5"), -1.5)
        self.assertEqual(parse_scalar('["a", 2]'), ["a", 2])
        self.assertEqual(parse_scalar("hello"), "hello")
        self.assertEqual(parse_scalar("[oops"), "[oops")

    def test_parse_env_builds_a_nested_layer(self):
        env = {"APP_DB__HOST": "h", "APP_DB__PORT": "5432", "APP_DEBUG": "true",
               "OTHER_X": "1", "APP_": "skip"}
        self.assertEqual(parse_env(env),
                         {"db": {"host": "h", "port": 5432}, "debug": True})


class InterpolateTest(unittest.TestCase):
    def test_whole_reference_keeps_the_type(self):
        cfg = {"port": 80, "url": "${port}", "name": "web"}
        self.assertEqual(interpolate(cfg)["url"], 80)

    def test_reference_inside_text_is_a_string(self):
        cfg = {"port": 80, "url": "http://h:${port}/x", "tail": "name-${port}"}
        out = interpolate(cfg)
        self.assertEqual(out["url"], "http://h:80/x")
        self.assertEqual(out["tail"], "name-80")

    def test_text_before_a_reference_is_kept(self):
        cfg = {"v": "1", "w": "v${v}"}
        self.assertEqual(interpolate(cfg)["w"], "v1")

    def test_env_with_default(self):
        cfg = {"a": "${env:HOME_DIR:-/tmp}", "b": "${env:X}"}
        out = interpolate(cfg, {"X": "set"})
        self.assertEqual(out["a"], "/tmp")
        self.assertEqual(out["b"], "set")

    def test_missing_and_cycles(self):
        with self.assertRaises(ConfigError):
            interpolate({"a": "${nope}"})
        with self.assertRaises(ConfigError):
            interpolate({"a": "${b}", "b": "${a}"})
        with self.assertRaises(ConfigError):
            interpolate({"a": "${env:NOPE}"}, {})

    def test_chains(self):
        cfg = {"a": "${b}/x", "b": "${c}/y", "c": "root"}
        self.assertEqual(interpolate(cfg)["a"], "root/y/x")


class DiffTest(unittest.TestCase):
    def test_diff_and_apply(self):
        old = {"a": 1, "b": {"c": 2}, "gone": True}
        new = {"a": 1, "b": {"c": 3, "d": 4}}
        changes = diff(old, new)
        self.assertEqual(changes, [("change", "b.c", 2, 3), ("add", "b.d", None, 4),
                                   ("remove", "gone", True, None)])
        self.assertEqual(apply_diff(old, changes), new)

    def test_redact(self):
        cfg = {"db": {"password": "p", "host": "h"}, "api_key": "k", "n": 1}
        self.assertEqual(redact(cfg),
                         {"db": {"password": "***", "host": "h"}, "api_key": "***", "n": 1})


SCHEMA = {
    "name": Field("str", required=True, min_len=1, pattern=r"[a-z][a-z0-9-]*"),
    "port": Field("int", default=8080, min=1, max=65535),
    "debug": Field("bool", default=False),
    "ratio": Field("float", min=0, max=1),
    "mode": Field("enum", choices=("fast", "safe"), default="safe"),
    "tags": Field("list", items=Field("str", min_len=2)),
    "db": Field("dict", schema={
        "host": Field("str", required=True),
        "pool": Field("int", default=4, min=1, max=64),
    }),
}


class SchemaTest(unittest.TestCase):
    def test_defaults_and_coercion(self):
        clean, errors = validate(SCHEMA, {"name": "web", "port": "9000", "db": {"host": "h"}})
        self.assertEqual(errors, [])
        self.assertEqual(clean["port"], 9000)
        self.assertEqual(clean["debug"], False)
        self.assertEqual(clean["mode"], "safe")
        self.assertEqual(clean["db"], {"host": "h", "pool": 4})

    def test_bool_words(self):
        for word, expected in (("True", True), ("OFF", False), (" yes ", True), ("no", False)):
            clean, errors = validate(SCHEMA, {"name": "a", "debug": word})
            self.assertEqual(errors, [], word)
            self.assertIs(clean["debug"], expected)
        _, errors = validate(SCHEMA, {"name": "a", "debug": "maybe"})
        self.assertEqual(len(errors), 1)

    def test_bounds_are_inclusive(self):
        for port in (1, 65535):
            _, errors = validate(SCHEMA, {"name": "a", "port": port})
            self.assertEqual(errors, [], port)
        _, errors = validate(SCHEMA, {"name": "a", "port": 65536, "ratio": 1.5})
        self.assertEqual(len(errors), 2)
        _, errors = validate(SCHEMA, {"name": "a", "port": 0, "ratio": 1})
        self.assertEqual(len(errors), 1)

    def test_every_error_is_reported_with_its_path(self):
        cfg = {"port": "x", "tags": ["ok", "b"], "db": {"pool": 100}, "extra": 1}
        _, errors = validate(SCHEMA, cfg)
        joined = "\n".join(errors)
        for needle in ("name: required", "port:", "tags[1]:", "db.host: required",
                       "db.pool:", "extra: unknown key"):
            self.assertIn(needle, joined)

    def test_unknown_policies(self):
        cfg = {"name": "a", "zzz": 1}
        clean, errors = validate(SCHEMA, cfg, unknown="ignore")
        self.assertEqual((clean["zzz"], errors), (1, []))
        clean, errors = validate(SCHEMA, cfg, unknown=STRIP)
        self.assertNotIn("zzz", clean)

    def test_merge_then_validate(self):
        layers = [{"name": "svc", "db": {"host": "a"}}, parse_env({"APP_DB__POOL": "8"})]
        clean, errors = validate(SCHEMA, merge_layers(layers))
        self.assertEqual(errors, [])
        self.assertEqual(clean["db"]["pool"], 8)


if __name__ == "__main__":
    unittest.main()

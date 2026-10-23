import unittest

from cli_args import ArgError, parse_args, usage


class CliArgsTest(unittest.TestCase):
    def test_defaults(self):
        opts, rest = parse_args([])
        self.assertEqual(opts["jobs"], 1)
        self.assertEqual(opts["retries"], 3)
        self.assertEqual(opts["verbose"], 0)
        self.assertEqual(rest, [])

    def test_long_value_forms(self):
        opts, _ = parse_args(["--jobs", "4", "--retries=7"])
        self.assertEqual((opts["jobs"], opts["retries"]), (4, 7))

    def test_short_options(self):
        opts, rest = parse_args(["-vv", "-j8", "-n", "a.txt"])
        self.assertEqual(opts["verbose"], 2)
        self.assertEqual(opts["jobs"], 8)
        self.assertTrue(opts["dry_run"])
        self.assertEqual(rest, ["a.txt"])

    def test_flag_and_dashes(self):
        opts, _ = parse_args(["--dry-run"])
        self.assertTrue(opts["dry_run"])

    def test_double_dash_ends_options(self):
        opts, rest = parse_args(["-v", "--", "--jobs", "-x"])
        self.assertEqual(opts["verbose"], 1)
        self.assertEqual(rest, ["--jobs", "-x"])

    def test_errors(self):
        with self.assertRaises(ArgError):
            parse_args(["--nope"])
        with self.assertRaises(ArgError):
            parse_args(["--jobs"])
        with self.assertRaises(ArgError):
            parse_args(["--jobs", "x"])

    def test_usage_mentions_defaults(self):
        self.assertIn("default: 3", usage())


if __name__ == "__main__":
    unittest.main()

import unittest

import wrap as w


class WrapTest(unittest.TestCase):
    def test_wrap_fills_the_line_to_the_width(self):
        self.assertEqual(w.wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"])

    def test_wrap_longer_word(self):
        self.assertEqual(w.wrap("hi extraordinary day", 6), ["hi", "extraordinary", "day"])

    def test_wrap_empty(self):
        self.assertEqual(w.wrap("", 10), [])

    def test_banner(self):
        self.assertEqual(w.banner("Hi", 10), "=== Hi ===")

    def test_banner_odd(self):
        self.assertEqual(w.banner("abc", 10), "== abc ===")

    def test_bullet_list(self):
        got = w.bullet_list(["one two three four"], width=11)
        self.assertEqual(got, ["* one two", "  three", "  four"])

    def test_truncate_and_pad(self):
        self.assertEqual(w.truncate("abcdefgh", 6), "abc...")
        self.assertEqual(w.pad("ab", 4, "."), "ab..")
        self.assertEqual(w.pad_left("ab", 4, "."), "..ab")

    def test_table_row(self):
        self.assertEqual(w.table_row(["a", 12, "xyz"], [3, 4, 3]), "a   | 12   | xyz")

    def test_justify(self):
        self.assertEqual(w.justify("a b c", 9), "a   b   c")


if __name__ == "__main__":
    unittest.main()

import unittest

import csvish


class CsvishTest(unittest.TestCase):
    def test_default_delimiter_is_comma(self):
        self.assertEqual(csvish.parse_line("a,b,c"), ["a", "b", "c"])

    def test_quoted_delimiter(self):
        self.assertEqual(csvish.parse_line('a,"b,c",d'), ["a", "b,c", "d"])

    def test_escaped_quote(self):
        self.assertEqual(csvish.parse_line('x,"say ""hi""",y'), ["x", 'say "hi"', "y"])

    def test_custom_delimiter(self):
        self.assertEqual(csvish.parse_line("a;b", ";"), ["a", "b"])

    def test_empty_fields(self):
        self.assertEqual(csvish.parse_line("a,,"), ["a", "", ""])

    def test_rows(self):
        text = "Name,Age,Ok\nann,31,yes\n\nbob,,no\n"
        rows = csvish.parse_rows(text, {"age": "int", "ok": "bool"})
        self.assertEqual(rows, [
            {"name": "ann", "age": 31, "ok": True},
            {"name": "bob", "age": None, "ok": False},
        ])

    def test_bad_field_count(self):
        with self.assertRaises(ValueError):
            csvish.parse_rows("a,b\n1\n")

    def test_round_trip(self):
        rows = [{"a": 'he said "no"', "b": "x,y"}]
        text = csvish.format_rows(rows, ["a", "b"])
        self.assertEqual(csvish.parse_rows(text), rows)

    def test_sum_column(self):
        rows = csvish.parse_rows("v\n1.5\n\n2\n", {"v": "float"})
        self.assertEqual(csvish.sum_column(rows, "v"), 3.5)


if __name__ == "__main__":
    unittest.main()

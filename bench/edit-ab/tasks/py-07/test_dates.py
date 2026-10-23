import unittest

import dates


class DatesTest(unittest.TestCase):
    def test_leap_years(self):
        self.assertTrue(dates.is_leap(2024))
        self.assertTrue(dates.is_leap(2000))
        self.assertFalse(dates.is_leap(1900))
        self.assertFalse(dates.is_leap(2023))

    def test_days_in_february(self):
        self.assertEqual(dates.days_in_month(2024, 2), 29)
        self.assertEqual(dates.days_in_month(2023, 2), 28)

    def test_next_and_prev_month(self):
        self.assertEqual(dates.next_month(2023, 12), (2024, 1))
        self.assertEqual(dates.prev_month(2024, 1), (2023, 12))
        self.assertEqual(dates.prev_month(2024, 5), (2024, 4))
        self.assertEqual(dates.prev_month(2024, 12), (2024, 11))

    def test_add_months_clamps(self):
        self.assertEqual(dates.add_months((2024, 1, 31), 1), (2024, 2, 29))
        self.assertEqual(dates.add_months((2024, 3, 15), -4), (2023, 11, 15))

    def test_weekday(self):
        self.assertEqual(dates.weekday_name((2024, 1, 1)), "Mon")
        self.assertEqual(dates.weekday_name((2000, 2, 29)), "Tue")

    def test_add_days_backwards(self):
        self.assertEqual(dates.add_days((2024, 3, 1), -1), (2024, 2, 29))
        self.assertEqual(dates.add_days((2024, 1, 5), -10), (2023, 12, 26))
        self.assertEqual(dates.add_days((2023, 12, 30), 5), (2024, 1, 4))

    def test_days_between(self):
        self.assertEqual(dates.days_between((2024, 1, 1), (2025, 1, 1)), 366)

    def test_iso(self):
        self.assertEqual(dates.parse_iso("2024-02-29"), (2024, 2, 29))
        self.assertEqual(dates.format_iso((2024, 2, 9)), "2024-02-09")
        with self.assertRaises(ValueError):
            dates.parse_iso("2023-02-29")

    def test_month_calendar(self):
        rows = dates.month_calendar(2024, 2)
        self.assertEqual(rows[0], [0, 0, 0, 1, 2, 3, 4])
        self.assertEqual(rows[-1], [26, 27, 28, 29, 0, 0, 0])


if __name__ == "__main__":
    unittest.main()

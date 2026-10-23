import unittest

import ranges


class RangesTest(unittest.TestCase):
    def test_merge_overlap(self):
        self.assertEqual(ranges.merge([(5, 9), (1, 3), (2, 4)]), [(1, 4), (5, 9)])

    def test_merge_touching(self):
        self.assertEqual(ranges.merge([(1, 3), (3, 6)]), [(1, 6)])

    def test_merge_empty(self):
        self.assertEqual(ranges.merge([]), [])

    def test_total_covered(self):
        self.assertEqual(ranges.total_covered([(0, 4), (2, 6), (10, 12)]), 8)

    def test_total_covered_empty(self):
        self.assertEqual(ranges.total_covered([]), 0)

    def test_gaps(self):
        self.assertEqual(ranges.gaps([(2, 4), (6, 7)], 0, 10), [(0, 2), (4, 6), (7, 10)])

    def test_clip(self):
        self.assertEqual(ranges.clip([(0, 3), (8, 12), (20, 30)], 2, 10), [(2, 3), (8, 10)])

    def test_describe(self):
        self.assertEqual(ranges.describe([(5, 9), (1, 3), (3, 4)]), "1-4, 5-9")

    def test_intersect(self):
        self.assertEqual(ranges.intersect((1, 5), (5, 8)), (5, 5))
        self.assertIsNone(ranges.intersect((1, 4), (5, 8)))


if __name__ == "__main__":
    unittest.main()

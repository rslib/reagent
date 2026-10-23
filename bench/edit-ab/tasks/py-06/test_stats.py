import math
import unittest

import stats


class StatsTest(unittest.TestCase):
    def test_median_odd(self):
        self.assertEqual(stats.median([5, 1, 3]), 3)

    def test_median_even(self):
        self.assertEqual(stats.median([4, 1, 3, 2]), 2.5)

    def test_median_two(self):
        self.assertEqual(stats.median([10, 20]), 15)

    def test_sample_variance(self):
        self.assertAlmostEqual(stats.variance([2, 4, 4, 4, 5, 5, 7, 9]), 32 / 7)

    def test_population_variance(self):
        self.assertAlmostEqual(stats.variance([2, 4, 4, 4, 5, 5, 7, 9], sample=False), 4.0)

    def test_stdev(self):
        self.assertAlmostEqual(stats.stdev([1, 2, 3, 4]), math.sqrt(5 / 3))

    def test_percentile_interpolates(self):
        self.assertAlmostEqual(stats.percentile([1, 2, 3, 4], 50), 2.5)

    def test_percentile_top(self):
        self.assertEqual(stats.percentile([1, 2, 3, 4], 100), 4)
        self.assertEqual(stats.percentile([7], 100), 7)

    def test_mode_and_mean(self):
        self.assertEqual(stats.mode([3, 1, 3, 1, 2]), 1)
        self.assertEqual(stats.mean([1, 2, 6]), 3)

    def test_histogram(self):
        self.assertEqual(stats.histogram([1, 2, 3, 4, 5, 6], 3), [2, 2, 2])

    def test_moving_average(self):
        self.assertEqual(stats.moving_average([1, 2, 3, 4], 2), [1.5, 2.5, 3.5])


if __name__ == "__main__":
    unittest.main()

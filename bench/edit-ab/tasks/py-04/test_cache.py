import unittest

from cache import Cache


class CacheTest(unittest.TestCase):
    def test_evicts_least_recently_used(self):
        c = Cache(capacity=2)
        c.put("a", 1)
        c.put("b", 2)
        c.get("a")
        c.put("c", 3)
        self.assertEqual(c.keys(), ["a", "c"])

    def test_ttl_boundary_is_expired(self):
        c = Cache(ttl=10)
        c.put("a", 1, now=0)
        self.assertEqual(c.get("a", now=9), 1)
        self.assertIsNone(c.get("a", now=10))

    def test_purge_counts_boundary(self):
        c = Cache(ttl=5)
        c.put("a", 1, now=0)
        c.put("b", 2, now=3)
        self.assertEqual(c.purge(now=5), 1)
        self.assertEqual(c.keys(), ["b"])

    def test_put_many_evicts_oldest(self):
        c = Cache(capacity=3)
        c.put_many([("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)])
        self.assertEqual(c.keys(), ["c", "d", "e"])

    def test_pop_newest(self):
        c = Cache(capacity=3)
        c.put_many([("a", 1), ("b", 2)])
        self.assertEqual(c.pop_newest(), ("b", 2))
        self.assertEqual(c.keys(), ["a"])

    def test_hit_rate(self):
        c = Cache()
        c.put("a", 1)
        c.get("a")
        c.get("zzz")
        self.assertEqual(c.hit_rate(), 0.5)

    def test_resize_and_clear(self):
        c = Cache(capacity=4)
        c.put_many([("a", 1), ("b", 2), ("c", 3)])
        c.resize(2)
        self.assertEqual(c.keys(), ["b", "c"])
        c.clear()
        self.assertEqual(len(c), 0)


if __name__ == "__main__":
    unittest.main()

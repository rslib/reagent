"""A small LRU cache with an optional time to live per entry."""

from collections import OrderedDict


class Cache:
    """Least recently used cache.

    Time is passed in by the caller as `now` (seconds), which keeps the class
    free of clocks. An entry older than `ttl` seconds counts as gone. With
    ttl=None entries never expire.
    """

    def __init__(self, capacity=128, ttl=None):
        if capacity < 1:
            raise ValueError("capacity must be at least 1")
        self.capacity = capacity
        self.ttl = ttl
        self._data = OrderedDict()
        self.hits = 0
        self.misses = 0

    def __len__(self):
        return len(self._data)

    def _expired(self, stamp, now):
        if self.ttl is None:
            return False
        return now - stamp > self.ttl

    def get(self, key, now=0, default=None):
        entry = self._data.get(key)
        if entry is None:
            self.misses += 1
            return default
        value, stamp = entry
        if self._expired(stamp, now):
            del self._data[key]
            self.misses += 1
            return default
        self._data.move_to_end(key)
        self.hits += 1
        return value

    def put(self, key, value, now=0):
        self._data[key] = (value, now)
        self._data.move_to_end(key)
        if len(self._data) > self.capacity:
            self._data.popitem(last=False)

    def put_many(self, pairs, now=0):
        for key, value in pairs:
            self._data[key] = (value, now)
            self._data.move_to_end(key)
            if len(self._data) > self.capacity:
                self._data.popitem(last=True)

    def pop_newest(self):
        """Remove and return the most recently used (key, value)."""
        if not self._data:
            raise KeyError("cache is empty")
        key, (value, _) = self._data.popitem(last=True)
        return key, value

    def discard(self, key):
        self._data.pop(key, None)

    def purge(self, now):
        """Drop every expired entry and return how many were dropped."""
        stale = [k for k, (_, stamp) in self._data.items() if self._expired(stamp, now)]
        for key in stale:
            del self._data[key]
        return len(stale)

    def keys(self):
        """Keys from least to most recently used."""
        return list(self._data)

    def hit_rate(self):
        total = self.hits + self.misses
        if total == 0:
            return 0.0
        return self.hits / total

    def resize(self, capacity):
        if capacity < 1:
            raise ValueError("capacity must be at least 1")
        self.capacity = capacity
        while len(self._data) > self.capacity:
            self._data.popitem(last=False)

    def clear(self):
        self._data.clear()
        self.hits = 0
        self.misses = 0

    def stats(self):
        """A snapshot of the counters, handy for logging."""
        return {
            "size": len(self._data),
            "capacity": self.capacity,
            "hits": self.hits,
            "misses": self.misses,
            "hit_rate": round(self.hit_rate(), 3),
        }

    def __iter__(self):
        return iter(list(self._data))

    def oldest(self):
        """The least recently used key, or None when the cache is empty."""
        for key in self._data:
            return key
        return None

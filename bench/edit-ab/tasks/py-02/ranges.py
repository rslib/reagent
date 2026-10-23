"""Helpers for closed integer intervals, stored as (lo, hi) tuples."""


def normalize(interval):
    """Return the interval with lo <= hi."""
    lo, hi = interval
    if lo > hi:
        lo, hi = hi, lo
    return (lo, hi)


def length(interval):
    lo, hi = normalize(interval)
    return hi - lo


def contains(interval, point):
    lo, hi = normalize(interval)
    return lo <= point <= hi


def overlaps(a, b):
    a = normalize(a)
    b = normalize(b)
    return a[0] <= b[1] and b[0] <= a[1]


def intersect(a, b):
    """The shared part of two intervals, or None when they are apart."""
    a = normalize(a)
    b = normalize(b)
    lo = max(a[0], b[0])
    hi = min(a[1], b[1])
    if lo > hi:
        return None
    return (lo, hi)


def merge(intervals):
    """Merge overlapping or touching intervals into a sorted list."""
    ordered = sorted(normalize(i) for i in intervals)
    merged = [ordered[0]]
    for lo, hi in ordered[1:]:
        last_lo, last_hi = merged[-1]
        if lo < last_hi:
            merged[-1] = (last_lo, max(last_hi, hi))
        else:
            merged.append((lo, hi))
    return merged


def total_covered(intervals):
    """Sum of lengths after merging, so overlaps count once."""
    return sum(length(i) for i in merge(intervals))


def gaps(intervals, lo, hi):
    """The parts of [lo, hi] that no interval covers."""
    out = []
    cursor = lo
    for start, end in merge(intervals):
        if end < cursor:
            continue
        if start > hi:
            break
        if start > cursor:
            out.append((cursor, start))
        cursor = max(cursor, end)
    if cursor < hi:
        out.append((cursor, hi))
    return out


def clip(intervals, lo, hi):
    """Cut every interval down to [lo, hi], dropping the ones left empty."""
    window = (lo, hi)
    clipped = []
    for interval in intervals:
        part = intersect(interval, window)
        if part is not None:
            clipped.append(part)
    return clipped


def shift(intervals, delta):
    return [(lo + delta, hi + delta) for lo, hi in map(normalize, intervals)]


def describe(intervals):
    """Text such as '1-3, 5-9' for a list of intervals."""
    return ", ".join("%d-%d" % i for i in merge(intervals))


def split_at(interval, points):
    """Cut an interval at the given points, in order."""
    lo, hi = normalize(interval)
    pieces = []
    cursor = lo
    for point in sorted(points):
        if cursor < point < hi:
            pieces.append((cursor, point))
            cursor = point
    pieces.append((cursor, hi))
    return pieces


def nearest(intervals, point):
    """The interval closest to a point; distance is 0 inside an interval."""
    best = None
    best_distance = None
    for interval in intervals:
        lo, hi = normalize(interval)
        distance = 0 if lo <= point <= hi else min(abs(point - lo), abs(point - hi))
        if best is None or distance < best_distance:
            best = (lo, hi)
            best_distance = distance
    return best

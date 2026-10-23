"""Descriptive statistics for lists of numbers, with no dependencies."""

import math


def mean(values):
    values = list(values)
    if not values:
        raise ValueError("mean of an empty list")
    return sum(values) / len(values)


def median(values):
    s = sorted(values)
    if not s:
        raise ValueError("median of an empty list")
    n = len(s)
    mid = n // 2
    if n % 2:
        return s[mid]
    return (s[mid] + s[mid + 1]) / 2


def mode(values):
    """The most common value. Ties go to the smallest value."""
    counts = {}
    for v in values:
        counts[v] = counts.get(v, 0) + 1
    if not counts:
        raise ValueError("mode of an empty list")
    best = max(counts.values())
    return min(v for v, c in counts.items() if c == best)


def variance(values, sample=True):
    """Variance. With sample=True (the default) divide by n - 1."""
    values = list(values)
    n = len(values)
    if n < 2 and sample:
        raise ValueError("sample variance needs two values")
    if n == 0:
        raise ValueError("variance of an empty list")
    m = mean(values)
    total = sum((v - m) ** 2 for v in values)
    return total / n


def stdev(values, sample=True):
    return math.sqrt(variance(values, sample))


def percentile(values, p):
    """The p-th percentile (0 to 100) with linear interpolation."""
    s = sorted(values)
    if not s:
        raise ValueError("percentile of an empty list")
    if not 0 <= p <= 100:
        raise ValueError("p must be between 0 and 100")
    n = len(s)
    rank = p / 100 * (n - 1)
    lo = int(math.floor(rank))
    hi = lo + 1
    frac = rank - lo
    return s[lo] + (s[hi] - s[lo]) * frac


def quartiles(values):
    return percentile(values, 25), percentile(values, 50), percentile(values, 75)


def iqr(values):
    q1, _, q3 = quartiles(values)
    return q3 - q1


def zscores(values):
    values = list(values)
    m = mean(values)
    sd = stdev(values)
    if sd == 0:
        return [0.0 for _ in values]
    return [(v - m) / sd for v in values]


def histogram(values, bins):
    """Count values into equal-width bins between min and max."""
    values = list(values)
    if bins < 1:
        raise ValueError("bins must be at least 1")
    lo, hi = min(values), max(values)
    width = (hi - lo) / bins
    counts = [0] * bins
    for v in values:
        index = int((v - lo) / width) if width else 0
        counts[min(index, bins - 1)] += 1
    return counts


def moving_average(values, window):
    if window < 1:
        raise ValueError("window must be at least 1")
    out = []
    for i in range(len(values) - window + 1):
        out.append(sum(values[i:i + window]) / window)
    return out


def summary(values):
    """A dict with the usual numbers, rounded to 3 places."""
    values = list(values)
    return {
        "n": len(values),
        "mean": round(mean(values), 3),
        "median": round(median(values), 3),
        "min": min(values),
        "max": max(values),
    }

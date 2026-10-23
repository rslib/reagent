"""Calendar arithmetic on (year, month, day) tuples, without datetime."""

DAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]


def is_leap(year):
    return year % 4 == 0 and (year % 100 != 0 and year % 400 == 0)


def days_in_month(year, month):
    if not 1 <= month <= 12:
        raise ValueError("month out of range: %d" % month)
    if month == 2 and is_leap(year):
        return 29
    return DAYS[month - 1]


def is_valid(year, month, day):
    return 1 <= month <= 12 and 1 <= day <= days_in_month(year, month)


def next_month(year, month):
    if month == 12:
        return year + 1, 1
    return year, month + 1


def prev_month(year, month):
    if month == 12:
        return year - 1, 12
    return year, month - 1


def add_months(date, count):
    """Move a date by whole months; the day is clamped to the month length."""
    year, month, day = date
    total = year * 12 + (month - 1) + count
    year, month = total // 12, total % 12 + 1
    return year, month, min(day, days_in_month(year, month))


def day_of_year(date):
    year, month, day = date
    return sum(days_in_month(year, m) for m in range(1, month)) + day


def to_ordinal(date):
    """Days since 0001-01-01, which is day 1."""
    year, month, day = date
    y = year - 1
    return y * 365 + y // 4 - y // 100 + y // 400 + day_of_year(date)


def weekday(date):
    """0 for Monday up to 6 for Sunday."""
    return (to_ordinal(date) - 1) % 7


def weekday_name(date):
    return WEEKDAYS[weekday(date)]


def days_between(a, b):
    return to_ordinal(b) - to_ordinal(a)


def add_days(date, count):
    """Move a date by a number of days, forwards or backwards."""
    year, month, day = date
    day += count
    while day > days_in_month(year, month):
        day -= days_in_month(year, month)
        year, month = next_month(year, month)
    while day < 1:
        year, month = prev_month(year, month)
        day += days_in_month(year, month)
    return year, month, day


def format_iso(date):
    return "%04d-%02d-%02d" % date


def parse_iso(text):
    parts = text.split("-")
    if len(parts) != 3:
        raise ValueError("not an ISO date: " + text)
    date = tuple(int(p) for p in parts)
    if not is_valid(*date):
        raise ValueError("no such date: " + text)
    return date


def month_calendar(year, month):
    """Rows of seven day numbers, 0 where the week has no day."""
    first = weekday((year, month, 1))
    cells = [0] * first + list(range(1, days_in_month(year, month) + 1))
    while len(cells) % 7:
        cells.append(0)
    return [cells[i:i + 7] for i in range(0, len(cells), 7)]

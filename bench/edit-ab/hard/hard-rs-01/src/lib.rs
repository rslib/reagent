//! Interval arithmetic and a small calendar for scheduling.
//!
//! Time is counted in whole minutes from an arbitrary zero. An `Interval` is
//! half open: it holds `start` and every minute up to but not including `end`.

use std::fmt;

pub const MINUTES_PER_DAY: i64 = 24 * 60;
pub const MINUTES_PER_WEEK: i64 = 7 * MINUTES_PER_DAY;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    /// An interval whose end is not after its start.
    Empty { start: i64, end: i64 },
    /// An event that overlaps one already in the calendar.
    Conflict { with: u32 },
    /// An id that is not in the calendar.
    UnknownEvent(u32),
    /// Text that is not a time such as `d1 09:30`.
    BadTime(String),
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalendarError::Empty { start, end } => write!(f, "interval {start}..{end} is empty"),
            CalendarError::Conflict { with } => write!(f, "overlaps event {with}"),
            CalendarError::UnknownEvent(id) => write!(f, "no event {id}"),
            CalendarError::BadTime(text) => write!(f, "bad time {text:?}"),
        }
    }
}

impl std::error::Error for CalendarError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    pub start: i64,
    pub end: i64,
}

impl Interval {
    pub fn new(start: i64, end: i64) -> Result<Interval, CalendarError> {
        if end <= start {
            return Err(CalendarError::Empty { start, end });
        }
        Ok(Interval { start, end })
    }

    pub fn len(&self) -> i64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn contains(&self, t: i64) -> bool {
        self.start <= t && t < self.end
    }

    pub fn contains_interval(&self, other: &Interval) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// True when the two share at least one minute. Touching ends do not count.
    pub fn overlaps(&self, other: &Interval) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// True when they overlap or one starts exactly where the other ends.
    pub fn touches(&self, other: &Interval) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    pub fn intersect(&self, other: &Interval) -> Option<Interval> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start < end {
            Some(Interval { start, end })
        } else {
            None
        }
    }

    /// The smallest interval holding both, when they touch.
    pub fn union(&self, other: &Interval) -> Option<Interval> {
        if self.touches(other) {
            Some(Interval {
                start: self.start.min(other.start),
                end: self.end.max(other.end),
            })
        } else {
            None
        }
    }

    pub fn shift(&self, by: i64) -> Interval {
        Interval {
            start: self.start + by,
            end: self.end + by,
        }
    }

    /// Grow the interval by `by` minutes on both sides.
    pub fn pad(&self, by: i64) -> Interval {
        Interval {
            start: self.start - by,
            end: self.end + by,
        }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", format_time(self.start), format_time(self.end))
    }
}

// -- times as text -------------------------------------------------------------

/// Format minutes as `d<day> HH:MM`, for example `d1 09:30`.
pub fn format_time(t: i64) -> String {
    let day = t.div_euclid(MINUTES_PER_DAY);
    let rest = t.rem_euclid(MINUTES_PER_DAY);
    format!("d{} {:02}:{:02}", day, rest / 60, rest % 60)
}

/// The inverse of `format_time`.
pub fn parse_time(text: &str) -> Result<i64, CalendarError> {
    let bad = || CalendarError::BadTime(text.to_string());
    let rest = text.trim().strip_prefix('d').ok_or_else(bad)?;
    let (day, clock) = rest.split_once(' ').ok_or_else(bad)?;
    let (hours, minutes) = clock.split_once(':').ok_or_else(bad)?;
    let day: i64 = day.parse().map_err(|_| bad())?;
    let hours: i64 = hours.parse().map_err(|_| bad())?;
    let minutes: i64 = minutes.parse().map_err(|_| bad())?;
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
        return Err(bad());
    }
    Ok(day * MINUTES_PER_DAY + hours * 60 + minutes)
}

// -- sets of intervals -------------------------------------------------------------

/// Sort, drop empty intervals and join those that overlap or touch.
pub fn normalize(list: &[Interval]) -> Vec<Interval> {
    let mut sorted: Vec<Interval> = list.iter().copied().filter(|i| !i.is_empty()).collect();
    sorted.sort();
    let mut out: Vec<Interval> = Vec::new();
    for next in sorted {
        match out.last_mut() {
            Some(cur) if next.start <= cur.end => {
                cur.end = next.end;
            }
            _ => out.push(next),
        }
    }
    out
}

/// Total minutes covered by the union of the intervals.
pub fn total(list: &[Interval]) -> i64 {
    normalize(list).iter().map(Interval::len).sum()
}

/// The parts of `base` that are not inside any interval of `cut`.
pub fn subtract(base: &[Interval], cut: &[Interval]) -> Vec<Interval> {
    let cuts = normalize(cut);
    let mut out = Vec::new();
    for b in normalize(base) {
        let mut start = b.start;
        for c in &cuts {
            if c.end <= start {
                continue;
            }
            if c.start >= b.end {
                break;
            }
            if c.start >= start {
                out.push(Interval { start, end: c.start });
            }
            start = start.max(c.end);
        }
        if start < b.end {
            out.push(Interval { start, end: b.end });
        }
    }
    out
}

/// The parts that are in both sets.
pub fn intersect_all(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let a = normalize(a);
    let b = normalize(b);
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if let Some(common) = a[i].intersect(&b[j]) {
            out.push(common);
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The gaps inside `window` that no busy interval covers, earliest first.
pub fn complement(window: Interval, busy: &[Interval]) -> Vec<Interval> {
    subtract(&[window], busy)
}

/// The earliest slot of `duration` minutes inside `window` that is free.
pub fn first_fit(window: Interval, busy: &[Interval], duration: i64) -> Option<Interval> {
    if duration <= 0 {
        return None;
    }
    for gap in complement(window, busy) {
        let (gap_start, gap_end) = (gap.start, gap.end);
        if gap_end - gap_start >= duration {
            let candidate = gap_start;
            return Some(Interval {
                start: candidate,
                end: candidate + duration,
            });
        }
    }
    None
}

/// The latest slot of `duration` minutes inside `window` that is free.
pub fn last_fit(window: Interval, busy: &[Interval], duration: i64) -> Option<Interval> {
    if duration <= 0 {
        return None;
    }
    for gap in complement(window, busy).into_iter().rev() {
        let (gap_start, gap_end) = (gap.start, gap.end);
        if gap_end - gap_start >= duration {
            let candidate = gap_start;
            return Some(Interval {
                start: candidate,
                end: candidate + duration,
            });
        }
    }
    None
}

/// The most intervals that cover a single minute.
pub fn max_overlap(list: &[Interval]) -> usize {
    let mut edges: Vec<(i64, i32)> = Vec::new();
    for i in list.iter().filter(|i| !i.is_empty()) {
        edges.push((i.start, 1));
        edges.push((i.end, -1));
    }
    // At the same minute an end comes before a start, so touching intervals
    // do not count as overlapping.
    edges.sort();
    let (mut now, mut best) = (0i32, 0i32);
    for (_, delta) in edges {
        now += delta;
        best = best.max(now);
    }
    best as usize
}

/// Cut every interval to the part that lies inside `window`.
pub fn clip(list: &[Interval], window: Interval) -> Vec<Interval> {
    list.iter().filter_map(|i| i.intersect(&window)).collect()
}

// -- more set helpers ------------------------------------------------------------------

/// Join intervals that are at most `gap` minutes apart.
pub fn merge_within(list: &[Interval], gap: i64) -> Vec<Interval> {
    let mut out: Vec<Interval> = Vec::new();
    for next in normalize(list) {
        match out.last_mut() {
            Some(cur) if next.start - cur.end <= gap => {
                cur.end = cur.end.max(next.end);
            }
            _ => out.push(next),
        }
    }
    out
}

/// Cut an interval at every midnight. Each piece lies inside one day.
pub fn split_days(span: Interval) -> Vec<Interval> {
    let mut out = Vec::new();
    let mut start = span.start;
    while start < span.end {
        let next_midnight = (start.div_euclid(MINUTES_PER_DAY) + 1) * MINUTES_PER_DAY;
        let end = next_midnight.min(span.end);
        out.push(Interval { start, end });
        start = end;
    }
    out
}

/// The longest free gap inside the window, earliest first on a tie.
pub fn longest_gap(window: Interval, busy: &[Interval]) -> Option<Interval> {
    let mut best: Option<Interval> = None;
    for gap in complement(window, busy) {
        if best.map_or(true, |b| gap.len() > b.len()) {
            best = Some(gap);
        }
    }
    best
}

/// Round a time up to the next multiple of `step` minutes.
pub fn round_up(t: i64, step: i64) -> i64 {
    let rem = t.rem_euclid(step);
    if rem == 0 {
        t
    } else {
        t + step - rem
    }
}

// -- events and the calendar --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: u32,
    pub title: String,
    pub span: Interval,
}

#[derive(Debug, Clone)]
pub struct Calendar {
    events: Vec<Event>,
    next_id: u32,
}

impl Calendar {
    pub fn new() -> Calendar {
        Calendar {
            events: Vec::new(),
            next_id: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Add an event. Fails when it overlaps one that is already there.
    pub fn add(&mut self, title: &str, span: Interval) -> Result<u32, CalendarError> {
        if let Some(clash) = self.events.iter().find(|e| e.span.overlaps(&span)) {
            return Err(CalendarError::Conflict { with: clash.id });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.events.push(Event {
            id,
            title: title.to_string(),
            span,
        });
        self.events.sort_by_key(|e| (e.span.start, e.id));
        Ok(id)
    }

    pub fn remove(&mut self, id: u32) -> Result<Event, CalendarError> {
        match self.events.iter().position(|e| e.id == id) {
            Some(index) => Ok(self.events.remove(index)),
            None => Err(CalendarError::UnknownEvent(id)),
        }
    }

    pub fn get(&self, id: u32) -> Option<&Event> {
        self.events.iter().find(|e| e.id == id)
    }

    /// Events in start order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Move an event to a new start, keeping its length.
    pub fn reschedule(&mut self, id: u32, new_start: i64) -> Result<(), CalendarError> {
        let event = self.remove(id)?;
        let span = Interval {
            start: new_start,
            end: new_start + event.span.len(),
        };
        if let Some(clash) = self.events.iter().find(|e| e.span.overlaps(&span)) {
            let with = clash.id;
            self.events.push(event);
            self.events.sort_by_key(|e| (e.span.start, e.id));
            return Err(CalendarError::Conflict { with });
        }
        self.events.push(Event { span, ..event });
        self.events.sort_by_key(|e| (e.span.start, e.id));
        Ok(())
    }

    pub fn busy(&self) -> Vec<Interval> {
        self.events.iter().map(|e| e.span).collect()
    }

    /// Events that overlap an interval.
    pub fn during(&self, window: Interval) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| e.span.overlaps(&window))
            .collect()
    }

    /// Free gaps inside a window.
    pub fn free(&self, window: Interval) -> Vec<Interval> {
        complement(window, &self.busy())
    }

    /// Where a new event of `duration` minutes fits, earliest first.
    pub fn find_slot(&self, window: Interval, duration: i64) -> Option<Interval> {
        first_fit(window, &self.busy(), duration)
    }

    /// Minutes booked inside a window.
    pub fn booked(&self, window: Interval) -> i64 {
        total(&clip(&self.busy(), window))
    }

    /// Share of the window that is booked, from 0.0 to 1.0.
    pub fn load(&self, window: Interval) -> f64 {
        if window.is_empty() {
            return 0.0;
        }
        self.booked(window) as f64 / window.len() as f64
    }

    /// One text line per event.
    pub fn agenda(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| format!("{} {}", e.span, e.title))
            .collect()
    }
}

// -- working hours and repeats ----------------------------------------------------------------

/// The working part of each day, as minutes since midnight.
#[derive(Debug, Clone, Copy)]
pub struct WorkDay {
    pub open: i64,
    pub close: i64,
}

impl WorkDay {
    /// Working intervals for days `first_day` up to and including `last_day`.
    /// Days 5 and 6 of each week (counting from day 0) are the weekend.
    pub fn intervals(&self, first_day: i64, last_day: i64) -> Vec<Interval> {
        let mut out = Vec::new();
        for day in first_day..=last_day {
            if day.rem_euclid(7) >= 5 {
                continue;
            }
            let base = day * MINUTES_PER_DAY;
            out.push(Interval {
                start: base + self.open,
                end: base + self.close,
            });
        }
        out
    }

    /// Working minutes inside the window.
    pub fn minutes_in(&self, window: Interval) -> i64 {
        let first = window.start.div_euclid(MINUTES_PER_DAY);
        let last = (window.end - 1).div_euclid(MINUTES_PER_DAY);
        total(&clip(&self.intervals(first, last), window))
    }
}

/// `count` copies of an event, one every `every_days` days.
pub fn daily(first: Interval, count: usize, every_days: i64) -> Vec<Interval> {
    (0..count as i64)
        .map(|i| first.shift(i * every_days * MINUTES_PER_DAY))
        .collect()
}

/// `count` copies of an event, one every `every_weeks` weeks.
pub fn weekly(first: Interval, count: usize, every_weeks: i64) -> Vec<Interval> {
    (0..count as i64)
        .map(|i| first.shift(i * MINUTES_PER_WEEK))
        .collect()
}

/// Book every copy of a repeating event. Stops at the first conflict and
/// leaves the calendar as it was.
pub fn book_series(
    cal: &mut Calendar,
    title: &str,
    series: &[Interval],
) -> Result<Vec<u32>, CalendarError> {
    let backup = cal.clone();
    let mut ids = Vec::new();
    for span in series {
        match cal.add(title, *span) {
            Ok(id) => ids.push(id),
            Err(error) => {
                *cal = backup;
                return Err(error);
            }
        }
    }
    Ok(ids)
}

impl Calendar {
    /// Booked minutes per day, as (day, minutes) pairs for days that have events.
    pub fn minutes_by_day(&self) -> Vec<(i64, i64)> {
        let mut days: Vec<(i64, i64)> = Vec::new();
        for event in &self.events {
            for piece in split_days(event.span) {
                let day = piece.start.div_euclid(MINUTES_PER_DAY);
                match days.iter_mut().find(|(d, _)| *d == day) {
                    Some(entry) => entry.1 += piece.len(),
                    None => days.push((day, piece.len())),
                }
            }
        }
        days.sort();
        days
    }

    /// The day with the most booked minutes, the earliest day on a tie.
    pub fn busiest_day(&self) -> Option<i64> {
        let mut best: Option<(i64, i64)> = None;
        for (day, minutes) in self.minutes_by_day() {
            if best.map_or(true, |(_, m)| minutes > m) {
                best = Some((day, minutes));
            }
        }
        best.map(|(day, _)| day)
    }
}

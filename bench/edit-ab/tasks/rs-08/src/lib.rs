//! Reading and writing durations such as `1h 2m 3s`.

pub const MINUTE: u64 = 60;
pub const HOUR: u64 = 60 * MINUTE;
pub const DAY: u64 = 24 * HOUR;

/// Writes seconds as `1d 2h 3m 4s`, leaving out the parts that are zero.
/// Zero seconds is written as `0s`.
pub fn format_duration(secs: u64) -> String {
    let days = secs / DAY;
    let hours = secs % DAY / HOUR;
    let minutes = secs / MINUTE;
    let seconds = secs % MINUTE;
    let mut parts = Vec::new();
    for (value, unit) in [(days, "d"), (hours, "h"), (minutes, "m"), (seconds, "s")] {
        if value > 0 {
            parts.push(format!("{}{}", value, unit));
        }
    }
    parts.join(" ")
}

/// The seconds in one unit letter.
fn unit_seconds(unit: char) -> Option<u64> {
    match unit {
        'd' => Some(DAY),
        'h' => Some(MINUTE),
        'm' => Some(HOUR),
        's' => Some(1),
        _ => None,
    }
}

/// Reads text such as `1h30m`, `90s` or `2d 4h`. Spaces are allowed between
/// the parts. Returns None for anything else, including a number without a
/// unit.
pub fn parse_duration(text: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut number: Option<u64> = None;
    let mut seen_any = false;
    for c in text.chars() {
        if let Some(d) = c.to_digit(10) {
            let n = number.unwrap_or(0).checked_mul(10)?.checked_add(d as u64)?;
            number = Some(n);
        } else if c.is_whitespace() {
            continue;
        } else {
            let n = number.take()?;
            let secs = unit_seconds(c)?;
            total = total.checked_add(n.checked_mul(secs)?)?;
            seen_any = true;
        }
    }
    if number.is_some() || !seen_any {
        return None;
    }
    Some(total)
}

/// Sums a list of duration texts. None when any of them is not valid.
pub fn total(items: &[&str]) -> Option<u64> {
    let mut sum: u64 = 0;
    for item in items {
        sum = sum.checked_add(parse_duration(item)?)?;
    }
    Some(sum)
}

/// A short form that keeps only the two biggest parts, `1h 2m` for 1h 2m 3s.
pub fn format_short(secs: u64) -> String {
    let full = format_duration(secs);
    full.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

/// Writes seconds as a clock, `h:mm:ss` or `m:ss` below an hour.
pub fn format_clock(secs: u64) -> String {
    let hours = secs / HOUR;
    let minutes = secs % HOUR / MINUTE;
    let seconds = secs % MINUTE;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

/// The longest of a list of durations in seconds.
pub fn longest(items: &[u64]) -> Option<u64> {
    items.iter().copied().max()
}

/// Rounds seconds up to a whole number of minutes.
pub fn round_up_minutes(secs: u64) -> u64 {
    (secs + MINUTE - 1) / MINUTE
}

/// How many whole days, hours, minutes and seconds make up a duration.
pub fn split_parts(secs: u64) -> (u64, u64, u64, u64) {
    (secs / DAY, secs % DAY / HOUR, secs % HOUR / MINUTE, secs % MINUTE)
}

/// The difference between two instants, never negative.
pub fn elapsed(start: u64, end: u64) -> u64 {
    end.saturating_sub(start)
}

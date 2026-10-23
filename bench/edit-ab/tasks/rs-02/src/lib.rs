//! Roman numerals for 1 to 3999.

const TABLE: [(u32, &str); 13] = [
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (1, "I"),
];

pub fn to_roman(mut n: u32) -> Option<String> {
    if n == 0 || n > 3999 {
        return None;
    }
    let mut out = String::new();
    for &(value, symbol) in TABLE.iter() {
        while n >= value {
            out.push_str(symbol);
            n -= value;
        }
    }
    Some(out)
}

fn digit(c: char) -> Option<i64> {
    match c.to_ascii_uppercase() {
        'I' => Some(1),
        'V' => Some(5),
        'X' => Some(10),
        'L' => Some(50),
        'C' => Some(100),
        'D' => Some(500),
        'M' => Some(1000),
        _ => None,
    }
}

/// Reads a numeral. Returns None for an empty string, a bad letter or a
/// numeral that is not written in the standard form.
pub fn from_roman(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut vals = Vec::new();
    for c in s.chars() {
        vals.push(digit(c)?);
    }
    let mut total: i64 = 0;
    for i in 0..vals.len() {
        if i + 1 < vals.len() && vals[i] > vals[i + 1] {
            total -= vals[i];
        } else {
            total += vals[i];
        }
    }
    if total < 1 || total > 3999 {
        return None;
    }
    let n = total as u32;
    // Only the standard form counts: IIII and VX are not numerals.
    if to_roman(n)?.eq_ignore_ascii_case(s) {
        Some(n)
    } else {
        None
    }
}

pub fn is_roman(s: &str) -> bool {
    from_roman(s).is_some()
}

/// Adds two numerals and writes the sum as a numeral.
pub fn add(a: &str, b: &str) -> Option<String> {
    to_roman(from_roman(a)? + from_roman(b)?)
}

/// Orders two numerals by value. Returns None when either is not valid.
pub fn compare(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    Some(from_roman(a)?.cmp(&from_roman(b)?))
}

/// The largest numeral in a list, by value.
pub fn largest<'a>(items: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(&str, u32)> = None;
    for &item in items {
        let value = from_roman(item)?;
        match best {
            Some((_, b)) if b >= value => {}
            _ => best = Some((item, value)),
        }
    }
    best.map(|(s, _)| s)
}

/// Writes every number in a range as numerals, skipping the ones that cannot
/// be written.
pub fn table(from: u32, to: u32) -> Vec<(u32, String)> {
    (from..=to)
        .filter_map(|n| to_roman(n).map(|r| (n, r)))
        .collect()
}

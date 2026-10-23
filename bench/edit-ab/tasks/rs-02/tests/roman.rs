use roman::{add, compare, from_roman, is_roman, largest, table, to_roman};
use std::cmp::Ordering;

#[test]
fn simple_numbers() {
    assert_eq!(to_roman(3).as_deref(), Some("III"));
    assert_eq!(to_roman(58).as_deref(), Some("LVIII"));
    assert_eq!(to_roman(1994).as_deref(), Some("MCMXCIV"));
}

#[test]
fn four_and_nine_use_subtraction() {
    assert_eq!(to_roman(4).as_deref(), Some("IV"));
    assert_eq!(to_roman(14).as_deref(), Some("XIV"));
    assert_eq!(to_roman(24).as_deref(), Some("XXIV"));
}

#[test]
fn out_of_range() {
    assert_eq!(to_roman(0), None);
    assert_eq!(to_roman(4000), None);
}

#[test]
fn reading_numerals() {
    assert_eq!(from_roman("XIV"), Some(14));
    assert_eq!(from_roman("MCMXCIV"), Some(1994));
    assert_eq!(from_roman("mmxxiv"), Some(2024));
}

#[test]
fn bad_numerals() {
    assert_eq!(from_roman(""), None);
    assert_eq!(from_roman("IIII"), None);
    assert_eq!(from_roman("VX"), None);
    assert_eq!(from_roman("ABC"), None);
    assert!(!is_roman("IC"));
}

#[test]
fn round_trip() {
    for n in 1..=3999 {
        let r = to_roman(n).unwrap();
        assert_eq!(from_roman(&r), Some(n), "{} -> {}", n, r);
    }
}

#[test]
fn arithmetic_helpers() {
    assert_eq!(add("IX", "XI").as_deref(), Some("XX"));
    assert_eq!(compare("IV", "VI"), Some(Ordering::Less));
    assert_eq!(largest(&["IV", "XL", "IX"]), Some("XL"));
    assert_eq!(table(3, 5), vec![(3, "III".to_string()), (4, "IV".to_string()), (5, "V".to_string())]);
}

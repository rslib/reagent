use duration::{format_clock, format_duration, format_short, parse_duration, round_up_minutes, total};

#[test]
fn formats_parts() {
    assert_eq!(format_duration(59), "59s");
    assert_eq!(format_duration(61), "1m 1s");
    assert_eq!(format_duration(3_600), "1h");
    assert_eq!(format_duration(3_723), "1h 2m 3s");
    assert_eq!(format_duration(90_061), "1d 1h 1m 1s");
}

#[test]
fn formats_minutes_below_an_hour_boundary() {
    assert_eq!(format_duration(7_320), "2h 2m");
    assert_eq!(format_duration(5_400), "1h 30m");
}

#[test]
fn zero_is_written() {
    assert_eq!(format_duration(0), "0s");
    assert_eq!(format_short(0), "0s");
}

#[test]
fn parses_units() {
    assert_eq!(parse_duration("90s"), Some(90));
    assert_eq!(parse_duration("1h30m"), Some(5_400));
    assert_eq!(parse_duration("2d 4h"), Some(2 * 86_400 + 4 * 3_600));
    assert_eq!(parse_duration("5m"), Some(300));
}

#[test]
fn rejects_bad_text() {
    assert_eq!(parse_duration(""), None);
    assert_eq!(parse_duration("12"), None);
    assert_eq!(parse_duration("1x"), None);
    assert_eq!(parse_duration("h"), None);
}

#[test]
fn round_trip() {
    for secs in [1, 59, 60, 61, 3_599, 3_600, 3_661, 86_399, 86_400, 200_000] {
        assert_eq!(parse_duration(&format_duration(secs)), Some(secs));
    }
}

#[test]
fn helpers() {
    assert_eq!(total(&["1m", "30s"]), Some(90));
    assert_eq!(format_short(90_061), "1d 1h");
    assert_eq!(format_clock(3_723), "1:02:03");
    assert_eq!(format_clock(65), "1:05");
    assert_eq!(round_up_minutes(61), 2);
}

use intervals::*;

fn iv(start: i64, end: i64) -> Interval {
    Interval::new(start, end).unwrap()
}

#[test]
fn touching_intervals_do_not_overlap() {
    assert!(!iv(0, 10).overlaps(&iv(10, 20)));
    assert!(!iv(10, 20).overlaps(&iv(0, 10)));
    assert!(iv(0, 10).touches(&iv(10, 20)));
    assert!(iv(0, 10).overlaps(&iv(9, 20)));
    assert!(iv(0, 10).overlaps(&iv(2, 3)));
    assert!(!iv(0, 10).touches(&iv(11, 20)));
}

#[test]
fn constructing_and_combining() {
    assert_eq!(Interval::new(5, 5), Err(CalendarError::Empty { start: 5, end: 5 }));
    assert_eq!(iv(0, 10).intersect(&iv(5, 15)), Some(iv(5, 10)));
    assert_eq!(iv(0, 10).intersect(&iv(10, 15)), None);
    assert_eq!(iv(0, 10).union(&iv(10, 15)), Some(iv(0, 15)));
    assert_eq!(iv(0, 10).union(&iv(11, 15)), None);
    assert_eq!(iv(0, 10).shift(5), iv(5, 15));
    assert_eq!(iv(10, 20).pad(3), iv(7, 23));
    assert!(iv(0, 10).contains(0));
    assert!(!iv(0, 10).contains(10));
    assert!(iv(0, 10).contains_interval(&iv(2, 10)));
}

#[test]
fn normalize_sorts_and_joins() {
    let list = [iv(5, 8), iv(1, 3), iv(2, 4), iv(8, 9)];
    assert_eq!(normalize(&list), vec![iv(1, 4), iv(5, 9)]);
    assert_eq!(total(&list), 3 + 4);
}

#[test]
fn normalize_keeps_the_far_end_of_a_big_interval() {
    assert_eq!(normalize(&[iv(1, 10), iv(2, 3)]), vec![iv(1, 10)]);
    assert_eq!(normalize(&[iv(1, 10), iv(2, 12), iv(3, 4)]), vec![iv(1, 12)]);
    assert_eq!(total(&[iv(0, 100), iv(10, 20), iv(30, 40)]), 100);
}

#[test]
fn normalize_drops_empty_intervals() {
    let empty = Interval { start: 4, end: 4 };
    assert_eq!(normalize(&[empty, iv(1, 2)]), vec![iv(1, 2)]);
}

#[test]
fn subtract_cuts_holes() {
    let base = [iv(0, 10)];
    assert_eq!(subtract(&base, &[iv(2, 4), iv(6, 7)]), vec![iv(0, 2), iv(4, 6), iv(7, 10)]);
    assert_eq!(subtract(&base, &[iv(0, 10)]), vec![]);
    assert_eq!(subtract(&base, &[iv(20, 30)]), vec![iv(0, 10)]);
    assert_eq!(subtract(&base, &[iv(5, 50)]), vec![iv(0, 5)]);
}

#[test]
fn subtract_makes_no_empty_pieces() {
    let base = [iv(0, 10), iv(20, 30)];
    let out = subtract(&base, &[iv(0, 3), iv(20, 25)]);
    assert_eq!(out, vec![iv(3, 10), iv(25, 30)]);
    assert!(out.iter().all(|i| !i.is_empty()));
}

#[test]
fn intersect_all_walks_both_lists() {
    let a = [iv(0, 5), iv(8, 12), iv(20, 30)];
    let b = [iv(3, 9), iv(11, 25)];
    assert_eq!(intersect_all(&a, &b), vec![iv(3, 5), iv(8, 9), iv(11, 12), iv(20, 25)]);
}

#[test]
fn first_fit_takes_the_earliest_gap() {
    let window = iv(0, 100);
    let busy = [iv(0, 10), iv(15, 40), iv(50, 60)];
    assert_eq!(first_fit(window, &busy, 5), Some(iv(10, 15)));
    assert_eq!(first_fit(window, &busy, 6), Some(iv(40, 46)));
    assert_eq!(first_fit(window, &busy, 41), None);
    assert_eq!(first_fit(window, &busy, 0), None);
}

#[test]
fn last_fit_takes_the_end_of_the_latest_gap() {
    let window = iv(0, 100);
    let busy = [iv(0, 10), iv(15, 40), iv(50, 60)];
    assert_eq!(last_fit(window, &busy, 5), Some(iv(95, 100)));
    assert_eq!(last_fit(window, &busy, 30), Some(iv(70, 100)));
    assert_eq!(last_fit(window, &busy, 40), Some(iv(60, 100)));
    assert_eq!(last_fit(window, &busy, 41), None);
    let tight = [iv(0, 30), iv(60, 100)];
    assert_eq!(last_fit(window, &tight, 10), Some(iv(50, 60)));
}

#[test]
fn overlap_depth() {
    assert_eq!(max_overlap(&[iv(0, 10), iv(10, 20)]), 1);
    assert_eq!(max_overlap(&[iv(0, 10), iv(5, 15), iv(8, 9)]), 3);
    assert_eq!(max_overlap(&[]), 0);
}

#[test]
fn times_as_text() {
    assert_eq!(format_time(0), "d0 00:00");
    assert_eq!(format_time(MINUTES_PER_DAY + 9 * 60 + 30), "d1 09:30");
    assert_eq!(format_time(-1), "d-1 23:59");
    assert_eq!(parse_time("d1 09:30"), Ok(MINUTES_PER_DAY + 570));
    assert_eq!(parse_time("d-1 23:59"), Ok(-1));
    for t in [-3000, -1, 0, 59, 1440, 100_000] {
        assert_eq!(parse_time(&format_time(t)), Ok(t));
    }
    assert!(parse_time("1 09:30").is_err());
    assert!(parse_time("d1 24:00").is_err());
    assert!(parse_time("d1 09:61").is_err());
    assert_eq!(iv(60, 120).to_string(), "d0 01:00-d0 02:00");
}

#[test]
fn calendar_allows_back_to_back_events() {
    let mut cal = Calendar::new();
    let a = cal.add("standup", iv(540, 570)).unwrap();
    let b = cal.add("review", iv(570, 630)).unwrap();
    assert_eq!((a, b), (1, 2));
    assert_eq!(cal.add("clash", iv(560, 580)), Err(CalendarError::Conflict { with: a }));
    assert_eq!(cal.len(), 2);
}

#[test]
fn calendar_keeps_events_in_start_order() {
    let mut cal = Calendar::new();
    cal.add("late", iv(900, 960)).unwrap();
    cal.add("early", iv(60, 120)).unwrap();
    let titles: Vec<&str> = cal.events().iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["early", "late"]);
    assert_eq!(cal.agenda()[0], "d0 01:00-d0 02:00 early");
}

#[test]
fn removing_and_rescheduling() {
    let mut cal = Calendar::new();
    let a = cal.add("a", iv(0, 60)).unwrap();
    let b = cal.add("b", iv(60, 120)).unwrap();
    assert_eq!(cal.reschedule(a, 90), Err(CalendarError::Conflict { with: b }));
    assert_eq!(cal.get(a).unwrap().span, iv(0, 60));
    cal.reschedule(a, 120).unwrap();
    assert_eq!(cal.get(a).unwrap().span, iv(120, 180));
    assert_eq!(cal.remove(b).unwrap().title, "b");
    assert_eq!(cal.remove(b), Err(CalendarError::UnknownEvent(b)));
}

#[test]
fn free_time_and_load() {
    let mut cal = Calendar::new();
    cal.add("a", iv(60, 120)).unwrap();
    cal.add("b", iv(180, 240)).unwrap();
    let day = iv(0, 300);
    assert_eq!(cal.free(day), vec![iv(0, 60), iv(120, 180), iv(240, 300)]);
    assert_eq!(cal.booked(day), 120);
    assert!((cal.load(day) - 0.4).abs() < 1e-9);
    assert_eq!(cal.booked(iv(90, 200)), 30 + 20);
    assert_eq!(cal.find_slot(day, 60), Some(iv(0, 60)));
    assert_eq!(cal.find_slot(day, 61), None);
    assert_eq!(cal.during(iv(100, 190)).len(), 2);
    assert_eq!(cal.during(iv(120, 180)).len(), 0);
}

#[test]
fn working_hours_skip_weekends() {
    let work = WorkDay { open: 9 * 60, close: 17 * 60 };
    let week = work.intervals(0, 6);
    assert_eq!(week.len(), 5);
    assert_eq!(week[4], iv(4 * MINUTES_PER_DAY + 540, 4 * MINUTES_PER_DAY + 1020));
    assert_eq!(work.minutes_in(iv(0, MINUTES_PER_WEEK)), 5 * 480);
    assert_eq!(work.minutes_in(iv(12 * 60, 14 * 60)), 120);
    assert_eq!(work.minutes_in(iv(5 * MINUTES_PER_DAY, 7 * MINUTES_PER_DAY)), 0);
}

#[test]
fn repeats() {
    let first = iv(600, 660);
    let d = daily(first, 3, 2);
    assert_eq!(d[2], iv(600 + 4 * MINUTES_PER_DAY, 660 + 4 * MINUTES_PER_DAY));
    let w = weekly(first, 3, 2);
    assert_eq!(w[0], first);
    assert_eq!(w[1], first.shift(2 * MINUTES_PER_WEEK));
    assert_eq!(w[2], first.shift(4 * MINUTES_PER_WEEK));
    let every_week = weekly(first, 2, 1);
    assert_eq!(every_week[1], first.shift(MINUTES_PER_WEEK));
}

#[test]
fn a_series_books_all_or_nothing() {
    let mut cal = Calendar::new();
    cal.add("blocker", iv(2 * MINUTES_PER_WEEK + 620, 2 * MINUTES_PER_WEEK + 640)).unwrap();
    let series = weekly(iv(600, 660), 3, 1);
    let result = book_series(&mut cal, "sync", &series);
    assert!(matches!(result, Err(CalendarError::Conflict { .. })));
    assert_eq!(cal.len(), 1);
    let ok = book_series(&mut cal, "sync", &series[..2]).unwrap();
    assert_eq!(ok.len(), 2);
    assert_eq!(cal.len(), 3);
}

#[test]
fn more_helpers() {
    let list = [iv(0, 5), iv(7, 10), iv(20, 25)];
    assert_eq!(merge_within(&list, 2), vec![iv(0, 10), iv(20, 25)]);
    assert_eq!(merge_within(&list, 1), vec![iv(0, 5), iv(7, 10), iv(20, 25)]);
    assert_eq!(
        split_days(iv(1400, 3000)),
        vec![iv(1400, 1440), iv(1440, 2880), iv(2880, 3000)]
    );
    assert_eq!(longest_gap(iv(0, 30), &list), Some(iv(10, 20)));
    assert_eq!(longest_gap(iv(0, 5), &list), None);
    assert_eq!(round_up(61, 15), 75);
    assert_eq!(round_up(60, 15), 60);
    assert_eq!(round_up(-7, 15), 0);
}

#[test]
fn per_day_totals() {
    let mut cal = Calendar::new();
    cal.add("night", iv(1400, 1500)).unwrap();
    cal.add("day two", iv(1600, 1700)).unwrap();
    assert_eq!(cal.minutes_by_day(), vec![(0, 40), (1, 160)]);
    assert_eq!(cal.busiest_day(), Some(1));
    assert_eq!(Calendar::new().busiest_day(), None);
}

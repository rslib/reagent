use config::{parse, ParseError};

#[test]
fn simple_pairs() {
    let c = parse("name = reagent\nthreads=4\n").unwrap();
    assert_eq!(c.get("name"), Some("reagent"));
    assert_eq!(c.get_int("threads"), Some(4));
    assert_eq!(c.len(), 2);
}

#[test]
fn comments_and_blank_lines() {
    let c = parse("# a comment\n\n   \nkey = v\n  # indented comment\n").unwrap();
    assert_eq!(c.keys(), vec!["key"]);
}

#[test]
fn value_may_contain_equals() {
    let c = parse("url = http://x/?a=1&b=2\n").unwrap();
    assert_eq!(c.get("url"), Some("http://x/?a=1&b=2"));
}

#[test]
fn sections() {
    let c = parse("top = 1\n[db]\nhost = h\nport = 5432\n[ui]\ncolor = on\n").unwrap();
    assert_eq!(c.get("top"), Some("1"));
    assert_eq!(c.get("db.host"), Some("h"));
    assert_eq!(c.section("db").len(), 2);
    assert_eq!(c.get_bool("ui.color"), Some(true));
}

#[test]
fn booleans() {
    let c = parse("a = Yes\nb = OFF\nc = on\nd = maybe\n").unwrap();
    assert_eq!(c.get_bool("a"), Some(true));
    assert_eq!(c.get_bool("b"), Some(false));
    assert_eq!(c.get_bool("c"), Some(true));
    assert_eq!(c.get_bool("d"), None);
}

#[test]
fn errors_carry_line_numbers() {
    assert_eq!(parse("a = 1\nnonsense\n"), Err(ParseError::BadLine(2)));
    assert_eq!(parse("[oops\n"), Err(ParseError::BadSection(1)));
    assert_eq!(parse("\n = 3\n"), Err(ParseError::EmptyKey(2)));
}

#[test]
fn merge_overrides() {
    let mut a = parse("x = 1\ny = 2\n").unwrap();
    a.merge(parse("y = 3\nz = 4\n").unwrap());
    assert_eq!(a.get("y"), Some("3"));
    assert_eq!(a.get_or("q", "none"), "none");
}

use minijson::*;

fn p(text: &str) -> Value {
    parse(text).unwrap()
}

fn fails(text: &str) -> ParseError {
    parse(text).unwrap_err()
}

#[test]
fn scalars() {
    assert_eq!(p("null"), Value::Null);
    assert_eq!(p(" true "), Value::Bool(true));
    assert_eq!(p("\n\tfalse\r\n"), Value::Bool(false));
    assert_eq!(p("42"), Value::Number(42.0));
    assert_eq!(p("-0.5"), Value::Number(-0.5));
    assert_eq!(p("1e3"), Value::Number(1000.0));
    assert_eq!(p("2.5E-2"), Value::Number(0.025));
    assert_eq!(p("\"hi\""), Value::String("hi".to_string()));
}

#[test]
fn false_is_false() {
    assert_eq!(p("[true, false, null]"), Value::Array(vec![
        Value::Bool(true),
        Value::Bool(false),
        Value::Null,
    ]));
    assert_eq!(p("{\"a\": false}").get("a").and_then(Value::as_bool), Some(false));
}

#[test]
fn numbers_follow_the_grammar() {
    for bad in ["01", "-", "1.", "1.e5", ".5", "+1", "1e", "1e+", "--1", "0x10"] {
        assert!(parse(bad).is_err(), "{bad} should not parse");
    }
    assert_eq!(p("0"), Value::Number(0.0));
    assert_eq!(p("-0"), Value::Number(-0.0));
    assert_eq!(p("0.0"), Value::Number(0.0));
    assert_eq!(p("10.25e1"), Value::Number(102.5));
    assert!(fails("1.").message.contains("digit"));
}

#[test]
fn a_dangling_point_inside_a_container() {
    assert!(parse("[1.]").is_err());
    assert!(parse("{\"a\": 2.}").is_err());
}

#[test]
fn string_escapes() {
    assert_eq!(p(r#""a\nb\t\"q\" \\ \/ \b\f\r""#), Value::String("a\nb\t\"q\" \\ / \u{8}\u{c}\r".to_string()));
    assert_eq!(p(r#""\u0041\u00e9""#), Value::String("Aé".to_string()));
    assert_eq!(p("\"héllo wörld ✓\""), Value::String("héllo wörld ✓".to_string()));
}

#[test]
fn surrogate_pairs() {
    assert_eq!(p(r#""\ud83d\ude00""#), Value::String("😀".to_string()));
    assert_eq!(p(r#""\uD834\uDD1E""#), Value::String("\u{1D11E}".to_string()));
    assert_eq!(p(r#""x\ud800\udc00y""#), Value::String("x\u{10000}y".to_string()));
    assert!(parse(r#""\ud83d""#).is_err());
    assert!(parse(r#""\ud83dx""#).is_err());
    assert!(parse(r#""\ude00""#).is_err());
    assert!(parse(r#""\ud83d\u0041""#).is_err());
}

#[test]
fn bad_strings() {
    for bad in ["\"abc", "\"a\nb\"", r#""\x""#, r#""\u12""#, r#""\u12g4""#, r#""\"#, "\"a\u{1}b\""] {
        assert!(parse(bad).is_err(), "{bad:?} should not parse");
    }
    assert!(parse("\"\\u00é\"").is_err());
}

#[test]
fn arrays_and_objects() {
    let v = p(r#" { "b": [1, 2, {"c": null}], "a": "x", "e": {}, "f": [] } "#);
    assert_eq!(v.get("a").and_then(Value::as_str), Some("x"));
    assert_eq!(v.get("b").and_then(|b| b.at(1)).and_then(Value::as_i64), Some(2));
    assert_eq!(v.get("e"), Some(&Value::Object(Default::default())));
    assert_eq!(v.get("f"), Some(&Value::Array(vec![])));
    assert_eq!(v.get("nope"), None);
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["a", "b", "e", "f"]);
}

#[test]
fn structure_errors() {
    for bad in ["[1,]", "[1 2]", "{\"a\":1,}", "{\"a\" 1}", "{a: 1}", "[", "{", "{\"a\":}", "", "  "] {
        assert!(parse(bad).is_err(), "{bad:?} should not parse");
    }
    assert!(parse("1 2").is_err());
    assert!(parse("[] x").is_err());
}

#[test]
fn errors_say_where() {
    let e = fails("{\n  \"a\": [1, 2,\n   oops]\n}");
    assert_eq!((e.line, e.column), (3, 4));
    assert_eq!(e.to_string(), "unexpected character at line 3 column 4");
    let e = fails("héé x");
    assert_eq!((e.line, e.column, e.offset), (1, 1, 0));
    let e = fails("[\"é\", ?]");
    assert_eq!((e.line, e.column), (1, 7));
}

#[test]
fn nesting_limit() {
    let deep = format!("{}{}", "[".repeat(10), "]".repeat(10));
    assert!(parse_with(&deep, Limits { max_depth: 10 }).is_ok());
    let err = parse_with(&deep, Limits { max_depth: 9 }).unwrap_err();
    assert!(err.message.contains("deep"));
    assert!(parse(&format!("{}{}", "[".repeat(100), "]".repeat(100))).is_err());
}

#[test]
fn compact_output() {
    let v = p(r#"{"b": [1, 2.5, "x\ny"], "a": {"k": null, "t": true}}"#);
    assert_eq!(v.to_json(), r#"{"a":{"k":null,"t":true},"b":[1,2.5,"x\ny"]}"#);
    assert_eq!(v.to_string(), v.to_json());
    assert_eq!(p("[]").to_json(), "[]");
    assert_eq!(p("{}").to_json(), "{}");
    assert_eq!(Value::Number(f64::NAN).to_json(), "null");
    assert_eq!(Value::Number(1e21).to_json(), "1000000000000000000000");
    assert_eq!(Value::Number(-3.0).to_json(), "-3");
    assert_eq!(Value::String("\u{1}\"\\\u{8}é".to_string()).to_json(), "\"\\u0001\\\"\\\\\\bé\"");
}

#[test]
fn output_parses_back_to_the_same_value() {
    let text = r#"{"list": [0, -1, 2.5, 1e-7, 123456789012, "é\t"], "n": {"deep": {"x": [[], {}]}}, "s": ""}"#;
    let v = p(text);
    assert_eq!(p(&v.to_json()), v);
    assert_eq!(p(&v.to_pretty(2)), v);
    assert_eq!(p(&v.to_pretty(4)), v);
}

#[test]
fn pretty_output_indents_every_level() {
    let v = p(r#"{"a": {"b": [1, {"c": 2}]}, "d": []}"#);
    let expected = "{\n  \"a\": {\n    \"b\": [\n      1,\n      {\n        \"c\": 2\n      }\n    ]\n  },\n  \"d\": []\n}";
    assert_eq!(v.to_pretty(2), expected);
    assert_eq!(p("[1]").to_pretty(3), "[\n   1\n]");
    assert_eq!(p("3").to_pretty(2), "3");
}

#[test]
fn nested_objects_inside_objects() {
    let v = p(r#"{"a": {"b": {"c": 1}}}"#);
    assert_eq!(v.to_pretty(1), "{\n \"a\": {\n  \"b\": {\n   \"c\": 1\n  }\n }\n}");
}

#[test]
fn reading_numbers() {
    assert_eq!(p("3").as_i64(), Some(3));
    assert_eq!(p("3.5").as_i64(), None);
    assert_eq!(p("-7").as_i64(), Some(-7));
    assert_eq!(p("1e300").as_i64(), None);
    assert_eq!(p("\"3\"").as_f64(), None);
    assert_eq!(p("[]").type_name(), "array");
    assert!(p("null").is_null());
}

#[test]
fn depth_and_count() {
    assert_eq!(p("1").depth(), 0);
    assert_eq!(p("[]").depth(), 0);
    assert_eq!(p("[1]").depth(), 1);
    assert_eq!(p(r#"{"a": [1, {"b": 2}]}"#).depth(), 3);
    assert_eq!(p(r#"{"a": [1, {"b": 2}]}"#).count(), 5);
}

#[test]
fn building_values() {
    let v = object(vec![("n", Value::from(1i64)), ("s", Value::from("x")), ("l", Value::from(vec![1i64, 2]))]);
    assert_eq!(v.to_json(), r#"{"l":[1,2],"n":1,"s":"x"}"#);
    assert_eq!(Value::from(true), Value::Bool(true));
    assert_eq!(Value::from(0.5), Value::Number(0.5));
}

#[test]
fn paths() {
    let v = p(r#"{"a": {"b": [10, {"c": "deep"}]}, "x.y": 1}"#);
    assert_eq!(v.get_path("a.b[1].c").and_then(Value::as_str), Some("deep"));
    assert_eq!(v.get_path("a.b[0]").and_then(Value::as_i64), Some(10));
    assert_eq!(v.get_path("a.b[2]"), None);
    assert_eq!(v.get_path("a.zzz"), None);
    assert_eq!(v.get_path("a.b[x]"), None);
    assert_eq!(v.get_path("a.b[1"), None);
    assert_eq!(
        split_path("a[0].b"),
        Some(vec![Step::Key("a".into()), Step::Index(0), Step::Key("b".into())])
    );
}

#[test]
fn pointers() {
    let v = p(r#"{"a": [{"b": 1}, 2], "x/y": 3, "m~n": 4, "": 5, "~1": 6}"#);
    assert_eq!(v.pointer(""), Some(&v));
    assert_eq!(v.pointer("/a/0/b").and_then(Value::as_i64), Some(1));
    assert_eq!(v.pointer("/a/1").and_then(Value::as_i64), Some(2));
    assert_eq!(v.pointer("/x~1y").and_then(Value::as_i64), Some(3));
    assert_eq!(v.pointer("/m~0n").and_then(Value::as_i64), Some(4));
    assert_eq!(v.pointer("/").and_then(Value::as_i64), Some(5));
    assert_eq!(v.pointer("/a/2"), None);
    assert_eq!(v.pointer("/a/01"), None);
    assert_eq!(v.pointer("a"), None);
}

#[test]
fn pointer_escapes_are_undone_in_the_right_order() {
    let v = p(r#"{"~1": 1, "~0": 2, "/": 3, "~": 4}"#);
    assert_eq!(v.pointer("/~01").and_then(Value::as_i64), Some(1));
    assert_eq!(v.pointer("/~00").and_then(Value::as_i64), Some(2));
    assert_eq!(v.pointer("/~1").and_then(Value::as_i64), Some(3));
    assert_eq!(v.pointer("/~0").and_then(Value::as_i64), Some(4));
}

#[test]
fn leaves_report_pointers() {
    let v = p(r#"{"a/b": [1, {"c": true}], "m": {}, "z": null}"#);
    let leaves: Vec<(String, String)> = v.leaves().iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
    assert_eq!(
        leaves,
        vec![
            ("/a~1b/0".to_string(), "1".to_string()),
            ("/a~1b/1/c".to_string(), "true".to_string()),
            ("/m".to_string(), "{}".to_string()),
            ("/z".to_string(), "null".to_string()),
        ]
    );
    for (pointer, leaf) in v.leaves() {
        assert_eq!(v.pointer(&pointer), Some(leaf));
    }
}

#[test]
fn merge_patch_rfc_examples() {
    let target = p(r#"{"a":"b","c":{"d":"e","f":"g"}}"#);
    let patch = p(r#"{"a":"z","c":{"f":null}}"#);
    assert_eq!(merge_patch(&target, &patch).to_json(), r#"{"a":"z","c":{"d":"e"}}"#);
    assert_eq!(merge_patch(&p(r#"{"a":"b"}"#), &p(r#"{"a":["c"]}"#)).to_json(), r#"{"a":["c"]}"#);
    assert_eq!(merge_patch(&p(r#"{"a":"foo"}"#), &p("null")).to_json(), "null");
    assert_eq!(merge_patch(&p(r#"["a","b"]"#), &p(r#"{"a":"c"}"#)).to_json(), r#"{"a":"c"}"#);
    assert_eq!(merge_patch(&p(r#"{"e":null}"#), &p(r#"{"a":1}"#)).to_json(), r#"{"a":1,"e":null}"#);
    assert_eq!(merge_patch(&p("{}"), &p(r#"{"a":{"bb":{"ccc":null}}}"#)).to_json(), r#"{"a":{"bb":{}}}"#);
}

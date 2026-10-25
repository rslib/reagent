use linepatch::diff::*;
use linepatch::patch::{self, Line, PatchError};

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("l{i}\n")).collect()
}

fn replace(text: &str, line: usize, with: &str) -> String {
    text.lines()
        .enumerate()
        .map(|(i, l)| if i + 1 == line { format!("{with}\n") } else { format!("{l}\n") })
        .collect()
}

#[test]
fn lines_ignore_the_final_newline() {
    assert_eq!(lines("a\nb\n"), vec!["a", "b"]);
    assert_eq!(lines("a\nb"), vec!["a", "b"]);
    assert_eq!(lines(""), Vec::<&str>::new());
    assert_eq!(lines("\n"), vec![""]);
    assert_eq!(lines("a\n\nb\n"), vec!["a", "", "b"]);
}

#[test]
fn prefix_and_suffix() {
    assert_eq!(common_prefix(&["a", "b", "c"], &["a", "b", "x"]), 2);
    assert_eq!(common_suffix(&["a", "b", "c"], &["x", "b", "c"]), 2);
    assert_eq!(common_prefix(&[], &["a"]), 0);
}

#[test]
fn a_simple_edit_script() {
    let edits = diff_text("a\nb\nc\n", "a\nx\nc\n");
    assert_eq!(
        edits,
        vec![Edit::Equal("a"), Edit::Delete("b"), Edit::Insert("x"), Edit::Equal("c")]
    );
    assert_eq!(new_side(&edits), vec!["a", "x", "c"]);
    assert_eq!(old_side(&edits), vec!["a", "b", "c"]);
    assert!(!is_identical(&edits));
    assert!(is_identical(&diff_text("a\n", "a\n")));
}

#[test]
fn edit_scripts_rebuild_both_sides() {
    let old = "one\ntwo\nthree\nfour\nfive\nsix\n";
    let new = "one\n2\nthree\nfive\nsix\nseven\neight\n";
    let edits = diff_text(old, new);
    let joined = |v: Vec<String>| v.iter().map(|l| format!("{l}\n")).collect::<String>();
    assert_eq!(joined(old_side(&edits)), old);
    assert_eq!(joined(new_side(&edits)), new);
    assert_eq!(lcs_len(&lines(old), &lines(new)), 4);
}

#[test]
fn counting_inserts_and_deletes() {
    let edits = diff_text("a\nb\nc\nd\n", "a\nc\nx\ny\nz\n");
    assert_eq!(added(&edits), 3);
    assert_eq!(removed(&edits), 2);
    assert_eq!(unchanged(&edits), 2);
    assert_eq!(summary(&edits), "+3 -2 =2");
}

#[test]
fn counting_only_deletes() {
    let edits = diff_text("a\nb\nc\n", "a\n");
    assert_eq!((added(&edits), removed(&edits)), (0, 2));
    let edits = diff_text("a\n", "a\nb\nc\n");
    assert_eq!((added(&edits), removed(&edits)), (2, 0));
}

#[test]
fn similarity_is_a_ratio() {
    assert_eq!(similarity("a\nb\n", "a\nb\n"), 1.0);
    assert_eq!(similarity("a\nb\n", "c\nd\n"), 0.0);
    assert_eq!(similarity("", ""), 1.0);
    assert!((similarity("a\nb\nc\nd\n", "a\nb\nx\nd\n") - 0.75).abs() < 1e-9);
}

#[test]
fn words_inside_a_line() {
    assert_eq!(tokens("a  b c"), vec!["a", "  ", "b", " ", "c"]);
    assert_eq!(mark_words("the quick fox", "the slow fox"), "the [-quick-]{+slow+} fox");
    assert_eq!(longest_change_run(&diff_text("a\nb\nc\nd\n", "a\nx\ny\nd\n")), 4);
    assert_eq!(churn(&[]), 0.0);
    assert_eq!(inserted_lines(&diff_text("a\nc\n", "a\nb\nc\nd\n")), vec![1, 3]);
    assert_eq!(deleted_lines(&diff_text("a\nb\nc\nd\n", "a\nd\n")), vec![1, 2]);
    assert_eq!(side_by_side(&diff_text("a\n", "b\n"), 2), vec!["a  <   ", "   > b "]);
}

#[test]
fn nearby_changes_share_a_hunk() {
    let old = numbered(10);
    let new = replace(&replace(&old, 3, "x"), 8, "y");
    let edits = diff_text(&old, &new);
    let merged = hunks(&edits, 2);
    assert_eq!(merged.len(), 1);
    assert_eq!((merged[0].old_start, merged[0].old_len), (1, 10));
    let split = hunks(&edits, 1);
    assert_eq!(split.len(), 2);
    assert_eq!(split[0].to_string(), "@@ -2,3 +2,3 @@");
    assert_eq!(split[1].to_string(), "@@ -7,3 +7,3 @@");
}

#[test]
fn changes_a_little_further_apart_stay_separate() {
    let old = numbered(12);
    let new = replace(&replace(&old, 3, "x"), 10, "y");
    let edits = diff_text(&old, &new);
    assert_eq!(hunks(&edits, 2).len(), 2);
    assert_eq!(hunks(&edits, 3).len(), 1);
    assert_eq!(hunks(&edits, 0).len(), 2);
}

#[test]
fn hunk_numbers_for_empty_ranges() {
    let edits = diff_text("a\nb\nc\n", "a\nx\nb\nc\n");
    let h = hunks(&edits, 0);
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].to_string(), "@@ -1,0 +2,1 @@");
    let edits = diff_text("a\nb\n", "b\n");
    assert_eq!(hunks(&edits, 0)[0].to_string(), "@@ -1,1 +0,0 @@");
}

#[test]
fn unified_text() {
    let text = unified("a/f", "b/f", "a\nb\nc\nd\n", "a\nB\nc\nd\ne\n", 1);
    assert_eq!(text, "--- a/f\n+++ b/f\n@@ -1,4 +1,5 @@\n a\n-b\n+B\n c\n d\n+e\n");
    assert_eq!(unified("a", "b", "same\n", "same\n", 3), "");
}

#[test]
fn parse_reads_what_unified_writes() {
    let old = numbered(8);
    let new = replace(&old, 5, "five");
    let text = unified("a/x", "b/x", &old, &new, 2);
    let p = patch::parse(&text).unwrap();
    assert_eq!((p.old_name.as_str(), p.new_name.as_str()), ("a/x", "b/x"));
    assert_eq!(p.hunks.len(), 1);
    assert_eq!(p.counts(), (1, 1));
    assert_eq!(p.to_string(), text);
    assert!(p.hunks[0].lines.contains(&Line::Del("l5".to_string())));
}

#[test]
fn parse_errors() {
    assert_eq!(patch::parse("hello\n"), Err(PatchError::MissingHeader));
    assert!(matches!(
        patch::parse("--- a\n+++ b\n@@ nonsense @@\n"),
        Err(PatchError::BadHunkHeader(_))
    ));
    assert_eq!(
        patch::parse("--- a\n+++ b\n@@ -1,2 +1,2 @@\n x\n"),
        Err(PatchError::CountMismatch { hunk: 0 })
    );
    assert!(matches!(
        patch::parse("--- a\n+++ b\n@@ -1,1 +1,1 @@\n?x\n"),
        Err(PatchError::BadLine { line: 4, .. })
    ));
    let one = patch::parse("--- a\n+++ b\n@@ -3 +3 @@\n-x\n+y\n").unwrap();
    assert_eq!((one.hunks[0].old_len, one.hunks[0].new_len), (1, 1));
}

#[test]
fn applying_a_patch_gives_the_new_text() {
    let old = numbered(20);
    let new = replace(&replace(&old, 4, "four"), 17, "seventeen");
    let p = patch::parse(&unified("a", "b", &old, &new, 3)).unwrap();
    assert_eq!(p.hunks.len(), 2);
    assert_eq!(patch::apply(&p, &old).unwrap(), new);
    assert!(patch::applies(&p, &old));
    assert!(!patch::applies(&p, &new));
}

#[test]
fn applying_pure_insertions() {
    let old = "a\nb\nc\n";
    let new = "a\nx\nb\nc\n";
    let p = patch::parse(&unified("a", "b", old, new, 0)).unwrap();
    assert_eq!(patch::apply(&p, old).unwrap(), new);
    let at_start = patch::parse(&unified("a", "b", old, "top\na\nb\nc\n", 0)).unwrap();
    assert_eq!(at_start.hunks[0].old_start, 0);
    assert_eq!(patch::apply(&at_start, old).unwrap(), "top\na\nb\nc\n");
    let at_end = patch::parse(&unified("a", "b", old, "a\nb\nc\nz\n", 0)).unwrap();
    assert_eq!(patch::apply(&at_end, old).unwrap(), "a\nb\nc\nz\n");
}

#[test]
fn applying_pure_deletions() {
    let old = "a\nb\nc\nd\n";
    let p = patch::parse(&unified("a", "b", old, "a\nd\n", 0)).unwrap();
    assert_eq!(patch::apply(&p, old).unwrap(), "a\nd\n");
    let all = patch::parse(&unified("a", "b", old, "", 0)).unwrap();
    assert_eq!(patch::apply(&all, old).unwrap(), "");
}

#[test]
fn a_mismatch_names_the_hunk() {
    let old = numbered(6);
    let new = replace(&old, 3, "x");
    let p = patch::parse(&unified("a", "b", &old, &new, 1)).unwrap();
    let other = replace(&old, 3, "different");
    assert_eq!(
        patch::apply(&p, &other),
        Err(PatchError::Mismatch { hunk: 0, line: 2 })
    );
}

#[test]
fn fuzz_finds_a_shifted_hunk() {
    let old = numbered(10);
    let new = replace(&old, 6, "six");
    let p = patch::parse(&unified("a", "b", &old, &new, 2)).unwrap();
    let shifted = format!("extra1\nextra2\n{old}");
    assert!(patch::apply(&p, &shifted).is_err());
    let fixed = patch::apply_with_fuzz(&p, &shifted, 2).unwrap();
    assert_eq!(fixed, format!("extra1\nextra2\n{new}"));
    assert!(patch::apply_with_fuzz(&p, &shifted, 1).is_err());
    let moved_up = old.lines().skip(1).map(|l| format!("{l}\n")).collect::<String>();
    assert_eq!(
        patch::apply_with_fuzz(&p, &moved_up, 1).unwrap(),
        new.lines().skip(1).map(|l| format!("{l}\n")).collect::<String>()
    );
}

#[test]
fn reversing_a_patch() {
    let old = "a\nb\nc\nd\n";
    let new = "a\nB\nc\nd\ne\n";
    let forward = patch::parse(&unified("a/f", "b/f", old, new, 1)).unwrap();
    let back = patch::reverse(&forward);
    assert!(back.to_string().starts_with("--- b/f\n+++ a/f\n@@ -1,5 +1,4 @@\n"));
    assert_eq!(patch::apply(&back, new).unwrap(), old);
    assert_eq!(patch::reverse(&back), forward);
}

#[test]
fn reversing_insertions_and_deletions() {
    let old = "a\nb\nc\n";
    let new = "a\nx\ny\nb\nc\n";
    let p = patch::parse(&unified("a", "b", old, new, 0)).unwrap();
    let back = patch::reverse(&p);
    assert_eq!((back.hunks[0].old_len, back.hunks[0].new_len), (2, 0));
    assert_eq!(patch::apply(&back, new).unwrap(), old);
}

#[test]
fn a_series_and_touched_lines() {
    let v1 = numbered(5);
    let v2 = replace(&v1, 2, "two");
    let v3 = replace(&v2, 4, "four");
    let p1 = patch::parse(&unified("a", "b", &v1, &v2, 1)).unwrap();
    let p2 = patch::parse(&unified("a", "b", &v2, &v3, 1)).unwrap();
    assert_eq!(patch::apply_series(&[p1.clone(), p2], &v1).unwrap(), v3);
    assert_eq!(patch::touched_lines(&p1), vec![2]);
    let ins = patch::parse(&unified("a", "b", "a\nb\n", "a\nx\nb\n", 0)).unwrap();
    assert_eq!(patch::touched_lines(&ins), Vec::<usize>::new());
    assert_eq!(unified("a", "b", "q\n", "q\n", 1), "");
}

#[test]
fn round_trip_over_many_small_files() {
    let mut seed: u64 = 12345;
    let mut next = move |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for round in 0..60 {
        let old: Vec<String> = (0..next(12)).map(|_| format!("w{}", next(5))).collect();
        let new: Vec<String> = (0..next(12)).map(|_| format!("w{}", next(5))).collect();
        let to_text = |v: &Vec<String>| v.iter().map(|l| format!("{l}\n")).collect::<String>();
        let (a, b) = (to_text(&old), to_text(&new));
        for context in [0usize, 1, 3] {
            let p = patch::parse(&unified("a", "b", &a, &b, context)).unwrap();
            assert_eq!(patch::apply(&p, &a).unwrap(), b, "round {round} context {context}");
            assert_eq!(patch::apply(&patch::reverse(&p), &b).unwrap(), a);
        }
    }
}

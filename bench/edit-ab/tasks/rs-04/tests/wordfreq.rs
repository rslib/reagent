use wordfreq::{count_words, hapaxes, length_histogram, longest_word, mean_word_length, tokenize, top_n};

#[test]
fn tokenize_basic() {
    assert_eq!(tokenize("Hello, World! Hello."), vec!["hello", "world", "hello"]);
}

#[test]
fn tokenize_keeps_digits_and_apostrophes() {
    assert_eq!(tokenize("Don't panic at 3pm in room 42"), vec!["don't", "panic", "at", "3pm", "in", "room", "42"]);
}

#[test]
fn counts() {
    let c = count_words("a b a c a b");
    assert_eq!(c["a"], 3);
    assert_eq!(c["b"], 2);
    assert_eq!(c["c"], 1);
}

#[test]
fn top_n_orders_ties_alphabetically() {
    let top = top_n("pear apple fig apple pear fig kiwi", 3);
    assert_eq!(
        top,
        vec![("apple".to_string(), 2), ("fig".to_string(), 2), ("pear".to_string(), 2)]
    );
}

#[test]
fn top_n_counts_first() {
    let top = top_n("b a b c c c", 2);
    assert_eq!(top, vec![("c".to_string(), 3), ("b".to_string(), 2)]);
}

#[test]
fn once_only_words() {
    assert_eq!(hapaxes("x y x z"), vec!["y", "z"]);
}

#[test]
fn lengths() {
    assert_eq!(longest_word("a bbb cc ddd"), Some("bbb".to_string()));
    assert_eq!(length_histogram("a bb cc d"), vec![(1, 2), (2, 2)]);
    assert!((mean_word_length("ab abcd") - 3.0).abs() < 1e-9);
}

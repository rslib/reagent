//! Word frequency counting.

use std::collections::HashMap;

/// Lower-cases the text and splits it into words made of letters and digits.
/// An apostrophe inside a word stays in it, so "don't" is one word.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_alphabetic() || (c == '\'' && !current.is_empty()) {
            current.extend(c.to_lowercase());
        } else if !current.is_empty() {
            words.push(trim_apostrophes(&current));
            current.clear();
        }
    }
    if !current.is_empty() {
        words.push(trim_apostrophes(&current));
    }
    words.retain(|w| !w.is_empty());
    words
}

fn trim_apostrophes(word: &str) -> String {
    word.trim_end_matches('\'').to_string()
}

pub fn count_words(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in tokenize(text) {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}

/// The n most common words, most common first. Words with the same count are
/// listed in alphabetical order.
pub fn top_n(text: &str, n: usize) -> Vec<(String, usize)> {
    let mut items: Vec<(String, usize)> = count_words(text).into_iter().collect();
    items.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0.cmp(&a.0))
    });
    items.truncate(n);
    items
}

/// Words that appear exactly once, in alphabetical order.
pub fn hapaxes(text: &str) -> Vec<String> {
    let mut words: Vec<String> = count_words(text)
        .into_iter()
        .filter(|(_, c)| *c == 1)
        .map(|(w, _)| w)
        .collect();
    words.sort();
    words
}

/// How many different words the text has.
pub fn unique_count(text: &str) -> usize {
    count_words(text).len()
}

/// The average word length, or 0.0 for an empty text.
pub fn mean_word_length(text: &str) -> f64 {
    let words = tokenize(text);
    if words.is_empty() {
        return 0.0;
    }
    let total: usize = words.iter().map(|w| w.chars().count()).sum();
    total as f64 / words.len() as f64
}

/// The longest word, the first one when several tie.
pub fn longest_word(text: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for word in tokenize(text) {
        let longer = match &best {
            Some(b) => word.chars().count() > b.chars().count(),
            None => true,
        };
        if longer {
            best = Some(word);
        }
    }
    best
}

/// Counts of words by length, as (length, count) sorted by length.
pub fn length_histogram(text: &str) -> Vec<(usize, usize)> {
    let mut map: HashMap<usize, usize> = HashMap::new();
    for word in tokenize(text) {
        *map.entry(word.chars().count()).or_insert(0) += 1;
    }
    let mut out: Vec<(usize, usize)> = map.into_iter().collect();
    out.sort();
    out
}

/// The share of the text taken by one word, from 0.0 to 1.0.
pub fn frequency(text: &str, word: &str) -> f64 {
    let words = tokenize(text);
    if words.is_empty() {
        return 0.0;
    }
    let wanted = word.to_lowercase();
    words.iter().filter(|w| **w == wanted).count() as f64 / words.len() as f64
}

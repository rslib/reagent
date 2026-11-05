//! Matching a query against names: prefix first, then subsequence.
//!
//! The command list, the file list and the sheets all filter the same way. A
//! name that starts with the query comes first, in the order the caller lists
//! it. Then come the names that hold the query as a subsequence, best score
//! first, and a name with no match is not a hit.
//!
//! The matcher keeps its buffers between keys: a run clears and refills them,
//! and once they are warm a run allocates nothing.

/// The score of a name that starts with the query, above every subsequence.
const PREFIX: i32 = i32::MAX;

/// What a match at the start of a name or of a word adds.
const WORD: i32 = 16;

/// What a match right after the previous match adds.
const NEXT: i32 = 8;

/// One name that matched: its index in the input, its score, and the range of
/// its marked characters in the matcher's mark buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The index of the name in the input.
    pub index: u32,
    /// How well the name matched. Higher is better; a name that starts with the
    /// query scores `i32::MAX`, above every subsequence.
    pub score: i32,
    /// The start and the length of the marks in the matcher's mark buffer.
    marks: (u32, u32),
}

/// The hits of the last run, best first, with their marks.
///
/// The hits, the marks and the scratch of a name's chars keep their capacity,
/// so a run after the first allocates nothing.
#[derive(Debug, Default)]
pub struct Matcher {
    hits: Vec<Hit>,
    marks: Vec<u16>,
    chars: Vec<char>,
}

impl Matcher {
    /// A matcher with no hits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Matches `query` against `names` in order and keeps the hits, best first.
    ///
    /// An empty query hits every name, in input order, with score 0 and no
    /// marks. ASCII case does not matter; other chars compare as they are.
    pub fn run<'a>(&mut self, query: &str, names: impl IntoIterator<Item = &'a str>) {
        self.hits.clear();
        self.marks.clear();
        for (index, name) in names.into_iter().enumerate() {
            let index = index as u32;
            let hit = if query.is_empty() {
                Some(Hit {
                    index,
                    score: 0,
                    marks: (0, 0),
                })
            } else if starts_with(name, query) {
                Some(Hit {
                    index,
                    score: PREFIX,
                    marks: (0, 0),
                })
            } else {
                self.subsequence(index, name, query)
            };
            if let Some(hit) = hit {
                self.hits.push(hit);
            }
        }
        self.hits
            .sort_unstable_by_key(|hit| (-hit.score, hit.index));
    }

    /// The hits of the last run, best first.
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    /// The char positions, not bytes, in the name that the query matched.
    pub fn marks(&self, hit: &Hit) -> &[u16] {
        let (start, len) = (hit.marks.0 as usize, hit.marks.1 as usize);
        &self.marks[start..start + len]
    }

    /// The capacities of the hits and the marks, for the tests that check reuse.
    pub fn capacity(&self) -> (usize, usize) {
        (self.hits.capacity(), self.marks.capacity())
    }

    /// The hit of one name that holds `query` as a subsequence, with its marks.
    ///
    /// One greedy pass from left to right picks the marks. When the first place
    /// that holds the needed char sits inside a word, the pass takes the first
    /// word start after it instead, as long as the next needed char still comes
    /// after that one.
    fn subsequence(&mut self, index: u32, name: &str, query: &str) -> Option<Hit> {
        self.chars.clear();
        self.chars.extend(name.chars());
        // A mark is a `u16`, and no path comes near this length.
        if self.chars.len() > u16::MAX as usize {
            return None;
        }
        let start = self.marks.len();
        let mut from = 0;
        let mut needed = query.chars().peekable();
        while let Some(c) = needed.next() {
            let Some(first) = find(&self.chars, from, c) else {
                self.marks.truncate(start);
                return None;
            };
            let mut at = first;
            if let Some(next) = needed.peek().copied()
                && !starts_word(&self.chars, first)
            {
                let Some(after) = find(&self.chars, first + 1, next) else {
                    self.marks.truncate(start);
                    return None;
                };
                if let Some(word) = find_word(&self.chars, first + 1, c)
                    && word < after
                {
                    at = word;
                }
            }
            self.marks.push(at as u16);
            from = at + 1;
        }
        let len = self.marks.len() - start;
        Some(Hit {
            index,
            score: score(&self.chars, &self.marks[start..]).min(PREFIX - 1),
            marks: (start as u32, len as u32),
        })
    }
}

/// Whether `name` starts with `query`, ignoring ASCII case.
fn starts_with(name: &str, query: &str) -> bool {
    let name = name.as_bytes();
    name.len() >= query.len()
        && name[..query.len()]
            .iter()
            .zip(query.as_bytes())
            .all(|(name, query)| name.eq_ignore_ascii_case(query))
}

/// The first index at or after `from` that holds `c`, ignoring ASCII case.
fn find(name: &[char], from: usize, c: char) -> Option<usize> {
    name.get(from..)?
        .iter()
        .position(|&name| name.eq_ignore_ascii_case(&c))
        .map(|at| at + from)
}

/// The first index at or after `from` that holds `c` at the start of a word.
fn find_word(name: &[char], from: usize, c: char) -> Option<usize> {
    (from..name.len()).find(|&at| name[at].eq_ignore_ascii_case(&c) && starts_word(name, at))
}

/// Whether a match at `at` starts a word: the name's start, or after a
/// separator.
fn starts_word(name: &[char], at: usize) -> bool {
    at == 0 || matches!(name[at - 1], '/' | '_' | '-' | '.' | ' ')
}

/// The score of the marks of one hit.
fn score(name: &[char], marks: &[u16]) -> i32 {
    let mut score = 0;
    let mut previous = None;
    for &mark in marks {
        let at = mark as usize;
        if starts_word(name, at) {
            score += WORD;
        }
        if let Some(previous) = previous {
            if at == previous + 1 {
                score += NEXT;
            } else {
                score -= (at - previous - 1) as i32;
            }
        }
        previous = Some(at);
    }
    score
}

#[cfg(test)]
mod tests {
    use super::Matcher;

    fn matcher(query: &str, names: &[&str]) -> Matcher {
        let mut matcher = Matcher::new();
        matcher.run(query, names.iter().copied());
        matcher
    }

    fn indexes(matcher: &Matcher) -> Vec<u32> {
        matcher.hits().iter().map(|hit| hit.index).collect()
    }

    #[test]
    fn empty_query_keeps_every_name_in_order() {
        let matcher = matcher("", &["/model", "/compact", "quit"]);
        assert_eq!(indexes(&matcher), [0, 1, 2]);
        assert!(matcher.hits().iter().all(|hit| hit.score == 0));
        assert!(matcher.marks(&matcher.hits()[0]).is_empty());
    }

    #[test]
    fn prefix_hits_come_before_subsequence_hits_and_keep_input_order() {
        let matcher = matcher("/m", &["/compact", "/model", "/mode", "/cost"]);
        assert_eq!(indexes(&matcher), [1, 2, 0]);
        assert!(matcher.hits()[0].score > matcher.hits()[2].score);
        assert!(matcher.marks(&matcher.hits()[0]).is_empty());
    }

    #[test]
    fn subsequence_hits_rank_by_score_then_input_order() {
        let ranked = matcher("/ct", &["/compact", "/cost"]);
        assert_eq!(indexes(&ranked), [1, 0]);

        let tied = matcher("/ct", &["/cot", "/cat"]);
        assert_eq!(indexes(&tied), [0, 1]);
    }

    #[test]
    fn case_does_not_matter() {
        let prefix = matcher("MOD", &["/model", "/mode"]);
        assert_eq!(indexes(&prefix), [0, 1]);

        let partial = matcher("Md", &["/mode"]);
        assert_eq!(indexes(&partial), [0]);
        assert_eq!(partial.marks(&partial.hits()[0]), [1, 3]);
    }

    #[test]
    fn a_boundary_match_ranks_above_a_mid_word_match() {
        let matcher = matcher("sid", &["inside.rs", "src/sidebar.rs"]);
        assert_eq!(indexes(&matcher), [1, 0]);
        assert_eq!(matcher.marks(&matcher.hits()[0]), [0, 5, 6]);
        assert_eq!(matcher.marks(&matcher.hits()[1]), [2, 3, 4]);
    }

    #[test]
    fn marks_are_char_positions() {
        let matcher = matcher("sid", &["é/sidebar.rs"]);
        assert_eq!(indexes(&matcher), [0]);
        assert_eq!(matcher.marks(&matcher.hits()[0]), [2, 3, 4]);
    }

    #[test]
    fn a_path_query_marks_the_folder_that_holds_the_name() {
        let matcher = matcher("app/sid", &["crates/tui/src/app/sidebar.rs"]);
        assert_eq!(indexes(&matcher), [0]);
        assert_eq!(
            matcher.marks(&matcher.hits()[0]),
            &[15, 16, 17, 18, 19, 20, 21][..]
        );
    }

    #[test]
    fn no_match_gives_no_hit() {
        let matcher = matcher("xyz", &["/model", "/cost", "quit"]);
        assert!(matcher.hits().is_empty());
    }

    #[test]
    fn a_second_run_keeps_the_capacity() {
        let names = [
            "crates/tui/src/pick.rs",
            "crates/tui/src/keys.rs",
            "crates/tui/src/lib.rs",
        ];
        let mut matcher = Matcher::new();
        matcher.run("src/pi", names);
        let first = matcher.capacity();
        assert_ne!(first.0, 0);
        assert_ne!(first.1, 0);

        matcher.run("src/pi", names);
        assert_eq!(matcher.capacity(), first);
        assert_eq!(indexes(&matcher), [0]);
    }

    #[test]
    fn a_later_run_clears_the_marks_of_the_earlier_one() {
        let mut matcher = matcher("mo", &["/model"]);
        assert_eq!(matcher.marks(&matcher.hits()[0]), [1, 2]);

        matcher.run("de", ["/model"]);
        assert_eq!(matcher.marks(&matcher.hits()[0]), [3, 4]);
    }
}

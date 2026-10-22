//! A port of git's ignore rules (gitignore(5)) and its wildmatch, so a walk
//! outside a repository skips the files git would skip.
//!
//! Matching is case-sensitive: `core.ignorecase` is not read. Only the global
//! excludes file at its default location is supported (see
//! [`global_excludes_path`]); `core.excludesFile` is not read.

use std::path::PathBuf;
use std::sync::Arc;

use reagent_config::glob::is_glob_special;
pub(crate) use reagent_config::glob::wildmatch;

/// The default global excludes file: `$XDG_CONFIG_HOME/git/ignore`, else
/// `$HOME/.config/git/ignore`.
pub(crate) fn global_excludes_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("git/ignore"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/git/ignore"))
}

#[derive(Debug)]
struct Pattern {
    glob: Vec<u8>,
    negate: bool,
    dir_only: bool,
    /// The glob has a slash, so it is relative to its layer's directory.
    anchored: bool,
    /// Length of the literal head of `glob`, which git compares before wildmatch.
    prefix: usize,
}

impl Pattern {
    fn parse(line: &[u8]) -> Option<Self> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first() == Some(&b'#') {
            return None;
        }
        let mut glob = &line[..trimmed_len(line)];
        let negate = glob.first() == Some(&b'!');
        if negate {
            glob = &glob[1..];
        }
        let dir_only = glob.last() == Some(&b'/');
        if dir_only {
            glob = &glob[..glob.len() - 1];
        }
        let anchored = glob.contains(&b'/');
        if glob.first() == Some(&b'/') {
            glob = &glob[1..];
        }
        if glob.is_empty() {
            return None;
        }
        let prefix = glob
            .iter()
            .position(|c| is_glob_special(*c))
            .unwrap_or(glob.len());
        Some(Self {
            glob: glob.to_vec(),
            prefix,
            negate,
            dir_only,
            anchored,
        })
    }

    fn matches(&self, sub: &[u8], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if !self.anchored {
            let name = sub.rsplit(|b| *b == b'/').next().unwrap_or(sub);
            return wildmatch(&self.glob, name);
        }
        // Like git, compare the literal head first and wildmatch only the rest:
        // `ab**/*.b` must match `ab.b`.
        let pre = self.prefix;
        if sub.len() < pre || sub[..pre] != self.glob[..pre] {
            return false;
        }
        if pre == self.glob.len() && sub.len() == pre {
            return true;
        }
        wildmatch(&self.glob[pre..], &sub[pre..])
    }
}

/// Length of `line` without its unescaped trailing spaces.
fn trimmed_len(line: &[u8]) -> usize {
    let mut keep = 0;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {}
            b'\\' => {
                if i + 1 == line.len() {
                    // git leaves a line that ends in a lone backslash untouched.
                    return line.len();
                }
                i += 1;
                keep = i + 1;
            }
            _ => keep = i + 1,
        }
        i += 1;
    }
    keep
}

#[derive(Debug)]
struct Layer {
    parent: Option<usize>,
    /// Directory of the `.gitignore`, relative to the walk root, no trailing slash.
    base: Vec<u8>,
    patterns: Vec<Pattern>,
}

/// All parsed ignore files of one walk. A layer chains to the layer of the
/// parent directory, so a directory entry only needs its innermost layer index.
#[derive(Debug, Default)]
pub(crate) struct Ignores {
    layers: Vec<Layer>,
}

impl Ignores {
    /// Parses `text` as an ignore file for directory `base` under the layer
    /// `parent`; returns the new innermost layer, or `parent` when `text` has
    /// no patterns.
    pub(crate) fn add(&mut self, parent: Option<usize>, base: &[u8], text: &[u8]) -> Option<usize> {
        let patterns = parse_patterns(text);
        if patterns.is_empty() {
            return parent;
        }
        self.layers.push(Layer {
            parent,
            base: base.to_vec(),
            patterns,
        });
        Some(self.layers.len() - 1)
    }

    /// Whether the entry `rel` (relative to the walk root, `/` separated) is
    /// ignored by the layers from `tip` outward. The deepest file wins and the
    /// last matching pattern within a file wins. The caller must not descend
    /// into an ignored directory, which is how git forbids re-including
    /// a file under an excluded parent.
    pub(crate) fn is_ignored(&self, tip: Option<usize>, rel: &[u8], is_dir: bool) -> bool {
        let mut cur = tip;
        while let Some(i) = cur {
            let layer = &self.layers[i];
            cur = layer.parent;
            if let Some(ignored) = verdict(&layer.base, &layer.patterns, rel, is_dir) {
                return ignored;
            }
        }
        false
    }
}

/// One `.gitignore` file of a walk that many threads share: each directory
/// holds its innermost chain, which links to the chains of its parents.
#[derive(Debug)]
pub(crate) struct Chain {
    parent: Option<Arc<Chain>>,
    /// Directory of the `.gitignore`, relative to the walk root, no trailing slash.
    base: Vec<u8>,
    /// For a `.gitignore` above the walk root: the root's path relative to
    /// the `.gitignore`'s directory, prepended to each entry before matching.
    above: Vec<u8>,
    patterns: Vec<Pattern>,
}

impl Chain {
    /// Parses `text` as an ignore file for directory `base` under `parent`;
    /// returns the new innermost chain, or `parent` when `text` has no patterns.
    pub(crate) fn add(parent: Option<Arc<Chain>>, base: &[u8], text: &[u8]) -> Option<Arc<Chain>> {
        let patterns = parse_patterns(text);
        if patterns.is_empty() {
            return parent;
        }
        Some(Arc::new(Chain {
            parent,
            base: base.to_vec(),
            above: Vec::new(),
            patterns,
        }))
    }

    /// Like [`Chain::add`] for the `.gitignore` of a directory above the walk
    /// root, where `above` is the root's path relative to that directory.
    pub(crate) fn add_above(
        parent: Option<Arc<Chain>>,
        above: &[u8],
        text: &[u8],
    ) -> Option<Arc<Chain>> {
        let patterns = parse_patterns(text);
        if patterns.is_empty() {
            return parent;
        }
        Some(Arc::new(Chain {
            parent,
            base: Vec::new(),
            above: above.to_vec(),
            patterns,
        }))
    }

    /// Whether `rel` is ignored by this chain and its parents, by the rules
    /// of [`Ignores::is_ignored`]. `scratch` holds the joined path of a chain
    /// above the walk root: the caller reuses it across the entries of one
    /// directory, so matching costs no allocation per entry.
    pub(crate) fn is_ignored(&self, rel: &[u8], is_dir: bool, scratch: &mut Vec<u8>) -> bool {
        let mut cur = Some(self);
        while let Some(chain) = cur {
            let rel = if chain.above.is_empty() {
                rel
            } else {
                scratch.clear();
                scratch.extend_from_slice(&chain.above);
                scratch.push(b'/');
                scratch.extend_from_slice(rel);
                &scratch[..]
            };
            if let Some(ignored) = verdict(&chain.base, &chain.patterns, rel, is_dir) {
                return ignored;
            }
            cur = chain.parent.as_deref();
        }
        false
    }
}

fn parse_patterns(text: &[u8]) -> Vec<Pattern> {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    text.split(|b| *b == b'\n')
        .filter_map(Pattern::parse)
        .collect()
}

/// The last matching pattern of one ignore file decides: `Some(true)` when it
/// ignores `rel`, `Some(false)` when it re-includes it, `None` when no pattern
/// matches or `rel` lies outside `base`.
fn verdict(base: &[u8], patterns: &[Pattern], rel: &[u8], is_dir: bool) -> Option<bool> {
    let sub = if base.is_empty() {
        rel
    } else {
        match rel.strip_prefix(base) {
            Some(rest) if rest.first() == Some(&b'/') => &rest[1..],
            _ => return None,
        }
    };
    patterns
        .iter()
        .rev()
        .find(|p| p.matches(sub, is_dir))
        .map(|p| !p.negate)
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test setup on a temp dir"
)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn ignored(rules: &str, path: &str, is_dir: bool) -> bool {
        let mut ig = Ignores::default();
        let tip = ig.add(None, b"", rules.as_bytes());
        ig.is_ignored(tip, path.as_bytes(), is_dir)
    }

    fn file(rules: &str, path: &str) -> bool {
        ignored(rules, path, false)
    }

    fn dir(rules: &str, path: &str) -> bool {
        ignored(rules, path, true)
    }

    #[test]
    fn comments_blanks_and_escapes() {
        let rules = "# note\n\n   \n\\#a\n\\!b\n";
        assert!(file(rules, "#a"));
        assert!(file(rules, "!b"));
        assert!(!file(rules, "note"));
        assert!(!file(rules, "a"));
        assert!(!file(rules, "b"));
    }

    #[test]
    fn trailing_spaces_and_crlf() {
        assert!(file("a.txt   \n", "a.txt"));
        assert!(file("b\\ \n", "b "));
        assert!(!file("b\\ \n", "b"));
        assert!(file("c.txt\r\nd.txt\r\n", "c.txt"));
        assert!(file("c.txt\r\nd.txt\r\n", "d.txt"));
        assert!(file("\u{feff}e.txt\n", "e.txt"));
    }

    #[test]
    fn negation_reincludes_and_last_match_wins() {
        let rules = "*.log\n!keep.log\n";
        assert!(file(rules, "a.log"));
        assert!(!file(rules, "keep.log"));
        assert!(file("!keep.log\n*.log\n", "keep.log"));
    }

    #[test]
    fn parent_excluded_cannot_reinclude() {
        let mut ig = Ignores::default();
        let tip = ig.add(None, b"", b"out/\n!out/keep.txt\n");
        assert!(ig.is_ignored(tip, b"out", true));
        // The walk never descends into `out`, so `out/keep.txt` is unreachable.
        assert!(!ig.is_ignored(tip, b"out", false));
    }

    #[test]
    fn dir_only_pattern() {
        assert!(dir("build/\n", "build"));
        assert!(dir("build/\n", "src/build"));
        assert!(!file("build/\n", "build"));
    }

    #[test]
    fn anchored_and_unanchored() {
        assert!(file("/a.txt\n", "a.txt"));
        assert!(!file("/a.txt\n", "d/a.txt"));
        assert!(file("a.txt\n", "d/e/a.txt"));
        assert!(file("d/a.txt\n", "d/a.txt"));
        assert!(!file("d/a.txt\n", "x/d/a.txt"));
        assert!(!file("d/*.txt\n", "d/e/a.txt"));
    }

    #[test]
    fn star_and_question_stop_at_slash() {
        assert!(file("a*c\n", "abbc"));
        assert!(!file("x/a*c\n", "x/a/c"));
        assert!(file("x/a?c\n", "x/abc"));
        assert!(!file("x/a?c\n", "x/a/c"));
        assert!(file("*\n!keep\n", "other"));
    }

    #[test]
    fn brackets_and_classes() {
        assert!(file("[abc]x\n", "bx"));
        assert!(!file("[abc]x\n", "dx"));
        assert!(file("[a-c]x\n", "cx"));
        assert!(file("[!a-c]x\n", "dx"));
        assert!(!file("[!a-c]x\n", "ax"));
        assert!(file("[^a]x\n", "bx"));
        assert!(file("[]]x\n", "]x"));
        assert!(file("[[:digit:]]x\n", "5x"));
        assert!(!file("[[:digit:]]x\n", "ax"));
        assert!(file("[[:alpha:][:space:]]x\n", " x"));
        assert!(file("[[:upper:]]x\n", "Ax"));
        assert!(!file("[[:upper:]]x\n", "ax"));
        assert!(file("\\*x\n", "*x"));
        assert!(!file("\\*x\n", "ax"));
    }

    #[test]
    fn double_star_forms() {
        assert!(file("**/c\n", "c"));
        assert!(file("**/c\n", "a/b/c"));
        assert!(file("a/**\n", "a/b"));
        assert!(file("a/**\n", "a/b/c"));
        assert!(!file("a/**\n", "x/a/b"));
        assert!(file("a/**/z\n", "a/z"));
        assert!(file("a/**/z\n", "a/b/c/z"));
        assert!(!file("a/**/z\n", "a/b/y"));
        assert!(file("foo**bar\n", "fooxxbar"));
        assert!(!file("x/foo**bar\n", "x/foo/bar"));
    }

    #[test]
    fn literal_head_is_split_before_wildmatch() {
        assert!(file("ab**/*.b\n", "ab.b"));
        assert!(file("ab**/*.b\n", "ab/x.b"));
        assert!(file("/a**/a\n", "a/a"));
        assert!(file("/a**/a\n", "ax/a"));
        assert!(!file("/a**/a\n", "b/a"));
    }

    #[test]
    fn deeper_file_overrides_shallower() {
        let mut ig = Ignores::default();
        let root = ig.add(None, b"", b"*.txt\n");
        let sub = ig.add(root, b"sub", b"!keep.txt\n/local.md\n");
        assert!(ig.is_ignored(sub, b"a.txt", false));
        assert!(ig.is_ignored(sub, b"sub/a.txt", false));
        assert!(!ig.is_ignored(sub, b"sub/keep.txt", false));
        assert!(ig.is_ignored(sub, b"sub/local.md", false));
        assert!(!ig.is_ignored(sub, b"sub/x/local.md", false));
    }

    #[test]
    fn global_layer_has_lowest_priority() {
        let mut ig = Ignores::default();
        let global = ig.add(None, b"", b"*.bak\n");
        let root = ig.add(global, b"", b"!a.bak\n");
        assert!(ig.is_ignored(root, b"b.bak", false));
        assert!(!ig.is_ignored(root, b"a.bak", false));
    }

    const ROOT_RULES: &str = "# comment\n\\#hash.txt\n\\!bang.txt\n*.log\n!keep.log\n/rooted.txt\n\
build/\ndocs/*.md\n!docs/keep.md\n**/cache\nlogs/**\na/**/z.txt\ntemp?.dat\n[abc]file.txt\n\
[!a-c]x.txt\n[[:digit:]]num.txt\n*.[ch]\nspace\\ \ntrail.txt   \nfoo**bar\n/dir_a/*/inner.txt\n\
*.tmp\n\\*star.txt\nout/\n!out/keep.txt\ncrlf.txt\r\nab**/*.b\n/a**/a\n";
    const SUB_RULES: &str = "*.txt\n!important.txt\n/local.md\ndeep/\n";
    const OTHER_RULES: &str = "!x.txt\n/y.txt\n";
    const FILES: &[&str] = &[
        "#hash.txt",
        "!bang.txt",
        "a.log",
        "keep.log",
        "sub/keep.log",
        "rooted.txt",
        "sub/rooted.txt",
        "build/x.o",
        "src/build/y.o",
        "lib/build",
        "docs/a.md",
        "docs/keep.md",
        "docs/sub/b.md",
        "other/docs/c.md",
        "cache",
        "x/cache/f.txt",
        "x/y/cache",
        "logs/a.txt",
        "logs/b/c.txt",
        "a/z.txt",
        "a/b/z.txt",
        "a/b/c/z.txt",
        "a/q.txt",
        "tempA.dat",
        "temp.dat",
        "tempAB.dat",
        "afile.txt",
        "dfile.txt",
        "bx.txt",
        "dx.txt",
        "1num.txt",
        "anum.txt",
        "m.c",
        "m.h",
        "m.o",
        "space ",
        "space",
        "trail.txt",
        "foobar",
        "fooxxbar",
        "foo/bar",
        "dir_a/x/inner.txt",
        "dir_a/x/y/inner.txt",
        "dir_a/inner.txt",
        "z.tmp",
        "sub/z.tmp",
        "*star.txt",
        "xstar.txt",
        "out/keep.txt",
        "out/other.txt",
        "crlf.txt",
        "sub/a.txt",
        "sub/important.txt",
        "sub/local.md",
        "sub/x/local.md",
        "sub/deep/f.rs",
        "sub/other/x.txt",
        "sub/other/y.txt",
        "sub/other/z.txt",
        "sub/other/w.rs",
        "plain.rs",
        "ab.b",
        "ab/x.b",
        "a/a",
        "ax/a",
        "a/b/a",
    ];

    fn load(ig: &mut Ignores, tip: Option<usize>, dir: &Path, base: &[u8]) -> Option<usize> {
        match std::fs::read(dir.join(".gitignore")) {
            Ok(text) => ig.add(tip, base, &text),
            Err(_) => tip,
        }
    }

    /// Our decision for `rel` the way the walk reaches it: every ancestor
    /// directory is checked first.
    fn decide(root: &Path, rel: &str) -> bool {
        let mut ig = Ignores::default();
        let mut cur = root.to_path_buf();
        let mut tip = load(&mut ig, None, &cur, b"");
        let parts: Vec<&str> = rel.split('/').collect();
        let mut so_far = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                so_far.push('/');
            }
            so_far.push_str(part);
            cur.push(part);
            let is_dir = cur.is_dir();
            if ig.is_ignored(tip, so_far.as_bytes(), is_dir) {
                return true;
            }
            if is_dir {
                tip = load(&mut ig, tip, &cur, so_far.as_bytes());
            }
        }
        false
    }

    #[test]
    fn agrees_with_git_check_ignore() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let empty = tmp.path().join("xdg");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("XDG_CONFIG_HOME", &empty);
            cmd
        };
        assert!(git(&["init", "-q"]).output().unwrap().status.success());

        std::fs::write(root.join(".gitignore"), ROOT_RULES).unwrap();
        for (dir, rules) in [("sub", SUB_RULES), ("sub/other", OTHER_RULES)] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(".gitignore"), rules).unwrap();
        }
        let mut paths = BTreeSet::new();
        for f in FILES {
            let full = root.join(f);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "x").unwrap();
            let mut acc = String::new();
            for part in f.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                paths.insert(acc.clone());
            }
        }
        assert!(paths.len() >= 30);

        let mut child = git(&["check-ignore", "--stdin", "-z", "--no-index"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = Vec::new();
        for p in &paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let out = child.wait_with_output().unwrap();
        let theirs: BTreeSet<String> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();

        let mismatches: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                let ours = decide(&root, p);
                let git_says = theirs.contains(p);
                (ours != git_says).then(|| format!("{p}: ours={ours} git={git_says}"))
            })
            .collect();
        assert!(
            mismatches.is_empty(),
            "{} of {} paths differ:\n{}",
            mismatches.len(),
            paths.len(),
            mismatches.join("\n")
        );
        assert!(!theirs.is_empty());
    }
}

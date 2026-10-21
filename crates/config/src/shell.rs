//! Splitting a bash command into its parts, for permission rules.

use std::borrow::Cow;

const KEYWORDS: [&str; 12] = [
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
];
const MORE_KEYWORDS: [&str; 2] = ["function", "select"];

/// One simple command of a command line: its words after wrappers are
/// dropped, and its redirect targets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Part<'a> {
    pub words: Vec<Cow<'a, str>>,
    pub writes: Vec<Cow<'a, str>>,
    pub reads: Vec<Cow<'a, str>>,
}

/// A parsed command line. When `complex` is set, the parts are a best effort
/// and no allow rule may trust them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command<'a> {
    pub parts: Vec<Part<'a>>,
    pub complex: bool,
    /// Set when a program word is built by an expansion or a glob, so the
    /// program is not known.
    pub unknown_program: bool,
}

/// Splits `text` into its parts.
pub fn parse(text: &str) -> Command<'_> {
    parse_at(text, true)
}

/// The basename of a program word: the text after the last `/`.
pub fn program_name(word: &str) -> &str {
    word.rsplit_once('/').map_or(word, |(_, name)| name)
}

struct Word<'a> {
    text: Cow<'a, str>,
    glob: bool,
}

#[derive(Default)]
struct Builder<'a> {
    words: Vec<Word<'a>>,
    writes: Vec<Cow<'a, str>>,
    reads: Vec<Cow<'a, str>>,
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    complex: bool,
    unknown: bool,
}

fn is_term(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\r' | b'\n' | b'&' | b'|' | b';' | b'<' | b'>' | b'(' | b')' | b'{' | b'}'
    )
}

fn push<'a>(out: &mut Cow<'a, str>, s: &'a str) {
    if s.is_empty() {
        return;
    }
    if out.is_empty() {
        *out = Cow::Borrowed(s);
    } else {
        out.to_mut().push_str(s);
    }
}

impl<'a> Lexer<'a> {
    fn peek(&self, off: usize) -> Option<u8> {
        self.src.as_bytes().get(self.pos + off).copied()
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }

    fn read_word(&mut self) -> Word<'a> {
        let src = self.src;
        let bytes = src.as_bytes();
        let mut text = Cow::Borrowed("");
        let mut glob = false;
        while let Some(&b) = bytes.get(self.pos) {
            if is_term(b) {
                break;
            }
            match b {
                b'\'' => {
                    let start = self.pos + 1;
                    match src[start..].find('\'') {
                        Some(len) => {
                            push(&mut text, &src[start..start + len]);
                            self.pos = start + len + 1;
                        }
                        None => {
                            self.complex = true;
                            push(&mut text, &src[start..]);
                            self.pos = src.len();
                        }
                    }
                }
                b'"' => {
                    self.pos += 1;
                    self.double_quoted(&mut text);
                }
                b'\\' => {
                    self.pos += 1;
                    if let Some(c) = src[self.pos..].chars().next() {
                        let end = self.pos + c.len_utf8();
                        if c != '\n' {
                            push(&mut text, &src[self.pos..end]);
                        }
                        self.pos = end;
                    }
                }
                _ => {
                    let start = self.pos;
                    while let Some(&b) = bytes.get(self.pos) {
                        if is_term(b) || matches!(b, b'\'' | b'"' | b'\\') {
                            break;
                        }
                        match b {
                            b'$' | b'`' => self.complex = true,
                            b'*' | b'?' | b'[' => glob = true,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    push(&mut text, &src[start..self.pos]);
                }
            }
        }
        Word { text, glob }
    }

    fn double_quoted(&mut self, text: &mut Cow<'a, str>) {
        let src = self.src;
        let bytes = src.as_bytes();
        loop {
            let start = self.pos;
            while let Some(&b) = bytes.get(self.pos) {
                if matches!(b, b'"' | b'\\' | b'$' | b'`') {
                    break;
                }
                self.pos += 1;
            }
            push(text, &src[start..self.pos]);
            match bytes.get(self.pos) {
                None => {
                    self.complex = true;
                    return;
                }
                Some(b'"') => {
                    self.pos += 1;
                    return;
                }
                Some(b'\\') => match bytes.get(self.pos + 1) {
                    Some(b'$' | b'`' | b'"' | b'\\') => {
                        push(text, &src[self.pos + 1..self.pos + 2]);
                        self.pos += 2;
                    }
                    Some(b'\n') => self.pos += 2,
                    _ => {
                        push(text, &src[self.pos..self.pos + 1]);
                        self.pos += 1;
                    }
                },
                Some(_) => {
                    self.complex = true;
                    push(text, &src[self.pos..self.pos + 1]);
                    self.pos += 1;
                }
            }
        }
    }

    fn redirect(&mut self, cur: &mut Builder<'a>, input: bool, dup_ok: bool) {
        let mut write = !input;
        if self.peek(0) == Some(b'>') {
            self.pos += 1;
            write = true;
        }
        let mut dup = false;
        if dup_ok && self.peek(0) == Some(b'&') {
            self.pos += 1;
            dup = true;
        } else if !input && self.peek(0) == Some(b'|') {
            self.pos += 1;
        }
        self.skip_blanks();
        match self.peek(0) {
            Some(b) if !is_term(b) => {
                let word = self.read_word();
                if word.glob {
                    self.complex = true;
                }
                let fd = dup
                    && (&*word.text == "-"
                        || (!word.text.is_empty()
                            && word.text.bytes().all(|b| b.is_ascii_digit())));
                if input && !fd {
                    cur.reads.push(word.text.clone());
                }
                if write && !fd {
                    cur.writes.push(word.text);
                }
            }
            _ => self.complex = true,
        }
    }
}

fn parse_at(text: &str, unwrap: bool) -> Command<'_> {
    let mut lx = Lexer {
        src: text,
        pos: 0,
        complex: false,
        unknown: false,
    };
    let mut parts = Vec::new();
    let mut cur = Builder::default();
    while let Some(b) = lx.peek(0) {
        match b {
            b' ' | b'\t' | b'\r' => lx.pos += 1,
            b'\n' | b';' => {
                lx.pos += 1;
                finish(
                    &mut parts,
                    &mut cur,
                    &mut lx.complex,
                    &mut lx.unknown,
                    unwrap,
                );
            }
            b'&' => match lx.peek(1) {
                Some(b'&') => {
                    lx.pos += 2;
                    finish(
                        &mut parts,
                        &mut cur,
                        &mut lx.complex,
                        &mut lx.unknown,
                        unwrap,
                    );
                }
                Some(b'>') => {
                    lx.pos += 1;
                    lx.redirect(&mut cur, false, false);
                }
                _ => {
                    lx.pos += 1;
                    finish(
                        &mut parts,
                        &mut cur,
                        &mut lx.complex,
                        &mut lx.unknown,
                        unwrap,
                    );
                }
            },
            b'|' => {
                lx.pos += 1;
                if matches!(lx.peek(0), Some(b'&' | b'|')) {
                    lx.pos += 1;
                }
                finish(
                    &mut parts,
                    &mut cur,
                    &mut lx.complex,
                    &mut lx.unknown,
                    unwrap,
                );
            }
            b'(' | b')' | b'{' | b'}' => {
                lx.complex = true;
                lx.pos += 1;
                finish(
                    &mut parts,
                    &mut cur,
                    &mut lx.complex,
                    &mut lx.unknown,
                    unwrap,
                );
            }
            b'>' => {
                lx.pos += 1;
                lx.redirect(&mut cur, false, true);
            }
            b'<' => match lx.peek(1) {
                Some(b'<') => {
                    lx.complex = true;
                    lx.pos += 2;
                }
                Some(b'(') => {
                    lx.complex = true;
                    lx.pos += 1;
                }
                _ => {
                    lx.pos += 1;
                    lx.redirect(&mut cur, true, true);
                }
            },
            _ => {
                let start = lx.pos;
                let word = lx.read_word();
                let raw = &text[start..lx.pos];
                let fd = raw.bytes().all(|b| b.is_ascii_digit())
                    && matches!(lx.peek(0), Some(b'<' | b'>'));
                if !fd {
                    cur.words.push(word);
                }
            }
        }
    }
    finish(
        &mut parts,
        &mut cur,
        &mut lx.complex,
        &mut lx.unknown,
        unwrap,
    );
    Command {
        parts,
        complex: lx.complex,
        unknown_program: lx.unknown,
    }
}

fn is_assignment(text: &str) -> bool {
    let Some((name, _)) = text.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_risky_assignment(text: &str) -> bool {
    let name = text.split_once('=').map_or(text, |(name, _)| name);
    matches!(
        name,
        "PATH" | "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS" | "PS4" | "IFS"
    ) || ["LD_", "DYLD_", "GIT_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Whether a word could be a command line passed to another program: it holds
/// whitespace or one of `;&|`.
pub fn shell_text(word: &str) -> bool {
    word.chars()
        .any(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
}

fn is_keyword(text: &str) -> bool {
    KEYWORDS.contains(&text) || MORE_KEYWORDS.contains(&text)
}

fn check_head(words: &[Word<'_>], complex: &mut bool) {
    let Some(first) = words.first() else {
        return;
    };
    if is_assignment(&first.text) {
        return;
    }
    if first.glob || first.text == "!" || is_keyword(&first.text) {
        *complex = true;
    }
}

fn is_option(words: &[Word<'_>], i: usize) -> bool {
    words.get(i).is_some_and(|w| w.text.starts_with('-'))
}

fn wrapper_len(words: &[Word<'_>], complex: &mut bool) -> usize {
    let mut i = 0;
    while let Some(word) = words.get(i) {
        let text = &*word.text;
        if is_assignment(text) {
            if is_risky_assignment(text) {
                *complex = true;
            }
            i += 1;
            continue;
        }
        match program_name(text) {
            "nohup" => i += 1,
            "command" => {
                i += 1;
                while is_option(words, i) {
                    i += 1;
                }
            }
            "time" => {
                i += 1;
                while is_option(words, i) {
                    let opt = &*words[i].text;
                    i += 1;
                    if matches!(opt, "-o" | "-f" | "--output" | "--format") {
                        i += 1;
                    }
                }
            }
            "timeout" => {
                i += 1;
                while is_option(words, i) {
                    let opt = &*words[i].text;
                    i += 1;
                    if matches!(opt, "-s" | "-k" | "--signal" | "--kill-after") {
                        i += 1;
                    }
                }
                i += 1;
            }
            "nice" => {
                i += 1;
                while is_option(words, i) {
                    let opt = &*words[i].text;
                    i += 1;
                    if matches!(opt, "-n" | "--adjustment") {
                        i += 1;
                    }
                }
            }
            "stdbuf" => {
                i += 1;
                while is_option(words, i) {
                    let opt = &*words[i].text;
                    i += 1;
                    if matches!(opt, "-o" | "-e" | "-i" | "--output" | "--error" | "--input") {
                        i += 1;
                    }
                }
            }
            "env" => {
                i += 1;
                while is_option(words, i) {
                    let opt = &*words[i].text;
                    if opt.starts_with("-S") || opt.starts_with("--split-string") {
                        *complex = true;
                        return words.len().min(i);
                    }
                    i += 1;
                    if matches!(
                        opt,
                        "-u" | "-C" | "-a" | "-P" | "--unset" | "--chdir" | "--argv0"
                    ) {
                        i += 1;
                    }
                }
            }
            _ => break,
        }
    }
    i.min(words.len())
}

fn is_shell(text: &str) -> bool {
    matches!(program_name(text), "bash" | "sh" | "zsh")
}

fn is_c_flag(text: &str) -> bool {
    text.starts_with('-') && !text.starts_with("--") && text.contains('c')
}

fn own<'a>(part: Part<'_>) -> Part<'a> {
    Part {
        words: part
            .words
            .into_iter()
            .map(|w| Cow::Owned(w.into_owned()))
            .collect(),
        reads: part
            .reads
            .into_iter()
            .map(|w| Cow::Owned(w.into_owned()))
            .collect(),
        writes: part
            .writes
            .into_iter()
            .map(|w| Cow::Owned(w.into_owned()))
            .collect(),
    }
}

fn finish<'a>(
    parts: &mut Vec<Part<'a>>,
    cur: &mut Builder<'a>,
    complex: &mut bool,
    unknown: &mut bool,
    unwrap: bool,
) {
    let Builder {
        mut words,
        writes,
        reads,
    } = std::mem::take(cur);
    check_head(&words, complex);
    let drop = wrapper_len(&words, complex);
    words.drain(..drop);
    check_head(&words, complex);
    if words.is_empty() && writes.is_empty() && reads.is_empty() {
        return;
    }
    if let Some(first) = words.first()
        && (first.glob || first.text.is_empty() || first.text.contains(['$', '`']))
    {
        *unknown = true;
    }

    if let Some(first) = words.first()
        && is_shell(&first.text)
        && words.iter().skip(1).any(|w| is_c_flag(&w.text))
    {
        // `sh -c -- script`: `--` ends the options.
        let at = if words.get(2).is_some_and(|w| w.text == "--") {
            3
        } else {
            2
        };
        let literal = unwrap && words.len() > at && matches!(&*words[1].text, "-c" | "-lc");
        if !literal {
            *complex = true;
        } else {
            match &words[at].text {
                Cow::Borrowed(script) => {
                    let inner = parse_at(script, false);
                    *complex |= inner.complex;
                    *unknown |= inner.unknown_program;
                    parts.extend(inner.parts);
                }
                Cow::Owned(script) => {
                    let inner = parse_at(script, false);
                    *complex |= inner.complex;
                    *unknown |= inner.unknown_program;
                    parts.extend(inner.parts.into_iter().map(own));
                }
            }
            if !writes.is_empty() || !reads.is_empty() {
                parts.push(Part {
                    words: Vec::new(),
                    writes,
                    reads,
                });
            }
            return;
        }
    }

    let mut writes = writes;
    let mut after_tee = false;
    let mut options_done = false;
    for word in &words {
        if !after_tee {
            if program_name(&word.text) == "tee" {
                after_tee = true;
                options_done = false;
            }
        } else if !options_done && word.text == "--" {
            options_done = true;
        } else if options_done || !word.text.starts_with('-') || word.text.len() == 1 {
            writes.push(word.text.clone());
        }
    }
    parts.push(Part {
        words: words.into_iter().map(|w| w.text).collect(),
        writes,
        reads,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(part: &Part<'_>) -> Vec<String> {
        part.words.iter().map(|w| w.to_string()).collect()
    }

    fn writes(part: &Part<'_>) -> Vec<String> {
        part.writes.iter().map(|w| w.to_string()).collect()
    }

    fn all_words(text: &str) -> Vec<Vec<String>> {
        parse(text).parts.iter().map(words).collect()
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_on_every_operator() {
        let c = parse("a && b || c ; d | e |& f & g");
        assert_eq!(c.parts.len(), 7);
        assert!(!c.complex);
        let c = parse("a && b || c\nd | e |& f & g");
        assert_eq!(c.parts.len(), 7);
        assert_eq!(parse("a\nb").parts.len(), 2);
    }

    #[test]
    fn quotes_and_escapes() {
        let c = parse(r#"git commit -m 'a b' "c d" e\ f"#);
        assert!(!c.complex);
        assert_eq!(
            words(&c.parts[0]),
            v(&["git", "commit", "-m", "a b", "c d", "e f"])
        );
    }

    #[test]
    fn double_quotes_with_a_dollar_are_complex() {
        assert!(parse(r#"echo "a $HOME""#).complex);
        assert!(parse("echo \"a `x`\"").complex);
        assert!(!parse(r#"echo "a \$HOME""#).complex);
        assert!(parse("echo $HOME").complex);
        assert!(!parse("echo '$HOME'").complex);
    }

    #[test]
    fn substitution_and_subshells_are_complex() {
        for text in [
            "echo $(cat x)",
            "echo `cat x`",
            "(cd x)",
            "{ a; }",
            "cat <(x)",
            "tee >(x)",
        ] {
            assert!(parse(text).complex, "{text}");
        }
        assert!(
            all_words("{ git push; }")
                .iter()
                .any(|w| *w == v(&["git", "push"]))
        );
    }

    #[test]
    fn heredoc_is_complex() {
        assert!(parse("cat <<EOF\nhi\nEOF").complex);
        assert!(parse("cat <<< hi").complex);
    }

    #[test]
    fn control_keywords_are_complex() {
        assert!(parse("if true; then x; fi").complex);
        assert!(parse("for i in a; do x; done").complex);
        assert!(parse("while true; do x; done").complex);
        assert!(!parse("echo if").complex);
    }

    #[test]
    fn a_glob_in_the_program_is_complex() {
        assert!(parse("/bin/r*m x").complex);
        assert!(!parse("ls *.rs").complex);
    }

    #[test]
    fn redirect_targets_are_writes() {
        let c = parse("echo x > a");
        assert_eq!(writes(&c.parts[0]), v(&["a"]));
        assert_eq!(words(&c.parts[0]), v(&["echo", "x"]));
        assert_eq!(writes(&parse("echo x >> b").parts[0]), v(&["b"]));
        assert_eq!(writes(&parse("echo x 2> c").parts[0]), v(&["c"]));
        assert_eq!(writes(&parse("echo x &> d").parts[0]), v(&["d"]));
        assert_eq!(writes(&parse("echo x >e").parts[0]), v(&["e"]));
        let c = parse("cat < f");
        assert!(c.parts[0].writes.is_empty());
        assert_eq!(words(&c.parts[0]), v(&["cat"]));
        let c = parse("echo x 2>&1");
        assert!(c.parts[0].writes.is_empty());
        assert_eq!(words(&c.parts[0]), v(&["echo", "x"]));
        assert!(!c.complex);
    }

    #[test]
    fn tee_targets_are_writes() {
        let c = parse("echo x | tee -a log out");
        assert_eq!(writes(&c.parts[1]), v(&["log", "out"]));
        assert_eq!(words(&c.parts[1]), v(&["tee", "-a", "log", "out"]));
        assert!(c.parts[0].writes.is_empty());
    }

    #[test]
    fn wrappers_are_dropped() {
        let c = parse("FOO=1 nohup timeout -s KILL 60 nice -n 5 env -i A=b git push");
        assert!(!c.complex);
        assert_eq!(c.parts.len(), 1);
        assert_eq!(words(&c.parts[0]), v(&["git", "push"]));
        assert_eq!(
            all_words("time command stdbuf -oL git push"),
            [v(&["git", "push"])]
        );
        assert_eq!(
            all_words("/usr/bin/git push"),
            [v(&["/usr/bin/git", "push"])]
        );
    }

    #[test]
    fn sh_c_is_unwrapped_once() {
        let c = parse("sh -c 'git push && rm x'");
        assert!(!c.complex);
        assert_eq!(
            c.parts.iter().map(words).collect::<Vec<_>>(),
            [v(&["git", "push"]), v(&["rm", "x"])]
        );
        let c = parse(r#"bash -lc "git push""#);
        assert!(!c.complex);
        assert_eq!(
            c.parts.iter().map(words).collect::<Vec<_>>(),
            [v(&["git", "push"])]
        );
        let c = parse(r#"sh -c "sh -c 'x'""#);
        assert!(c.complex);
        assert!(parse("sh -xc 'git push'").complex);
        assert!(parse("sh -c 'echo $HOME'").complex);
        assert!(!parse("sh script.sh").complex);
    }

    #[test]
    fn unterminated_quote_is_complex() {
        assert!(parse("echo 'a").complex);
        assert!(parse("echo \"a").complex);
    }

    #[test]
    fn program_name_strips_the_path() {
        assert_eq!(program_name("/usr/bin/git"), "git");
        assert_eq!(program_name("git"), "git");
        assert_eq!(program_name("a/"), "");
    }

    #[test]
    fn an_empty_line_has_no_parts() {
        for text in ["", "   ", "\n\n", " ; "] {
            let c = parse(text);
            assert!(c.parts.is_empty(), "{text:?}");
            assert!(!c.complex);
        }
    }

    #[test]
    fn input_redirects_are_reads() {
        let c = parse("cat < f");
        assert_eq!(c.parts[0].reads, [Cow::Borrowed("f")]);
        assert!(c.parts[0].writes.is_empty());
        let c = parse("cat <> g");
        assert_eq!(c.parts[0].reads, [Cow::Borrowed("g")]);
        assert_eq!(writes(&c.parts[0]), v(&["g"]));
        assert!(parse("cat <&0").parts[0].reads.is_empty());
        assert!(parse("cat <<EOF").complex);
        assert!(parse("echo x > a").parts[0].reads.is_empty());
    }

    #[test]
    fn tee_after_a_runner_is_a_write() {
        assert_eq!(
            writes(&parse("sudo tee secrets/k").parts[0]),
            v(&["secrets/k"])
        );
        assert_eq!(writes(&parse("xargs tee -a k").parts[0]), v(&["k"]));
        assert_eq!(writes(&parse("x tee -- -f").parts[0]), v(&["-f"]));
    }

    #[test]
    fn time_and_env_options_consume_their_value() {
        assert_eq!(all_words("time -o git rm x"), [v(&["rm", "x"])]);
        assert_eq!(all_words("time -f %e rm x"), [v(&["rm", "x"])]);
        assert_eq!(
            all_words("env -a name -P /bin -C d -u V rm x"),
            [v(&["rm", "x"])]
        );
        assert!(parse("env -S 'git push'").complex);
    }

    #[test]
    fn risky_assignments_are_complex() {
        for text in [
            "PATH=/x git push",
            "LD_PRELOAD=x git push",
            "DYLD_INSERT_LIBRARIES=x ls",
            "BASH_ENV=x ls",
            "IFS=x ls",
            "GIT_SSH_COMMAND=x git push",
            "env PATH=/x ls",
        ] {
            assert!(parse(text).complex, "{text}");
        }
        let c = parse("FOO=1 git push");
        assert!(!c.complex);
        assert_eq!(words(&c.parts[0]), v(&["git", "push"]));
    }

    #[test]
    fn unknown_programs_are_flagged() {
        for text in [
            "$x push",
            "/usr/bin/gi[t] push",
            "$'\\x67it' push",
            "`echo git` push",
            "ls && $x",
            "sh -c '$x push'",
        ] {
            assert!(parse(text).unknown_program, "{text}");
        }
        assert!(!parse("git push $x").unknown_program);
        assert!(!parse("ls *.rs").unknown_program);
    }

    #[test]
    fn shell_text_detects_scripts() {
        assert!(shell_text("git push"));
        assert!(shell_text("a;b"));
        assert!(shell_text("a|b"));
        assert!(shell_text("a&b"));
        assert!(!shell_text("main.rs"));
        assert!(!shell_text("--flag=x"));
    }
}

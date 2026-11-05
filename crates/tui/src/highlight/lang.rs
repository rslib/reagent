//! The language table, one static entry per language, and the lookups over it.

/// A language, by its index in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang(u8);

/// How the lines of a language are split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    /// Code: comments, strings, numbers, words and punctuation.
    Code,
    /// A unified diff.
    Diff,
    /// JSON.
    Json,
    /// TOML.
    Toml,
    /// YAML.
    Yaml,
}

/// Everything the tokenizer knows about one language.
#[derive(Debug)]
pub(super) struct Language {
    /// The name [`Lang::name`] returns.
    pub(super) name: &'static str,
    /// The fence tags that select it.
    pub(super) tags: &'static [&'static str],
    /// How its lines are split.
    pub(super) flavor: Flavor,
    /// Reserved words, sorted.
    pub(super) keywords: &'static [&'static str],
    /// Built-in type names, sorted.
    pub(super) types: &'static [&'static str],
    /// Markers that comment out the rest of a line.
    pub(super) line_comment: &'static [&'static str],
    /// The opening and the closing marker of a block comment.
    pub(super) block_comment: Option<(&'static str, &'static str)>,
    /// String delimiters, a longer one first.
    pub(super) strings: &'static [&'static str],
    /// `word!` is a macro.
    pub(super) macros: bool,
    /// `word(` is a call.
    pub(super) calls: bool,
    /// A word that starts uppercase is a type.
    pub(super) caps: bool,
    /// `'a` is a lifetime rather than a char literal.
    pub(super) lifetimes: bool,
    /// `$NAME` and `${..}` are types.
    pub(super) vars: bool,
    /// The first word of a line is a command.
    pub(super) command: bool,
    /// Keywords match without case.
    pub(super) icase: bool,
}

impl Lang {
    /// The language of a fence tag, without case.
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let index = LANGS.iter().position(|lang| {
            lang.tags
                .iter()
                .any(|known| known.eq_ignore_ascii_case(tag))
        })?;
        Some(Lang(u8::try_from(index).expect("the table is small")))
    }

    /// The language's name.
    pub fn name(&self) -> &'static str {
        self.table().name
    }

    /// The table entry this language names.
    pub(super) fn table(&self) -> &'static Language {
        &LANGS[self.0 as usize]
    }
}

/// Whether a sorted list holds `word`.
pub(super) fn contains(list: &[&str], word: &str) -> bool {
    list.binary_search(&word).is_ok()
}

/// Whether a sorted list holds `word`, ignoring ASCII case.
pub(super) fn contains_ignoring_case(list: &[&str], word: &str) -> bool {
    list.binary_search_by(|entry| {
        entry
            .bytes()
            .map(|b| b.to_ascii_lowercase())
            .cmp(word.bytes().map(|b| b.to_ascii_lowercase()))
    })
    .is_ok()
}

/// Every language, for the tests that check the table.
#[cfg(test)]
pub(super) fn languages() -> &'static [Language] {
    &LANGS
}

/// Nothing a language does beyond plain code.
const PLAIN: Language = Language {
    name: "",
    tags: &[],
    flavor: Flavor::Code,
    keywords: &[],
    types: &[],
    line_comment: &[],
    block_comment: None,
    strings: &[],
    macros: false,
    calls: false,
    caps: false,
    lifetimes: false,
    vars: false,
    command: false,
    icase: false,
};

const RUST: Language = Language {
    name: "rust",
    tags: &["rust", "rs"],
    keywords: &[
        "Self", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
        "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i128", "i16", "i32", "i64", "i8", "isize", "str", "u128",
        "u16", "u32", "u64", "u8", "usize",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["\""],
    macros: true,
    calls: true,
    caps: true,
    lifetimes: true,
    ..PLAIN
};

const PYTHON: Language = Language {
    name: "python",
    tags: &["py", "python"],
    keywords: &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ],
    types: &[
        "bool", "bytes", "dict", "float", "int", "list", "set", "str", "tuple",
    ],
    line_comment: &["#"],
    strings: &["\"\"\"", "\"", "'"],
    calls: true,
    ..PLAIN
};

const JAVASCRIPT: Language = Language {
    name: "javascript",
    tags: &["js", "javascript"],
    keywords: &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "null",
        "of",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["`", "\"", "'"],
    calls: true,
    ..PLAIN
};

const TYPESCRIPT: Language = Language {
    name: "typescript",
    tags: &["ts", "tsx"],
    keywords: &[
        "abstract",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "namespace",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["`", "\"", "'"],
    calls: true,
    caps: true,
    ..PLAIN
};

const GO: Language = Language {
    name: "go",
    tags: &["go"],
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ],
    types: &[
        "bool",
        "byte",
        "complex128",
        "complex64",
        "error",
        "float32",
        "float64",
        "int",
        "int16",
        "int32",
        "int64",
        "int8",
        "rune",
        "string",
        "uint",
        "uint16",
        "uint32",
        "uint64",
        "uint8",
        "uintptr",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["`", "\"", "'"],
    calls: true,
    caps: true,
    ..PLAIN
};

const C: Language = Language {
    name: "c",
    tags: &["c", "h"],
    keywords: &[
        "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else",
        "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long", "register",
        "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
        "unsigned", "void", "volatile", "while",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["\"", "'"],
    calls: true,
    ..PLAIN
};

const CPP: Language = Language {
    name: "cpp",
    tags: &["cpp", "c++"],
    keywords: &[
        "alignas",
        "alignof",
        "auto",
        "bool",
        "break",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "constexpr",
        "continue",
        "default",
        "delete",
        "do",
        "double",
        "else",
        "enum",
        "explicit",
        "extern",
        "false",
        "float",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "namespace",
        "new",
        "nullptr",
        "operator",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "typename",
        "union",
        "unsigned",
        "using",
        "virtual",
        "void",
        "volatile",
        "while",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["\"", "'"],
    calls: true,
    caps: true,
    ..PLAIN
};

const JAVA: Language = Language {
    name: "java",
    tags: &["java"],
    keywords: &[
        "abstract",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "false",
        "final",
        "finally",
        "float",
        "for",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "throws",
        "true",
        "try",
        "void",
        "while",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &["\"", "'"],
    calls: true,
    caps: true,
    ..PLAIN
};

const SHELL: Language = Language {
    name: "shell",
    tags: &["sh", "bash", "zsh", "shell", "console"],
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
        "in", "local", "return", "select", "then", "until", "while",
    ],
    line_comment: &["#"],
    strings: &["`", "\"", "'"],
    vars: true,
    command: true,
    ..PLAIN
};

const JSON: Language = Language {
    name: "json",
    tags: &["json"],
    flavor: Flavor::Json,
    keywords: &["false", "null", "true"],
    strings: &["\""],
    ..PLAIN
};

const TOML: Language = Language {
    name: "toml",
    tags: &["toml"],
    flavor: Flavor::Toml,
    keywords: &["false", "true"],
    line_comment: &["#"],
    strings: &["\"", "'"],
    ..PLAIN
};

const YAML: Language = Language {
    name: "yaml",
    tags: &["yaml", "yml"],
    flavor: Flavor::Yaml,
    keywords: &["false", "null", "true"],
    line_comment: &["#"],
    strings: &["\"", "'"],
    ..PLAIN
};

const SQL: Language = Language {
    name: "sql",
    tags: &["sql"],
    keywords: &[
        "and", "as", "asc", "by", "case", "create", "delete", "desc", "distinct", "drop", "else",
        "end", "from", "group", "having", "in", "index", "inner", "insert", "into", "is", "join",
        "left", "like", "limit", "not", "null", "on", "or", "order", "outer", "select", "set",
        "table", "then", "union", "update", "values", "when", "where",
    ],
    types: &[
        "bigint",
        "boolean",
        "date",
        "integer",
        "numeric",
        "text",
        "timestamp",
        "varchar",
    ],
    line_comment: &["--"],
    block_comment: Some(("/*", "*/")),
    strings: &["\"", "'"],
    calls: true,
    icase: true,
    ..PLAIN
};

const RUBY: Language = Language {
    name: "ruby",
    tags: &["rb", "ruby"],
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else",
        "elsif", "end", "ensure", "false", "for", "if", "in", "module", "next", "nil", "not", "or",
        "raise", "rescue", "return", "self", "super", "then", "true", "unless", "until", "when",
        "while", "yield",
    ],
    types: &["Array", "Hash", "Integer", "String", "Symbol"],
    line_comment: &["#"],
    strings: &["\"", "'"],
    calls: true,
    ..PLAIN
};

const LUA: Language = Language {
    name: "lua",
    tags: &["lua"],
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ],
    line_comment: &["--"],
    block_comment: Some(("--[[", "]]")),
    strings: &["\"", "'"],
    calls: true,
    ..PLAIN
};

const DIFF: Language = Language {
    name: "diff",
    tags: &["diff", "patch"],
    flavor: Flavor::Diff,
    ..PLAIN
};

/// One entry per language, in the order [`Lang`] indexes them.
static LANGS: [Language; 16] = [
    RUST, PYTHON, JAVASCRIPT, TYPESCRIPT, GO, C, CPP, JAVA, SHELL, JSON, TOML, YAML, SQL, RUBY,
    LUA, DIFF,
];

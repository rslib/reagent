//! The `permissions` command: the rules that apply in a folder with the file
//! each comes from, and the removal of a saved one.
//!
//! The config files are read in `spawn_blocking`, as the other subcommands do.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use reagent_config::permissions::{Kind, Rules};
use reagent_config::{Layers, auth, load_for, saved};
use serde::Serialize;

/// The usage of the command, printed by `permissions --help`.
const USAGE: &str = concat!(
    "Usage:\n",
    "  reagent permissions [--json] [--profile <p>] [--cwd <dir>]\n",
    "  reagent permissions remove <rule> [--profile <p>] [--cwd <dir>]\n",
    "\n",
    "Every rule that applies in the folder is listed with its kind, its text\n",
    "and the file it comes from, then the saved-rules file of the project.\n",
    "`remove` takes a rule out of that file; a rule of another config file is\n",
    "refused, and the file that holds it is named.\n",
);

/// Runs one `permissions` command and returns the process exit code.
pub async fn run(args: Vec<OsString>) -> ExitCode {
    let options = match Options::parse(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(options)) => options,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };
    let cwd = canonical(&options.cwd).await;
    // The profile a run reads: the flag, or else `REAGENT_PROFILE`.
    let profile = crate::args::profile_from(options.profile, std::env::var("REAGENT_PROFILE").ok());
    match options.remove {
        Some(rule) => remove(&cwd, profile.as_deref(), &rule).await,
        None => list(&cwd, profile.as_deref(), options.json).await,
    }
}

/// What the words of the command ask for.
struct Options {
    json: bool,
    profile: Option<String>,
    cwd: PathBuf,
    /// The rule to remove, when the words name one.
    remove: Option<String>,
}

/// The command as the parser read it.
enum Parsed {
    Run(Options),
    Help,
}

impl Options {
    /// Reads the flags and the `remove <rule>` words. A flag this command does
    /// not have, and a word in the wrong place, are errors.
    fn parse(args: Vec<OsString>) -> Result<Parsed, String> {
        let mut parser = lexopt::Parser::from_args(args);
        let mut options = Options {
            json: false,
            profile: None,
            cwd: PathBuf::from("."),
            remove: None,
        };
        // Whether the `remove` word came, and how many words came so far.
        let mut remove = false;
        let mut words = 0;
        while let Some(arg) = parser.next().map_err(|error| error.to_string())? {
            match arg {
                lexopt::Arg::Short('h') | lexopt::Arg::Long("help") => return Ok(Parsed::Help),
                lexopt::Arg::Long("json") => options.json = true,
                lexopt::Arg::Long("profile") => {
                    let value = parser
                        .value()
                        .map_err(|_| "missing value for --profile".to_string())?;
                    options.profile = Some(
                        value
                            .into_string()
                            .map_err(|_| "--profile needs valid UTF-8".to_string())?,
                    );
                }
                lexopt::Arg::Long("cwd") => {
                    let value = parser
                        .value()
                        .map_err(|_| "missing value for --cwd".to_string())?;
                    options.cwd = PathBuf::from(value);
                }
                lexopt::Arg::Value(value) => {
                    let value = value
                        .into_string()
                        .map_err(|_| "the rule needs valid UTF-8".to_string())?;
                    match (words, remove) {
                        (0, _) if value == "remove" => remove = true,
                        (1, true) => options.remove = Some(value),
                        (0, _) => return Err(format!("unknown permissions command: {value}")),
                        _ => return Err(format!("unexpected argument: {value}")),
                    }
                    words += 1;
                }
                other => return Err(format!("unknown flag: {}", flag_text(other))),
            }
        }
        Ok(Parsed::Run(options))
    }
}

/// How an argument the command does not have is named in the error.
fn flag_text(arg: lexopt::Arg) -> String {
    match arg {
        lexopt::Arg::Short(name) => format!("-{name}"),
        lexopt::Arg::Long(name) => format!("--{name}"),
        other => format!("{other:?}"),
    }
}

/// `path` as the tools resolve it, so a symlinked `--cwd` anchors the rules
/// where they run.
async fn canonical(path: &Path) -> PathBuf {
    match tokio::fs::canonicalize(path).await {
        Ok(path) => path,
        Err(_) => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// Prints every rule that applies in `cwd`, as lines or as one JSON object.
async fn list(cwd: &Path, profile: Option<&str>, json: bool) -> ExitCode {
    let dir = cwd.to_path_buf();
    let profile = profile.map(str::to_owned);
    let listed = tokio::task::spawn_blocking(move || {
        let layers = load_for(&dir, profile.as_deref())?;
        let rules = rules_of(&layers, &dir)?;
        let entries = rules
            .entries()
            .map(|(kind, rule, file)| Entry {
                kind: kind_of(kind),
                rule: rule.to_owned(),
                file: file.to_owned(),
            })
            .collect();
        let saved = saved_file(&dir)?;
        Ok::<_, String>(Listing { entries, saved })
    })
    .await;
    let listing = match listed {
        Ok(Ok(listing)) => listing,
        Ok(Err(message)) => return config_error(&message),
        Err(_) => return config_error("the config task failed"),
    };
    if json {
        match serde_json::to_string(&listing.json()) {
            Ok(text) => println!("{text}"),
            Err(error) => return config_error(&error.to_string()),
        }
    } else {
        print!("{}", listing.text());
    }
    ExitCode::SUCCESS
}

/// Removes the rule `rule` from the saved-rules file of `cwd`, or names the
/// other file that holds it, or says that no file does.
async fn remove(cwd: &Path, profile: Option<&str>, rule: &str) -> ExitCode {
    let dir = cwd.to_path_buf();
    let profile = profile.map(str::to_owned);
    let rule = rule.trim().to_owned();
    let outcome =
        tokio::task::spawn_blocking(move || remove_rule(&dir, profile.as_deref(), &rule)).await;
    match outcome {
        Ok(Ok(line)) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Ok(Err(Refusal::Config(message))) => config_error(&message),
        Ok(Err(Refusal::Rule(message))) => {
            eprintln!("reagent: {message}");
            ExitCode::from(1)
        }
        Err(_) => config_error("the config task failed"),
    }
}

/// Why a removal did not happen: a config problem, or a rule that the saved
/// file cannot lose.
enum Refusal {
    Config(String),
    Rule(String),
}

/// The removal itself, on the blocking thread. Returns the line to print when
/// the file changed.
fn remove_rule(cwd: &Path, profile: Option<&str>, rule: &str) -> Result<String, Refusal> {
    let file = saved_file(cwd).map_err(Refusal::Config)?;
    if saved::remove(&file, rule).map_err(Refusal::Config)? {
        return Ok(format!("removed {rule}"));
    }
    let layers = load_for(cwd, profile).map_err(Refusal::Config)?;
    match holder(&layers, rule) {
        Some(file) => Err(Refusal::Rule(format!(
            "{rule} comes from {file}; edit that file to remove it"
        ))),
        None => Err(Refusal::Rule(format!("no saved rule {rule}"))),
    }
}

/// The file of `layers` whose lists hold the rule `rule`.
fn holder(layers: &Layers, rule: &str) -> Option<String> {
    layers
        .ordered()
        .into_iter()
        .find(|layer| {
            let lists = &layer.settings.permissions;
            lists
                .allow
                .iter()
                .chain(&lists.ask)
                .chain(&lists.deny)
                .any(|text| text.trim() == rule)
        })
        .map(|layer| layer.path.display().to_string())
}

/// The saved-rules file of the project of `cwd`.
fn saved_file(cwd: &Path) -> Result<PathBuf, String> {
    Ok(saved::path(&auth::dir()?, cwd))
}

/// The rules every layer holds, anchored as `args.rs` anchors them.
fn rules_of(layers: &Layers, cwd: &Path) -> Result<Rules, String> {
    let home = std::env::var_os("HOME").map(|home| canonical_sync(Path::new(&home)));
    Rules::from_layers(&layers.ordered(), cwd, home.as_deref())
}

#[expect(
    clippy::disallowed_methods,
    reason = "the caller runs it inside spawn_blocking"
)]
fn canonical_sync(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The kind of a rule as the command writes it.
fn kind_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Allow => "allow",
        Kind::Ask => "ask",
        Kind::Deny => "deny",
    }
}

/// The rules that apply in a folder, and the saved-rules file of the project.
struct Listing {
    entries: Vec<Entry>,
    saved: PathBuf,
}

/// One rule as the command shows it.
struct Entry {
    kind: &'static str,
    rule: String,
    file: String,
}

impl Listing {
    /// One rule per line with the kinds padded to one width, then the saved
    /// file, which is printed even when it does not exist yet.
    fn text(&self) -> String {
        let width = self
            .entries
            .iter()
            .map(|entry| entry.kind.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&format!(
                "{:width$}  {}  {}\n",
                entry.kind,
                entry.rule,
                entry.file,
                width = width
            ));
        }
        out.push_str(&format!("saved rules: {}\n", self.saved.display()));
        out
    }

    /// The same rules as one JSON object.
    fn json(&self) -> Json<'_> {
        Json {
            rules: self
                .entries
                .iter()
                .map(|entry| JsonRule {
                    kind: entry.kind,
                    rule: &entry.rule,
                    file: &entry.file,
                })
                .collect(),
            saved: self.saved.display().to_string(),
        }
    }
}

/// The `--json` answer: every rule with its file, and the saved-rules file.
#[derive(Serialize)]
struct Json<'a> {
    rules: Vec<JsonRule<'a>>,
    saved: String,
}

/// One rule of the `--json` answer.
#[derive(Serialize)]
struct JsonRule<'a> {
    kind: &'a str,
    rule: &'a str,
    file: &'a str,
}

/// A config problem: exit 2 with the message.
fn config_error(message: &str) -> ExitCode {
    eprintln!("reagent: {message}");
    ExitCode::from(2)
}

//! End-to-end tests for the `permissions` command.
//!
//! Each test runs the binary with a temp config, state and working folder, so
//! no test reads the real config or saves a rule outside its own temp folder.

#![cfg(feature = "testing")]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// The global ask rule the tests write.
const ASK: &str = "Bash(rm:*)";
/// The rule the tests save.
const SAVED: &str = "Bash(cargo test:*)";

/// A temp home the runs read their config and write their saved rules in, and
/// a temp project folder they run in.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// The state folder the runs write their saved rules in.
    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// The global config file a run reads its `[permissions]` table from.
    fn global(&self) -> PathBuf {
        self.root.path().join("config/reagent/config.toml")
    }

    /// The project folder a run sees: a folder with a `.git` in it.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes the folder it runs in"
    )]
    fn cwd(&self) -> PathBuf {
        let cwd = self.root.path().join("work");
        std::fs::create_dir_all(cwd.join(".git")).expect("create the project folder");
        cwd
    }

    /// The saved-rules file of `cwd`, as the binary names it.
    fn saved(&self, cwd: &Path) -> PathBuf {
        reagent_config::saved::path(&self.state().join("reagent"), &canonical(cwd))
    }

    /// Writes the global config file of the isolated home.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own config folder"
    )]
    fn config(&self, text: &str) {
        let dir = self.root.path().join("config/reagent");
        std::fs::create_dir_all(&dir).expect("create the config folder");
        std::fs::write(dir.join("config.toml"), text).expect("write the config");
    }

    /// Runs the binary in `cwd`.
    async fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.run_with(cwd, args, &[]).await
    }

    /// Runs the binary in `cwd` with the extra environment `env`.
    async fn run_with(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(BIN)
            .args(args)
            .envs(env.iter().copied())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state())
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .output()
            .await
            .expect("run the binary")
    }

    /// The one JSON object `permissions --json` prints.
    async fn json(&self, cwd: &Path) -> Value {
        let out = self.run(cwd, &["permissions", "--json"]).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("one json object")
    }
}

/// The folder a run sees, resolved as the binary resolves it.
#[expect(
    clippy::disallowed_methods,
    reason = "a test names the folder it runs in"
)]
fn canonical(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).expect("canonical folder")
}

/// The rule entries of a `--json` answer, as `(kind, rule, file)`.
fn rules(json: &Value) -> Vec<(String, String, String)> {
    json["rules"]
        .as_array()
        .expect("a rule list")
        .iter()
        .map(|rule| {
            let field = |name: &str| {
                rule[name]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} in {rule}"))
                    .to_string()
            };
            (field("kind"), field("rule"), field("file"))
        })
        .collect()
}

#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test writes its own profile file"
)]
async fn the_profile_of_reagent_profile_is_listed() {
    let home = Home::new();
    let cwd = home.cwd();
    let profiles = home.root.path().join("config/reagent/profiles");
    std::fs::create_dir_all(&profiles).expect("create the profiles folder");
    std::fs::write(
        profiles.join("work.toml"),
        "[permissions]\ndeny = [\"Bash(git push:*)\"]\n",
    )
    .expect("write the profile");
    let out = home
        .run_with(&cwd, &["permissions"], &[("REAGENT_PROFILE", "work")])
        .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.lines()
            .any(|line| line.starts_with("deny ") && line.contains("Bash(git push:*)")),
        "a run with REAGENT_PROFILE reads the profile, so the list shows it: {text}"
    );
}

#[tokio::test]
async fn a_global_ask_rule_and_a_saved_allow_rule_are_listed() {
    let home = Home::new();
    let cwd = home.cwd();
    home.config(&format!("[permissions]\nask = [\"{ASK}\"]\n"));
    let saved = home.saved(&cwd);
    assert!(
        reagent_config::saved::add(&saved, SAVED).expect("saves the rule"),
        "{}",
        saved.display()
    );

    let out = home.run(&cwd, &["permissions"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    // The layers come in their precedence order: the global file first, then
    // the saved rules of the project.
    assert!(lines[0].starts_with("ask "), "{text}");
    assert!(lines[0].contains(ASK), "{text}");
    assert!(
        lines[0].contains(&home.global().display().to_string()),
        "{text}"
    );
    assert!(lines[1].starts_with("allow "), "{text}");
    assert!(lines[1].contains(SAVED), "{text}");
    assert!(lines[1].ends_with(&saved.display().to_string()), "{text}");
    // The kinds are padded to one width.
    assert_eq!(lines[0].find(ASK), lines[1].find(SAVED), "{text}");
    assert_eq!(lines[2], format!("saved rules: {}", saved.display()));

    let json = home.json(&cwd).await;
    assert_eq!(
        rules(&json),
        [
            (
                "ask".to_string(),
                ASK.to_string(),
                home.global().display().to_string()
            ),
            (
                "allow".to_string(),
                SAVED.to_string(),
                saved.display().to_string()
            ),
        ]
    );
    assert_eq!(json["saved"], Value::from(saved.display().to_string()));
}

#[tokio::test]
async fn a_removed_saved_rule_is_gone_from_the_next_list() {
    let home = Home::new();
    let cwd = home.cwd();
    let saved = home.saved(&cwd);
    reagent_config::saved::add(&saved, SAVED).expect("saves the rule");

    let out = home.run(&cwd, &["permissions", "remove", SAVED]).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("removed {SAVED}")
    );
    // The file keeps its place with no rule in it, and the list is empty.
    let layer = reagent_config::saved::load(&saved)
        .expect("loads the saved file")
        .expect("the file stays");
    assert!(layer.settings.permissions.allow.is_empty());
    let json = home.json(&cwd).await;
    assert!(rules(&json).is_empty(), "{json}");
}

#[tokio::test]
async fn removing_a_rule_of_another_file_names_that_file() {
    let home = Home::new();
    let cwd = home.cwd();
    home.config(&format!("[permissions]\nask = [\"{ASK}\"]\n"));

    let out = home.run(&cwd, &["permissions", "remove", ASK]).await;
    assert_eq!(out.status.code(), Some(1));
    let error = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(error.starts_with("reagent: "), "{error}");
    assert!(error.contains(ASK), "{error}");
    assert!(
        error.contains(&home.global().display().to_string()),
        "{error}"
    );
    // The file that holds the rule is untouched, and the list still has it.
    assert_eq!(rules(&home.json(&cwd).await).len(), 1);
}

#[tokio::test]
async fn a_rule_nobody_saved_is_refused_by_name() {
    let home = Home::new();
    let cwd = home.cwd();

    let out = home
        .run(&cwd, &["permissions", "remove", "Bash(git push:*)"])
        .await;
    assert_eq!(out.status.code(), Some(1));
    let error = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(error.trim(), "reagent: no saved rule Bash(git push:*)");
}

#[tokio::test]
async fn help_prints_a_usage_and_an_unknown_flag_is_refused() {
    let home = Home::new();
    let cwd = home.cwd();

    let out = home.run(&cwd, &["permissions", "--help"]).await;
    assert!(out.status.success());
    let usage = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(usage.contains("reagent permissions"), "{usage}");

    let out = home.run(&cwd, &["permissions", "--nope"]).await;
    assert_eq!(out.status.code(), Some(2));
    let error = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(error.contains("--nope"), "{error}");
}

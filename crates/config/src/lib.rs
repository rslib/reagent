//! Settings read from the global and the repo `config.toml`.

pub mod glob;
pub mod permissions;
pub mod shell;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The settings a config file may hold. A key that is absent keeps the
/// built-in default.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The model text, `name/id` or a bare model id.
    pub model: Option<String>,
    /// The cap on reply tokens.
    pub max_tokens: Option<u32>,
    /// The loop policy names, as written in the file.
    pub policy: Option<Vec<String>>,
    /// Folder names that searches leave out.
    #[serde(default)]
    pub skip: Vec<String>,
    /// The provider tables by name. Only the global file may hold them.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// The `[permissions]` table, as written. A repo file may hold only its
    /// `ask` and `deny` lists.
    #[serde(default)]
    pub permissions: permissions::Lists,
    /// The `[sandbox]` table. Only the global file may hold it.
    #[serde(default)]
    pub sandbox: Sandbox,
}

/// The `[sandbox]` table: how bash commands are fenced in.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// `off` (the default), `auto` or `on`.
    pub mode: Option<String>,
    /// Whether commands may open network connections; default false.
    pub network: Option<bool>,
    /// Folders commands may write besides the working and temp folders.
    #[serde(default)]
    pub write: Vec<String>,
}

/// A secret string. Its `Debug` output hides the text, so settings printed
/// in a log, a panic or an error never show it.
#[derive(Deserialize, Clone, PartialEq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The secret text. Use it only to build a request.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Self {
        Secret(text.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// A `[providers.<name>]` table.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The wire name, as for `--provider`.
    pub wire: Option<String>,
    /// The endpoint URL.
    pub base_url: Option<String>,
    /// The name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// The API key itself. The global file must then be readable by you only.
    pub api_key: Option<Secret>,
    /// Extra request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The per-model tables by model id.
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
}

/// A `[providers.<name>.models."<id>"]` table.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// The cap on reply tokens for this model.
    pub max_tokens: Option<u32>,
    /// The size of the context window in tokens.
    pub context_window: Option<u64>,
    /// The loop policy names for this model.
    pub policy: Option<Vec<String>>,
}

/// The settings of one config file with the file's path.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// The file the settings came from.
    pub path: PathBuf,
    /// The settings read from it.
    pub settings: Settings,
}

/// The nearest folder at or above `cwd` that holds `.git`, a folder or a file.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn global_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|value| !value.is_empty())?).join(".config"),
    };
    Some(base.join("reagent"))
}

/// The global config folder: under `XDG_CONFIG_HOME` when it is set, else
/// under `$HOME/.config`.
pub fn global_dir() -> Option<PathBuf> {
    global_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// The global config file path.
pub fn global_path() -> Option<PathBuf> {
    global_dir().map(|dir| dir.join("config.toml"))
}

/// The text of a file and whether group or others may read it.
struct Text {
    text: String,
    shared: bool,
}

#[expect(
    clippy::disallowed_types,
    reason = "the config is read before the runtime starts"
)]
fn read(path: &Path) -> Result<Option<Text>, String> {
    use std::io::Read;

    let fail = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail(error)),
    };
    #[cfg(unix)]
    let shared = {
        use std::os::unix::fs::PermissionsExt;
        file.metadata().map_err(fail)?.permissions().mode() & 0o077 != 0
    };
    #[cfg(not(unix))]
    let shared = false;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    Ok(Some(Text { text, shared }))
}

/// The parse error with its place and message but not the source line, which
/// could hold an api_key.
fn parse_error(path: &Path, text: &str, error: &toml::de::Error) -> String {
    // An "invalid type" or "invalid value" message quotes the value it got.
    let message = match error.message().split_once(", expected ") {
        Some((head, expected)) if head.starts_with("invalid ") => {
            format!("invalid value, expected {expected}")
        }
        _ => error.message().to_string(),
    };
    let Some(span) = error.span() else {
        return format!("{}: {message}", path.display());
    };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.chars().rev().take_while(|&c| c != '\n').count() + 1;
    format!("{}:{line}:{column}: {message}", path.display())
}

fn load_file(path: Option<&Path>, repo: bool) -> Result<Option<Layer>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(Text { text, shared }) = read(path)? else {
        return Ok(None);
    };
    let settings: Settings =
        toml::from_str(&text).map_err(|error| parse_error(path, &text, &error))?;
    if !repo && shared && settings.providers.values().any(|p| p.api_key.is_some()) {
        return Err(format!(
            "{0}: holds an api_key, so only you may read it; run chmod 600 {0}",
            path.display()
        ));
    }
    if repo && !settings.providers.is_empty() {
        return Err(format!(
            "{}: providers may be set only in the global config",
            path.display()
        ));
    }
    if repo && settings.sandbox != Sandbox::default() {
        return Err(format!(
            "{}: sandbox may be set only in the global config",
            path.display()
        ));
    }
    if repo && (!settings.permissions.allow.is_empty() || settings.permissions.default.is_some()) {
        return Err(format!(
            "{}: permissions.allow and permissions.default may be set only in the global config",
            path.display()
        ));
    }
    Ok(Some(Layer {
        path: path.to_path_buf(),
        settings,
    }))
}

/// Loads the global file and then the repo file for `cwd`.
pub fn load_for(cwd: &Path) -> Result<Vec<Layer>, String> {
    let repo = repo_root(cwd).map(|root| root.join(".reagent").join("config.toml"));
    load(global_path().as_deref(), repo.as_deref())
}

/// Loads the global file and the repo file, in that order. A missing file
/// gives no layer. The repo file may set only `model`, `max_tokens`, `policy`
/// and `skip`.
pub fn load(global: Option<&Path>, repo: Option<&Path>) -> Result<Vec<Layer>, String> {
    Ok([load_file(global, false)?, load_file(repo, true)?]
        .into_iter()
        .flatten()
        .collect())
}

/// Merges layers, a later layer over an earlier one for `model`,
/// `max_tokens` and `policy`. `skip` lists are appended and de-duplicated in
/// order. Providers come from the first layer that holds any.
pub fn merge(layers: &[Layer]) -> Settings {
    let mut merged = Settings::default();
    for layer in layers {
        let settings = &layer.settings;
        merged.model = settings.model.clone().or(merged.model);
        merged.max_tokens = settings.max_tokens.or(merged.max_tokens);
        merged.policy = settings.policy.clone().or(merged.policy);
        for name in &settings.skip {
            if !merged.skip.contains(name) {
                merged.skip.push(name.clone());
            }
        }
        if merged.providers.is_empty() {
            merged.providers = settings.providers.clone();
        }
        if merged.sandbox == Sandbox::default() {
            merged.sandbox = settings.sandbox.clone();
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("makes the folder");
        std::fs::write(&path, text).expect("writes the file");
        path
    }

    fn settings(global: Option<&Path>, repo: Option<&Path>) -> Settings {
        merge(&load(global, repo).expect("loads"))
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmods");
    }

    #[cfg(unix)]
    #[test]
    fn an_api_key_loads_from_a_private_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        chmod(&global, 0o600);
        let merged = settings(Some(&global), None);
        let key = merged.providers["fi"].api_key.as_ref().expect("has a key");
        assert_eq!(key.expose(), "sk-test-123");
    }

    #[cfg(unix)]
    #[test]
    fn an_api_key_in_a_shared_global_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        chmod(&global, 0o644);
        let error = load(Some(&global), None).unwrap_err();
        let path = global.display();
        assert_eq!(
            error,
            format!("{path}: holds an api_key, so only you may read it; run chmod 600 {path}")
        );
        assert!(!error.contains("sk-test-123"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_global_file_without_an_api_key_loads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key_env = \"FI_KEY\"\n",
        );
        chmod(&global, 0o644);
        assert!(load(Some(&global), None).is_ok());
    }

    #[test]
    fn a_repo_file_may_not_hold_an_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(
            dir.path(),
            "repo.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(error.contains("providers"), "{error}");
        assert!(!error.contains("sk-test-123"), "{error}");
    }

    #[test]
    fn the_debug_text_of_an_api_key_is_hidden() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[providers.fi]\napi_key = \"sk-test-123\"\n",
        );
        #[cfg(unix)]
        chmod(&global, 0o600);
        let merged = settings(Some(&global), None);
        let text = format!("{merged:?}");
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("sk-test-123"), "{text}");
    }

    #[test]
    fn a_parse_error_never_shows_the_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        for text in [
            "model = \"m\"\n[providers.fi]\napi_key = sk-test-SECRET\n",
            "[providers.fi]\napi_key = \"sk-test-SECRET\n",
            "[providers.fi]\napi_key = 12345678\n",
        ] {
            let global = write(dir.path(), "g.toml", text);
            let error = load(Some(&global), None).unwrap_err();
            assert!(!error.contains("SECRET"), "{error}");
            assert!(!error.contains("12345678"), "{error}");
            assert!(error.contains("g.toml:"), "{error}");
            assert!(error.chars().any(|c| c.is_ascii_digit()), "{error}");
        }
    }

    #[test]
    fn a_parse_error_names_the_line_and_the_secret_free_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "model = \"m\"\nmodle = \"x\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("g.toml:2:1"), "{error}");
        assert!(error.contains("modle"), "{error}");
    }

    #[test]
    fn a_global_file_sets_the_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "model = \"glm\"\n");
        assert_eq!(settings(Some(&global), None).model.as_deref(), Some("glm"));
    }

    #[test]
    fn the_repo_file_beats_the_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            "model = \"glm\"\nmax_tokens = 100\n[providers.fi]\nwire = \"openai-chat\"\n",
        );
        let repo = write(dir.path(), "repo.toml", "model = \"kimi\"\n");
        let merged = settings(Some(&global), Some(&repo));
        assert_eq!(merged.model.as_deref(), Some("kimi"));
        assert_eq!(merged.max_tokens, Some(100));
        assert_eq!(merged.providers["fi"].wire.as_deref(), Some("openai-chat"));
    }

    #[test]
    fn skip_lists_add_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "skip = [\"a\", \"b\", \"a\"]\n");
        let repo = write(dir.path(), "repo.toml", "skip = [\"b\", \"c\"]\n");
        assert_eq!(
            settings(Some(&global), Some(&repo)).skip,
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn a_repo_file_may_not_define_providers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = write(
            dir.path(),
            "repo.toml",
            "[providers.evil]\nbase_url = \"http://evil\"\n",
        );
        let error = load(None, Some(&repo)).unwrap_err();
        assert!(error.contains("repo.toml"), "{error}");
        assert!(error.contains("providers"), "{error}");
    }

    #[test]
    fn a_repo_permission_file_may_add_only_ask_and_deny() {
        let dir = tempfile::tempdir().expect("temp dir");
        for text in [
            "[permissions]\nallow = [\"Bash\"]\n",
            "[permissions]\ndefault = \"allow\"\n",
        ] {
            let repo = write(dir.path(), "repo.toml", text);
            let error = load(None, Some(&repo)).unwrap_err();
            assert!(error.contains("repo.toml"), "{error}");
            assert!(error.contains("permissions"), "{error}");
        }
        let repo = write(
            dir.path(),
            "repo.toml",
            "[permissions]\ndeny = [\"Bash(git push:*)\"]\nask = [\"Bash(rm:*)\"]\n",
        );
        let layers = load(None, Some(&repo)).expect("deny and ask are fine");
        assert_eq!(
            layers[0].settings.permissions.deny,
            vec!["Bash(git push:*)"]
        );
    }

    #[test]
    fn sandbox_settings_come_only_from_the_global_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "g.toml",
            "[sandbox]\nmode = \"auto\"\nnetwork = true\nwrite = [\"~/.cache\"]\n",
        );
        let merged = settings(Some(&global), None);
        assert_eq!(merged.sandbox.mode.as_deref(), Some("auto"));
        assert_eq!(merged.sandbox.network, Some(true));
        assert_eq!(merged.sandbox.write, vec!["~/.cache"]);
        let repo = write(dir.path(), "repo.toml", "[sandbox]\nmode = \"off\"\n");
        let error = load(Some(&global), Some(&repo)).unwrap_err();
        assert!(
            error.contains("repo.toml") && error.contains("sandbox"),
            "{error}"
        );
        let typo = write(dir.path(), "t.toml", "[sandbox]\nmod = \"on\"\n");
        assert!(load(Some(&typo), None).unwrap_err().contains("mod"));
    }

    #[test]
    fn a_permission_rule_typo_is_an_unknown_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "g.toml", "[permissions]\ndenny = []\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("denny"), "{error}");
    }

    #[test]
    fn providers_and_models_parse() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            concat!(
                "[providers.fi]\n",
                "wire = \"openai-chat\"\n",
                "base_url = \"https://fi.example/v1\"\n",
                "api_key_env = \"FI_KEY\"\n",
                "[providers.fi.headers]\n",
                "x-team = \"a\"\n",
                "[providers.fi.models.\"glm-5.3-flash\"]\n",
                "max_tokens = 16000\n",
                "context_window = 128000\n",
                "policy = [\"budget\"]\n",
            ),
        );
        let merged = settings(Some(&global), None);
        let provider = &merged.providers["fi"];
        assert_eq!(provider.wire.as_deref(), Some("openai-chat"));
        assert_eq!(provider.base_url.as_deref(), Some("https://fi.example/v1"));
        assert_eq!(provider.api_key_env.as_deref(), Some("FI_KEY"));
        assert_eq!(provider.headers["x-team"], "a");
        let model = &provider.models["glm-5.3-flash"];
        assert_eq!(model.max_tokens, Some(16000));
        assert_eq!(model.context_window, Some(128_000));
        assert_eq!(model.policy, Some(vec!["budget".to_string()]));
    }

    #[test]
    fn an_unknown_provider_key_names_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(
            dir.path(),
            "global.toml",
            "[providers.fi]\nbaseurl = \"x\"\n",
        );
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("baseurl"), "{error}");
    }

    #[test]
    fn an_old_top_level_provider_key_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "base_url = \"http://x\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("base_url"), "{error}");
    }

    #[test]
    fn an_unknown_key_names_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "modle = \"glm\"\n");
        let error = load(Some(&global), None).unwrap_err();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("modle"), "{error}");
    }

    #[test]
    fn layers_keep_their_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let global = write(dir.path(), "global.toml", "policy = [\"all\"]\n");
        let layers = load(Some(&global), None).expect("loads");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].path, global);
        assert_eq!(layers[0].settings.policy, Some(vec!["all".to_string()]));
    }

    #[test]
    fn no_files_means_no_settings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing.toml");
        assert!(
            load(Some(&missing), Some(&missing))
                .expect("loads")
                .is_empty()
        );
        assert_eq!(settings(None, None), Settings::default());
    }

    #[test]
    fn repo_root_finds_the_folder_with_git() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "project/.git", "gitdir: elsewhere\n");
        write(dir.path(), "project/a/b/file", "");
        let nested = dir.path().join("project/a/b");
        assert_eq!(repo_root(&nested), Some(dir.path().join("project")));
    }

    #[test]
    fn the_global_dir_follows_xdg_then_home() {
        let some = |text: &str| Some(OsString::from(text));
        assert_eq!(
            global_dir_from(some("/x"), some("/h")),
            Some(PathBuf::from("/x/reagent"))
        );
        assert_eq!(
            global_dir_from(some(""), some("/h")),
            Some(PathBuf::from("/h/.config/reagent"))
        );
        assert_eq!(global_dir_from(None, None), None);
    }
}

//! The `plugin` command: install a plugin from npm or a git repository with its
//! lockfile, list the installed ones, remove one and update it.
//!
//! The fetch and the lockfile live in `reagent_script::plugin::install`, which
//! the `node` feature brings in: a build without it can still list and remove.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value as JsonValue;

/// The usage of the command, printed by `plugin` and `plugin --help`.
const USAGE: &str = concat!(
    "Usage:\n",
    "  reagent plugin add npm:<name>[@<range>] [--yes]\n",
    "  reagent plugin add git:<host>/<path>[@<ref>] [--yes]\n",
    "  reagent plugin list\n",
    "  reagent plugin remove <folder> [--yes]\n",
    "  reagent plugin update <folder> [--yes]\n",
    "\n",
    "add fetches a plugin published on npm, or the folder of a git repository\n",
    "at <ref> (the default branch when it names none), with every dependency\n",
    "its imports reach, into <config dir>/plugins/<folder>/ and writes its\n",
    "lockfile there; it asks before it moves the folder into place.\n",
    "list names every plugin folder with its version and its source, or local\n",
    "for a folder nobody installed.\n",
    "remove deletes a folder that add installed.\n",
    "update fetches the source add recorded again, and replaces the folder when\n",
    "the source serves something else.\n",
);

/// What a run says when stdin is not a terminal and `--yes` is missing.
const NO_TERMINAL: &str = "not a terminal: pass --yes to install without a question";

/// What a build without the `node` feature says about `add` and `update`.
#[cfg(not(feature = "node"))]
const NO_INSTALLER: &str = "this build has no plugin installer";

/// The exit code and the text of a refusal.
#[derive(Debug)]
struct Fail {
    /// The code the process exits with.
    code: u8,
    /// The line it writes to stderr.
    message: String,
}

impl Fail {
    /// A misuse of the command: an unknown word, a missing `--yes` on a
    /// terminal, a source this build cannot install.
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    /// A command that ran and did not finish its work.
    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

/// Runs one `plugin` command and returns the process exit code.
pub async fn run(args: Vec<OsString>) -> ExitCode {
    match dispatch(&args).await {
        Ok(code) => code,
        Err(fail) => {
            eprintln!("reagent: {}", fail.message);
            ExitCode::from(fail.code)
        }
    }
}

/// The command the words name.
async fn dispatch(args: &[OsString]) -> Result<ExitCode, Fail> {
    if args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "-h" || arg == "--help")
    }) {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    match args.first().and_then(|arg| arg.to_str()) {
        None => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("add") => add(&args[1..]).await,
        Some("list") => list().await,
        Some("remove") => remove(&args[1..]).await,
        Some("update") => update(&args[1..]).await,
        Some(word) => Err(Fail::usage(format!("unknown plugin command: {word}"))),
    }
}

/// The words one command takes: the one value it needs and `--yes`.
struct Words {
    /// The source or the folder the words name.
    value: String,
    /// Whether the user said yes already.
    yes: bool,
}

/// Reads the words of `add`, `remove` and `update`.
fn words(args: &[OsString], command: &str) -> Result<Words, Fail> {
    let mut parser = lexopt::Parser::from_args(args.to_vec());
    let mut value: Option<String> = None;
    let mut yes = false;
    while let Some(arg) = parser
        .next()
        .map_err(|error| Fail::usage(error.to_string()))?
    {
        match arg {
            lexopt::Arg::Long("yes") => yes = true,
            lexopt::Arg::Value(text) => {
                let text = text
                    .into_string()
                    .map_err(|_| Fail::usage(format!("{command} needs valid UTF-8")))?;
                if value.replace(text.clone()).is_some() {
                    return Err(Fail::usage(format!("unexpected argument: {text}")));
                }
            }
            other => return Err(Fail::usage(format!("unknown flag: {other:?}"))),
        }
    }
    let value =
        value.ok_or_else(|| Fail::usage(format!("usage: reagent plugin {command} <value>")))?;
    Ok(Words { value, yes })
}

/// `add`: fetches a plugin into the plugins folder and its lockfile.
async fn add(args: &[OsString]) -> Result<ExitCode, Fail> {
    let Words { value: spec, yes } = words(args, "add")?;
    if !spec.starts_with("npm:") && !spec.starts_with("git:") {
        return Err(Fail::usage(format!(
            "{spec}: a plugin source is npm:<name>[@<range>] or git:<host>/<path>[@<ref>]"
        )));
    }
    askable(yes)?;
    let plugins = plugins_dir(true).await?;
    #[cfg(feature = "node")]
    {
        let client = reagent_oauth::http::client();
        let pending = reagent_script::plugin::install::stage_add(&client, &spec, &plugins)
            .await
            .map_err(Fail::failed)?;
        eprintln!("{}", pending.summary.text());
        let name = pending.name.clone();
        if !yes && !confirm(&format!("Install {name}?")).await? {
            pending.abort().await;
            return Ok(ExitCode::from(1));
        }
        let source = pending.summary.source.clone();
        let folder = pending.name.clone();
        pending.commit().await.map_err(Fail::failed)?;
        println!("installed {folder} {source}");
        Ok(ExitCode::SUCCESS)
    }
    #[cfg(not(feature = "node"))]
    {
        let _ = &plugins;
        Err(Fail::usage(NO_INSTALLER))
    }
}

/// `list`: one line per plugin folder, with its version and its source.
async fn list() -> Result<ExitCode, Fail> {
    let plugins = plugins_dir(false).await?;
    let mut dir = match tokio::fs::read_dir(&plugins).await {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => return Err(Fail::failed(format!("{}: {error}", plugins.display()))),
    };
    let mut folders = Vec::new();
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|error| Fail::failed(format!("{}: {error}", plugins.display())))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        folders.push(name);
    }
    folders.sort();
    for folder in folders {
        let dir = plugins.join(&folder);
        let Some(manifest) = manifest(&dir.join("package.json")).await else {
            continue;
        };
        if manifest
            .pointer("/reagent/entry")
            .and_then(JsonValue::as_str)
            .is_none()
        {
            continue;
        }
        let version = manifest
            .get("version")
            .and_then(JsonValue::as_str)
            .unwrap_or("0.0.0");
        let source = installed_source(&dir)
            .await?
            .unwrap_or_else(|| "local".to_owned());
        println!("{folder}  {version}  {source}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `remove`: deletes a folder that `add` installed.
async fn remove(args: &[OsString]) -> Result<ExitCode, Fail> {
    let Words { value: folder, yes } = words(args, "remove")?;
    check_folder(&folder)?;
    askable(yes)?;
    let plugins = plugins_dir(false).await?;
    let dir = plugins.join(&folder);
    if tokio::fs::symlink_metadata(&dir).await.is_err() {
        return Err(Fail::failed(format!("no plugin folder {folder}")));
    }
    if installed_source(&dir).await?.is_none() {
        return Err(Fail::failed(format!(
            "{folder} was not installed by reagent plugin add; delete the folder yourself"
        )));
    }
    if !yes && !confirm(&format!("Remove {folder}?")).await? {
        return Ok(ExitCode::from(1));
    }
    tokio::fs::remove_dir_all(&dir)
        .await
        .map_err(|error| Fail::failed(format!("{folder}: {error}")))?;
    println!("removed {folder}");
    Ok(ExitCode::SUCCESS)
}

/// `update`: fetches the source the lockfile records again and replaces the
/// folder when the source serves something else.
async fn update(args: &[OsString]) -> Result<ExitCode, Fail> {
    let Words { value: folder, yes } = words(args, "update")?;
    check_folder(&folder)?;
    askable(yes)?;
    let plugins = plugins_dir(false).await?;
    #[cfg(feature = "node")]
    {
        use reagent_script::plugin::install::{Staged, stage_update};
        let client = reagent_oauth::http::client();
        let staged = stage_update(&client, &folder, &plugins)
            .await
            .map_err(Fail::failed)?;
        let ready = match staged {
            Staged::UpToDate => {
                println!("{folder} is up to date");
                return Ok(ExitCode::SUCCESS);
            }
            Staged::Ready(ready) => ready,
        };
        let summary = &ready.pending.summary;
        eprintln!("{folder}: {} -> {}", ready.old_version, summary.revision());
        if ready.old_permissions != summary.permissions {
            eprintln!(
                "permissions: {} -> {}",
                ready.old_permissions.as_deref().unwrap_or("none"),
                summary.permissions.as_deref().unwrap_or("none")
            );
        }
        eprintln!("{}", summary.text());
        if !yes && !confirm(&format!("Update {folder}?")).await? {
            ready.pending.abort().await;
            return Ok(ExitCode::from(1));
        }
        let version = summary.version.clone();
        ready.pending.commit().await.map_err(Fail::failed)?;
        println!("updated {folder} to {version}");
        Ok(ExitCode::SUCCESS)
    }
    #[cfg(not(feature = "node"))]
    {
        let _ = &plugins;
        Err(Fail::usage(NO_INSTALLER))
    }
}

/// Refuses a question the terminal cannot answer.
fn askable(yes: bool) -> Result<(), Fail> {
    if yes || std::io::stdin().is_terminal() {
        return Ok(());
    }
    Err(Fail::usage(NO_TERMINAL))
}

/// Asks `question` on the terminal: only `y` or `yes` is a yes.
async fn confirm(question: &str) -> Result<bool, Fail> {
    use std::io::Write as _;

    eprint!("{question} [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|error| Fail::failed(format!("the terminal: {error}")))?;
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .map_err(|_| Fail::failed("the terminal task failed"))?;
    let answer = read.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// The plugins folder of the config folder, made when `create` is set.
async fn plugins_dir(create: bool) -> Result<PathBuf, Fail> {
    let config = reagent_config::global_dir()
        .ok_or_else(|| Fail::usage("no config folder; set HOME or XDG_CONFIG_HOME"))?;
    let dir = config.join("plugins");
    if create {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| Fail::failed(format!("{}: {error}", dir.display())))?;
    }
    Ok(dir)
}

/// Refuses a name that is not one folder under the plugins root.
fn check_folder(folder: &str) -> Result<(), Fail> {
    if folder.is_empty() || folder.starts_with('.') || folder.contains('/') || folder.contains('\\')
    {
        return Err(Fail::usage(format!(
            "{folder:?} is not a plugin folder name"
        )));
    }
    Ok(())
}

/// The `package.json` of a plugin folder, when it reads as JSON.
async fn manifest(path: &std::path::Path) -> Option<JsonValue> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

/// The source the folder's lockfile records, or `None` when it has none.
async fn installed_source(dir: &std::path::Path) -> Result<Option<String>, Fail> {
    let dir = dir.to_path_buf();
    let lock = tokio::task::spawn_blocking(move || reagent_script::plugin::lock::read(&dir))
        .await
        .map_err(|_| Fail::failed("the lockfile task failed"))?;
    match lock {
        Ok(lock) => Ok(lock.map(|lock| lock.source)),
        Err(error) => Err(Fail::failed(error)),
    }
}

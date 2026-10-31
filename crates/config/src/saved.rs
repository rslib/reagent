//! The saved permission rules of one project.
//!
//! A "save" answer to an approval writes its allow rule here, in reagent's
//! state folder and outside the repo, so that every later session in the same
//! project allows the call without asking. Only the owner may read the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;

use crate::permissions::{Kind, Lists, parse_rule};
use crate::{Layer, Settings, parse_error, read_text, repo_root};

/// The most bytes of the project's last name a key keeps.
const NAME_MAX: usize = 40;

/// The text a key gets when the project folder has no name to keep, as `/`
/// has none.
const UNNAMED: &str = "root";

/// `<state>/permissions/<key>.toml` for the project of `cwd`.
///
/// The project is the nearest folder at or above `cwd` that holds `.git`, or
/// `cwd` itself outside a repo. The key is the project folder's last name and
/// a hash of the whole path, so two projects with one last name differ.
pub fn path(state: &Path, cwd: &Path) -> PathBuf {
    let project = project(cwd);
    state
        .join("permissions")
        .join(format!("{}.toml", key(&project)))
}

/// The settings of a saved file: `[permissions]` and only its `allow` list.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedFile {
    permissions: SavedPermissions,
}

/// The `[permissions]` table of a saved file.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SavedPermissions {
    #[serde(default)]
    allow: Vec<String>,
}

/// The saved layer of `path`, or `None` when the file is absent.
pub fn load(path: &Path) -> Result<Option<Layer>, String> {
    let Some(text) = read_text(path)?.map(|text| text.text) else {
        return Ok(None);
    };
    let file: SavedFile =
        toml::from_str(&text).map_err(|error| parse_error(path, &text, &error))?;
    Ok(Some(Layer {
        path: path.to_path_buf(),
        settings: Settings {
            permissions: Lists {
                allow: file.permissions.allow,
                ..Lists::default()
            },
            ..Settings::default()
        },
    }))
}

/// Adds the allow rule `rule` to the file of `path`, returning whether the
/// file changed: a rule that is saved already changes nothing.
pub fn add(path: &Path, rule: &str) -> Result<bool, String> {
    let rule = allow_text(path, rule)?;
    let _lock = lock(path)?;
    let mut rules = saved_rules(path)?;
    if rules.iter().any(|saved| saved == rule) {
        return Ok(false);
    }
    rules.push(rule.to_string());
    write(path, &rules)?;
    Ok(true)
}

/// Removes the rule `rule` from the file of `path`, returning whether the
/// file changed: a rule that is not saved changes nothing.
pub fn remove(path: &Path, rule: &str) -> Result<bool, String> {
    let rule = rule.trim();
    let _lock = lock(path)?;
    let mut rules = saved_rules(path)?;
    let before = rules.len();
    rules.retain(|saved| saved != rule);
    if rules.len() == before {
        return Ok(false);
    }
    write(path, &rules)?;
    Ok(true)
}

/// `rule` trimmed, once the permissions parser accepts it as an allow rule.
/// The caller writes nothing when it does not.
fn allow_text<'a>(path: &Path, rule: &'a str) -> Result<&'a str, String> {
    let file: Arc<str> = Arc::from(path.display().to_string());
    parse_rule(Kind::Allow, rule.trim(), &file)
        .map_err(|error| format!("{file}: rule {rule:?}: {error}"))?;
    Ok(rule.trim())
}

/// The rules the file of `path` holds, in their order; none when it is absent.
fn saved_rules(path: &Path) -> Result<Vec<String>, String> {
    Ok(load(path)?
        .map(|layer| layer.settings.permissions.allow)
        .unwrap_or_default())
}

/// The project folder of `cwd`: the repo root, or `cwd` itself, with its
/// links resolved, so `/tmp/p`, `/private/tmp/p` and `p/../p` name one
/// project wherever the folder came from.
#[expect(
    clippy::disallowed_methods,
    reason = "callers run it inside spawn_blocking or before the runtime starts"
)]
fn project(cwd: &Path) -> PathBuf {
    let cwd = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf()));
    repo_root(&cwd).unwrap_or(cwd)
}

/// The file name of the project, without its `.toml`.
fn key(project: &Path) -> String {
    let last = project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| UNNAMED.to_string());
    let mut name: String = last
        .bytes()
        .take(NAME_MAX)
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"._-".contains(&byte) {
                byte as char
            } else {
                '-'
            }
        })
        .collect();
    name.push('-');
    name.push_str(&format!("{:016x}", hash(project)));
    name
}

/// The FNV-1a hash of the project path bytes.
fn hash(project: &Path) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in project.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The text of a saved file that holds `rules`: one rule per line, in order.
fn render(rules: &[String]) -> String {
    let mut out = String::from("[permissions]\n");
    if rules.is_empty() {
        out.push_str("allow = []\n");
        return out;
    }
    out.push_str("allow = [\n");
    for rule in rules {
        out.push_str("  ");
        out.push_str(&toml::Value::String(rule.clone()).to_string());
        out.push_str(",\n");
    }
    out.push_str("]\n");
    out
}

/// Writes `rules` over the file of `path`.
///
/// The folder is created 0700 and the text lands in a temp file of the same
/// folder that is 0600 from its first byte, then renamed over the file: a
/// reader sees the old file or the new one, never half of either, and only
/// its owner may read it.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "callers run it inside spawn_blocking"
)]
fn write(path: &Path, rules: &[String]) -> Result<(), String> {
    use std::io::Write;

    make_folder(path)?;
    let temp = temp_name(path);
    let fail = |error: std::io::Error| format!("{}: {error}", temp.display());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temp)
        .map_err(fail)
        .and_then(|mut file| file.write_all(render(rules).as_bytes()).map_err(fail));
    if let Err(error) = written.and_then(|()| std::fs::rename(&temp, path).map_err(fail)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Makes the folder of `path` with mode 0700 when it is missing.
fn make_folder(path: &Path) -> Result<(), String> {
    let folder = path.parent().unwrap_or(Path::new(""));
    let mut create = std::fs::DirBuilder::new();
    create.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        create.mode(0o700);
    }
    create
        .create(folder)
        .map_err(|error| format!("{}: {error}", folder.display()))
}

/// Holds the lock of the file of `path` until it drops, so the read, the
/// change and the write of one `add` or `remove` never interleave with
/// another's, in this process or in another: one approval saves up to five
/// rules, and two workers may share a project.
#[expect(
    clippy::disallowed_types,
    reason = "callers run it inside spawn_blocking"
)]
fn lock(path: &Path) -> Result<std::fs::File, String> {
    make_folder(path)?;
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let at = PathBuf::from(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&at)
        .map_err(|error| format!("{}: {error}", at.display()))?;
    file.lock()
        .map_err(|error| format!("{}: {error}", at.display()))?;
    Ok(file)
}

/// A temp file name in the folder of `path`, so two writers never share one.
fn temp_name(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::permissions::{Call, Kind, Reason, Rules};

    #[test]
    fn saved_adds_from_many_threads_keep_every_rule() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let file = tmp.path().join("permissions").join("p.toml");
        let threads: Vec<_> = (0..5)
            .map(|n| {
                let file = file.clone();
                std::thread::spawn(move || add(&file, &format!("Bash(tool{n}:*)")).expect("adds"))
            })
            .collect();
        for thread in threads {
            assert!(thread.join().expect("the thread ends"));
        }
        let mut rules = saved_rules(&file).expect("reads");
        rules.sort();
        let want: Vec<String> = (0..5).map(|n| format!("Bash(tool{n}:*)")).collect();
        assert_eq!(rules, want, "no add overwrote another");
    }

    #[cfg(unix)]
    #[test]
    fn saved_path_is_one_for_a_folder_and_a_link_to_it() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let state = tmp.path().join("state");
        let real = folder(tmp.path(), "real");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("links");
        assert_eq!(path(&state, &link), path(&state, &real));
        assert_eq!(
            path(&state, &real.join("..").join("real")),
            path(&state, &real)
        );
    }

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn folder(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).expect("makes the folder");
        path
    }

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("writes the file");
    }

    #[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
    fn text(path: &Path) -> String {
        std::fs::read_to_string(path).expect("reads the file")
    }

    #[expect(clippy::disallowed_methods, reason = "a test lists its own folder")]
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("lists the folder")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[expect(clippy::disallowed_methods, reason = "a test reads its own mode")]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("reads the mode")
            .permissions()
            .mode()
            & 0o777
    }

    fn errors_with(error: &str, name: &str) {
        assert!(error.contains(name), "{error}");
    }

    /// The state folder and the project folder of one test.
    fn setup(dir: &Path) -> (PathBuf, PathBuf) {
        (dir.join("state"), folder(dir, "work"))
    }

    #[test]
    fn saved_path_is_stable_for_one_folder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        assert_eq!(path(&state, &work), path(&state, &work));
        // The state folder is not part of the key, the project is.
        assert_eq!(
            path(&state, &work).file_name(),
            path(&dir.path().join("other"), &work).file_name()
        );
    }

    #[test]
    fn saved_paths_differ_for_two_projects_with_the_same_last_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = dir.path().join("state");
        let one = folder(dir.path(), "one/app");
        let two = folder(dir.path(), "two/app");
        let (one_path, two_path) = (path(&state, &one), path(&state, &two));
        assert_ne!(one_path, two_path);
        for saved in [&one_path, &two_path] {
            let name = saved
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a name");
            let (prefix, hash) = name
                .strip_suffix(".toml")
                .and_then(|name| name.rsplit_once('-'))
                .expect("a name and a hash");
            assert_eq!(prefix, "app");
            assert_eq!(hash.len(), 16, "{name}");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        }
    }

    #[test]
    fn saved_path_is_the_repo_root_of_a_folder_inside_a_repo() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = dir.path().join("state");
        let root = folder(dir.path(), "root");
        folder(&root, ".git");
        let deep = folder(&root, "src/deep");
        assert_eq!(path(&state, &deep), path(&state, &root));
    }

    #[test]
    fn saved_path_is_under_the_permissions_folder_of_the_state_folder() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let saved = path(&state, &work);
        assert_eq!(saved.parent(), Some(state.join("permissions").as_path()));
        assert!(
            saved
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".toml")),
            "{}",
            saved.display()
        );
    }

    #[test]
    fn saved_load_of_a_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing.toml");
        assert_eq!(load(&missing).expect("loads"), None);
    }

    #[test]
    fn saved_load_reads_allow_rules_and_names_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("p.toml");
        write(&file, "[permissions]\nallow = [\"Bash(cargo test:*)\"]\n");
        let layer = load(&file).expect("loads").expect("has a layer");
        assert_eq!(layer.path, file);
        assert_eq!(layer.settings.permissions.allow, ["Bash(cargo test:*)"]);
        let rules = Rules::from_layers(&[layer], dir.path(), None).expect("rules");
        let decision = rules.decide(&Call {
            tool: "bash",
            main_arg: Some("command"),
            value: Some("cargo test -p b"),
            reads: &[],
            writes: &[],
        });
        assert_eq!(decision.kind, Kind::Allow);
        assert_eq!(
            decision.reason,
            Reason::Rule {
                rule: "Bash(cargo test:*)".into(),
                file: file.display().to_string().into(),
            }
        );
    }

    #[test]
    fn saved_load_refuses_an_ask_list_or_an_unknown_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, saved) in [
            ("ask.toml", "[permissions]\nask = [\"Bash\"]\n"),
            (
                "key.toml",
                "[permissions]\nallow = [\"Bash\"]\nextra = [\"Bash\"]\n",
            ),
            ("table.toml", "[model]\nname = \"m\"\n"),
        ] {
            let file = dir.path().join(name);
            write(&file, saved);
            let error = load(&file).expect_err("refused");
            errors_with(&error, name);
        }
    }

    #[cfg(unix)]
    #[test]
    fn saved_add_writes_the_expected_bytes_in_a_private_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        assert!(add(&file, "Bash(cargo test:*)").expect("adds"));
        assert_eq!(
            text(&file),
            "[permissions]\nallow = [\n  \"Bash(cargo test:*)\",\n]\n"
        );
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().expect("a folder")), 0o700);
        assert!(add(&file, "Read(src/*)").expect("adds"));
        assert_eq!(
            text(&file),
            "[permissions]\nallow = [\n  \"Bash(cargo test:*)\",\n  \"Read(src/*)\",\n]\n"
        );
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .expect("a name");
        // The file and the lock its writers share; no temp file is left.
        let mut want = [name.clone(), format!("{name}.lock")];
        want.sort();
        let mut found = names(file.parent().expect("a folder"));
        found.sort();
        assert_eq!(found, want);
    }

    #[test]
    fn saved_add_of_a_saved_rule_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        assert!(add(&file, "Bash(git status)").expect("adds"));
        let before = text(&file);
        assert!(!add(&file, "Bash(git status)").expect("adds"));
        assert_eq!(text(&file), before);
        // The same rule with spaces around it is the same rule.
        assert!(!add(&file, " Bash(git status) ").expect("adds"));
        assert_eq!(text(&file), before);
    }

    #[test]
    fn saved_add_refuses_a_rule_that_does_not_parse() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        let error = add(&file, "Bash(").expect_err("refused");
        errors_with(&error, "Bash(");
        assert!(!file.exists(), "{}", file.display());
        assert!(!file.parent().expect("a folder").exists());
        assert!(!state.exists());
    }

    #[test]
    fn saved_remove_keeps_the_other_rules_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        for rule in ["Bash(a)", "Bash(b)", "Read(src/*)"] {
            assert!(add(&file, rule).expect("adds"));
        }
        assert!(remove(&file, "Bash(b)").expect("removes"));
        assert_eq!(
            text(&file),
            "[permissions]\nallow = [\n  \"Bash(a)\",\n  \"Read(src/*)\",\n]\n"
        );
        let layer = load(&file).expect("loads").expect("has a layer");
        assert_eq!(layer.settings.permissions.allow, ["Bash(a)", "Read(src/*)"]);
    }

    #[test]
    fn saved_remove_of_an_unsaved_rule_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        assert!(!remove(&file, "Bash(a)").expect("removes"));
        assert!(!file.exists(), "{}", file.display());
        assert!(add(&file, "Bash(a)").expect("adds"));
        let before = text(&file);
        assert!(!remove(&file, "Bash(b)").expect("removes"));
        assert_eq!(text(&file), before);
    }

    #[test]
    fn saved_remove_of_the_last_rule_leaves_an_empty_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, work) = setup(dir.path());
        let file = path(&state, &work);
        assert!(add(&file, "Bash").expect("adds"));
        assert!(remove(&file, "Bash").expect("removes"));
        assert_eq!(text(&file), "[permissions]\nallow = []\n");
        let layer = load(&file).expect("loads").expect("has a layer");
        assert!(layer.settings.permissions.allow.is_empty());
    }
}

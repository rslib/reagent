//! Reading a plugin folder's `package.json`.

use std::path::{Component, Path, PathBuf};

/// What one folder under the plugins root holds.
#[expect(
    clippy::large_enum_variant,
    reason = "a manifest is read once, at load, and never copied"
)]
pub(crate) enum Found {
    /// A plugin, with the entry file it names.
    Plugin(Manifest),
    /// Not a plugin, and nothing to say about it.
    Skipped,
    /// A plugin whose manifest or entry is wrong, with the reason.
    Bad(String),
}

/// One plugin's `package.json`, with the folder and entry it names.
///
/// Both paths are canonical: a plugin that names its files through a symbol
/// link out of its folder is refused before it loads.
pub(crate) struct Manifest {
    /// The name the notes use, from `name` or else the folder.
    pub name: String,
    /// The version, from `version`; `"0.0.0"` when it names none.
    pub version: String,
    /// The plugin's own folder, canonical.
    pub dir: PathBuf,
    /// The module the folder's `reagent.entry` names, canonical.
    pub entry: PathBuf,
    /// What its `reagent.permissions` lets it touch.
    #[cfg_attr(
        not(feature = "node"),
        allow(dead_code, reason = "only the node modules read the permissions")
    )]
    pub permissions: Permissions,
    /// The role it plays: a normal run loads the plugins that are not
    /// [`Role::Gateway`], and `reagent gateway` loads only those.
    pub role: Role,
}

/// The role a plugin's `reagent.role` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// A plugin a normal run loads, and the gateway skips.
    Normal,
    /// A plugin only `reagent gateway` loads.
    Gateway,
}

impl Role {
    /// The role `reagent.role` names, or the reason it names none.
    fn read(reagent: &serde_json::Value) -> Result<Role, String> {
        match reagent.get("role") {
            None => Ok(Role::Normal),
            Some(role) if role.as_str() == Some("gateway") => Ok(Role::Gateway),
            Some(role) => Err(format!("unknown role {role}: `role` must be \"gateway\"")),
        }
    }
}

/// What a plugin's `reagent.permissions` lets it touch.
#[derive(Debug, PartialEq)]
pub(crate) struct Permissions {
    /// The file system roots it may use.
    pub fs: Fs,
    /// The environment variables it may read.
    pub env: Env,
    /// The hosts it may fetch from.
    #[cfg_attr(
        not(feature = "node"),
        allow(dead_code, reason = "only the node modules read the permissions")
    )]
    pub net: Vec<Host>,
    /// The programs it may run.
    pub run: Vec<Program>,
    /// The hooks it may install, by name.
    pub hooks: Vec<String>,
    /// Whether it may start sessions of its own, from `sessions: true`.
    pub sessions: bool,
}

impl Permissions {
    /// No file system, no environment and no host permission.
    fn none() -> Self {
        Self {
            fs: Fs::None,
            env: Env::None,
            net: Vec::new(),
            run: Vec::new(),
            hooks: Vec::new(),
            sessions: false,
        }
    }
}

/// One host a plugin may fetch from.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    not(feature = "node"),
    allow(dead_code, reason = "only the node modules read the permissions")
)]
pub(crate) enum Host {
    /// This host and no other.
    Name(String),
    /// Any host ending with `.` and this name.
    Wildcard(String),
    /// This address, as a literal in the URL.
    Ip(std::net::IpAddr),
    /// `*`: any public host for `fetch`, over https. A child process of the
    /// plugin gets the whole network with it, private addresses included: the
    /// sandbox fences a child's network only as all or nothing.
    Any,
}

/// One program a plugin's `run` list names.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Program {
    /// A bare name, which a later change resolves on `PATH` when it runs.
    Name(String),
    /// An absolute path.
    Path(PathBuf),
}

/// The file system a plugin may use.
#[derive(Debug, PartialEq)]
pub(crate) enum Fs {
    /// Nothing.
    None,
    /// Read anywhere.
    ReadAll,
    /// Read and write anywhere.
    WriteAll,
    /// Just these roots; a write root grants read on it too.
    Lists {
        /// Every root it may read.
        read: Vec<Root>,
        /// Every root it may write.
        write: Vec<Root>,
    },
}

/// The environment variables a plugin may read.
#[derive(Debug, PartialEq)]
pub(crate) enum Env {
    /// None of them.
    None,
    /// Every one.
    All,
    /// Just these names.
    Names(Vec<String>),
}

/// One folder a permissions path names, with no file system touched.
#[derive(Debug, PartialEq)]
pub(crate) enum Root {
    /// The session folder plus a relative part, which may be empty.
    Session(PathBuf),
    /// The home folder plus a relative part, which may be empty.
    Home(PathBuf),
    /// An absolute path.
    Absolute(PathBuf),
}

/// Reads the manifest of the plugin in `dir`.
///
/// A folder with no `package.json`, or one that names no `reagent.entry`, is
/// not a plugin and is skipped without a word.
pub(crate) fn read(dir: &Path) -> Found {
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let dir = match std::fs::canonicalize(dir) {
        Ok(dir) => dir,
        Err(error) => return Found::Bad(format!("the plugin folder: {error}")),
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let text = match std::fs::read_to_string(dir.join("package.json")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Found::Skipped,
        Err(error) => return Found::Bad(format!("package.json: {error}")),
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(error) => return Found::Bad(format!("package.json is not valid JSON: {error}")),
    };
    let Some(object) = json.as_object() else {
        return Found::Bad("package.json is not a JSON object".into());
    };
    let Some(reagent) = object.get("reagent") else {
        return Found::Skipped;
    };
    let Some(entry) = reagent.get("entry").and_then(|entry| entry.as_str()) else {
        return Found::Skipped;
    };
    let permissions = match permissions(reagent) {
        Ok(permissions) => permissions,
        Err(reason) => return Found::Bad(reason),
    };
    let role = match Role::read(reagent) {
        Ok(role) => role,
        Err(reason) => return Found::Bad(reason),
    };
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = object
        .get("name")
        .and_then(|name| name.as_str())
        .unwrap_or(&folder)
        .to_string();
    let version = object
        .get("version")
        .and_then(|version| version.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    let Some(relative) = normalize(Path::new(entry)) else {
        return Found::Bad(format!("entry {entry} leaves the plugin folder"));
    };
    let Some(relative) = inside(&relative) else {
        return Found::Bad(format!("entry {entry} leaves the plugin folder"));
    };
    let full = dir.join(&relative);
    // Canonical, so an entry that is a link out of the folder is caught too.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let Ok(entry_at) = std::fs::canonicalize(&full) else {
        return Found::Bad(format!("entry {entry} does not exist"));
    };
    if !entry_at.starts_with(&dir) {
        return Found::Bad(format!("entry {entry} leaves the plugin folder"));
    }
    if !entry_at.is_file() {
        return Found::Bad(format!("entry {entry} is not a file"));
    }
    Found::Plugin(Manifest {
        name,
        version,
        dir,
        entry: entry_at,
        permissions,
        role,
    })
}

/// The permissions `reagent.permissions` names, or the reason they are wrong.
fn permissions(reagent: &serde_json::Value) -> Result<Permissions, String> {
    let Some(permissions) = reagent.get("permissions") else {
        return Ok(Permissions::none());
    };
    let Some(object) = permissions.as_object() else {
        return Err("the `permissions` block must be an object with `fs` and `env`".into());
    };
    for key in object.keys() {
        if key != "fs"
            && key != "env"
            && key != "net"
            && key != "run"
            && key != "hooks"
            && key != "sessions"
        {
            return Err(format!("unknown permission `{key}`"));
        }
    }
    let fs = match object.get("fs") {
        Some(fs) => fs_of(fs)?,
        None => Fs::None,
    };
    let env = match object.get("env") {
        Some(env) => env_of(env)?,
        None => Env::None,
    };
    let net = match object.get("net") {
        Some(net) => net_of(net)?,
        None => Vec::new(),
    };
    let run = match object.get("run") {
        Some(run) => run_of(run)?,
        None => Vec::new(),
    };
    let hooks = match object.get("hooks") {
        Some(hooks) => hooks_of(hooks)?,
        None => Vec::new(),
    };
    let sessions = match object.get("sessions") {
        Some(sessions) => sessions_of(sessions)?,
        None => false,
    };
    Ok(Permissions {
        fs,
        env,
        net,
        run,
        hooks,
        sessions,
    })
}

/// The session permission `sessions` names, or the reason it is wrong.
fn sessions_of(value: &serde_json::Value) -> Result<bool, String> {
    const EXPECTED: &str = "permission `sessions` must be true or false";
    match value.as_bool() {
        Some(flag) => Ok(flag),
        None => Err(format!("{EXPECTED}, not {value}")),
    }
}

/// The hooks `hooks` names, or the reason one of them is not a hook.
fn hooks_of(value: &serde_json::Value) -> Result<Vec<String>, String> {
    const EXPECTED: &str = "permission `hooks` must be a list of hook names";
    let Some(items) = value.as_array() else {
        return Err(EXPECTED.into());
    };
    let mut hooks = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(EXPECTED.into());
        };
        if !super::api::HOOKS.contains(&text) {
            return Err(format!(
                "permission `hooks` names {text:?}, which is not a hook: use one of {}",
                super::api::HOOKS.join(", ")
            ));
        }
        if !hooks.iter().any(|hook| hook == text) {
            hooks.push(text.to_owned());
        }
    }
    Ok(hooks)
}

/// The file system permission `fs` names, or the reason it is wrong.
fn fs_of(fs: &serde_json::Value) -> Result<Fs, String> {
    const EXPECTED: &str =
        r#"permission `fs` must be "read", "write" or an object with `read` and `write` lists"#;
    if let Some(mode) = fs.as_str() {
        return match mode {
            "read" => Ok(Fs::ReadAll),
            "write" => Ok(Fs::WriteAll),
            other => Err(format!("{EXPECTED}, not {other:?}")),
        };
    }
    let Some(object) = fs.as_object() else {
        return Err(EXPECTED.into());
    };
    for key in object.keys() {
        if key != "read" && key != "write" {
            return Err(format!("unknown key `{key}` in permission `fs`"));
        }
    }
    Ok(Fs::Lists {
        read: roots(object.get("read"), "read")?,
        write: roots(object.get("write"), "write")?,
    })
}

/// The roots a `fs.read` or `fs.write` list names, or the reason it is wrong.
fn roots(value: Option<&serde_json::Value>, key: &str) -> Result<Vec<Root>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(format!("permission `fs.{key}` must be a list of paths"));
    };
    items
        .iter()
        .map(|item| {
            let Some(text) = item.as_str() else {
                return Err(format!("permission `fs.{key}` must be a list of paths"));
            };
            root(text).map_err(|reason| format!("permission `fs.{key}` {reason}"))
        })
        .collect()
}

/// The root one path in an `fs` list names, or the reason it is wrong.
fn root(text: &str) -> Result<Root, String> {
    if text.is_empty() {
        return Err(format!("path {text:?} must not be empty"));
    }
    if text.contains('\0') {
        return Err(format!("path {text:?} must not contain a NUL byte"));
    }
    if text == "~" {
        return Ok(Root::Home(PathBuf::new()));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(Root::Home(unresolved(Path::new(rest), text)?));
    }
    if text.starts_with('~') {
        return Err(format!(
            "path {text:?} must be `~` or `~/` and a folder, not another user's home"
        ));
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return Ok(Root::Absolute(unresolved(path, text)?));
    }
    Ok(Root::Session(unresolved(path, text)?))
}

/// `path` with its `.` parts dropped and its root kept, or the reason a `..`
/// refuses it: the lists stay as written, so a `..` would be hard to read.
fn unresolved(path: &Path, text: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(format!("path {text:?} must not go up a folder"));
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// The environment permission `env` names, or the reason it is wrong.
fn env_of(env: &serde_json::Value) -> Result<Env, String> {
    const EXPECTED: &str = "permission `env` must be true, false or a list of names";
    match env {
        serde_json::Value::Bool(true) => Ok(Env::All),
        serde_json::Value::Bool(false) => Ok(Env::None),
        serde_json::Value::Array(items) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(EXPECTED.into());
                };
                if !is_name(name) {
                    return Err(format!(
                        "permission `env` name {name:?} must match [A-Za-z_][A-Za-z0-9_]*"
                    ));
                }
                names.push(name.to_string());
            }
            Ok(Env::Names(names))
        }
        _ => Err(EXPECTED.into()),
    }
}

/// Whether `name` is an environment variable name.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The programs a `run` list names, or the reason it is wrong.
fn run_of(run: &serde_json::Value) -> Result<Vec<Program>, String> {
    let Some(items) = run.as_array() else {
        return Err("permission `run` must be a list of programs".into());
    };
    items.iter().map(program_of).collect()
}

/// The program one entry of a `run` list names, or the reason it is wrong: a
/// bare name with no `/`, or an absolute path, and no whitespace or NUL either
/// way, so an entry is never read as two programs or as an argument of its own.
fn program_of(item: &serde_json::Value) -> Result<Program, String> {
    let refused =
        || format!("permission `run` entry {item} must be a program name or an absolute path");
    let Some(text) = item.as_str() else {
        return Err(refused());
    };
    if text.is_empty() || text.contains('\0') || text.chars().any(char::is_whitespace) {
        return Err(refused());
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return Ok(Program::Path(path.to_path_buf()));
    }
    if text.contains('/') {
        return Err(refused());
    }
    Ok(Program::Name(text.to_string()))
}

/// The hosts a `net` list names, or the reason it is wrong.
fn net_of(net: &serde_json::Value) -> Result<Vec<Host>, String> {
    let Some(items) = net.as_array() else {
        return Err("permission `net` must be a list of hosts".into());
    };
    items.iter().map(host_of).collect()
}

/// The host one entry of a `net` list names, or the reason it is wrong.
fn host_of(item: &serde_json::Value) -> Result<Host, String> {
    let Some(text) = item.as_str() else {
        return Err("permission `net` must be a list of hosts".into());
    };
    let lowered = text.to_ascii_lowercase();
    if lowered == "*" {
        return Ok(Host::Any);
    }
    if let Some(address) = literal(&lowered) {
        return Ok(Host::Ip(address));
    }
    if let Some(rest) = lowered.strip_prefix("*.") {
        if is_host(rest) {
            return Ok(Host::Wildcard(rest.to_string()));
        }
    } else if is_host(&lowered) {
        return Ok(Host::Name(lowered));
    }
    Err(format!(
        "permission `net` entry {text:?} must be a host, an IP address or `*.` and a host"
    ))
}

/// The address an entry spells, when it spells one: `::1` and `127.0.0.1` as
/// they are, and the `[::1]` form a URL host uses.
fn literal(text: &str) -> Option<std::net::IpAddr> {
    let bare = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(text);
    bare.parse().ok()
}

/// Whether `text` is a host name: letters, digits, `-` and `.`, and no empty
/// name.
fn is_host(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Rewrites `path` without `.` or `..`, or `None` when a `..` leaves the root.
pub(crate) fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// `path` as a path inside a folder, or `None` when it is absolute.
fn inside(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() || path.as_os_str().is_empty() {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use crate::plugin::{AnswerAsk, Plugins};

    use super::*;

    /// The `package.json` of a plugin whose `reagent.permissions` is `permissions`.
    fn package(permissions: &str) -> String {
        format!(
            r#"{{"name": "p", "reagent": {{"entry": "index.js", "permissions": {permissions}}}}}"#
        )
    }

    /// Reads the manifest of a plugin whose `package.json` is `text`.
    fn read_manifest(text: &str) -> Found {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("package.json"), text).expect("writes the manifest");
        std::fs::write(dir.path().join("index.js"), "export {};\n").expect("writes the entry");
        read(dir.path())
    }

    /// The permissions of a plugin, which must load.
    fn permissions_of(text: &str) -> Permissions {
        match read_manifest(text) {
            Found::Plugin(manifest) => manifest.permissions,
            Found::Bad(reason) => panic!("refused a plugin it should load: {reason}"),
            Found::Skipped => panic!("skipped a plugin it should load"),
        }
    }

    /// The reason a plugin is refused, which must be one.
    fn refused(text: &str) -> String {
        match read_manifest(text) {
            Found::Bad(reason) => reason,
            Found::Plugin(_) => panic!("loaded a plugin it should refuse"),
            Found::Skipped => panic!("skipped a plugin it should refuse"),
        }
    }

    #[test]
    fn a_relative_entry_stays_inside() {
        assert_eq!(
            normalize(Path::new("a/b.js")),
            Some(PathBuf::from("a/b.js"))
        );
        assert_eq!(normalize(Path::new("./b.js")), Some(PathBuf::from("b.js")));
        assert_eq!(normalize(Path::new("../b.js")), None);
        assert_eq!(inside(Path::new("/etc/passwd")), None);
        assert_eq!(inside(Path::new("")), None);
        assert_eq!(
            inside(Path::new("index.js")),
            Some(PathBuf::from("index.js"))
        );
    }

    #[test]
    fn permissions_missing_grants_nothing() {
        let text = r#"{"name": "p", "reagent": {"entry": "index.js"}}"#;
        assert_eq!(
            permissions_of(text),
            Permissions {
                fs: Fs::None,
                env: Env::None,
                net: Vec::new(),
                run: Vec::new(),
                hooks: Vec::new(),
                sessions: false,
            }
        );
    }

    #[test]
    fn permissions_read_grants_read_anywhere() {
        assert_eq!(
            permissions_of(&package(r#"{"fs": "read"}"#)),
            Permissions {
                fs: Fs::ReadAll,
                env: Env::None,
                net: Vec::new(),
                run: Vec::new(),
                hooks: Vec::new(),
                sessions: false,
            }
        );
    }

    #[test]
    fn permissions_write_grants_write_anywhere() {
        assert_eq!(
            permissions_of(&package(r#"{"fs": "write"}"#)),
            Permissions {
                fs: Fs::WriteAll,
                env: Env::None,
                net: Vec::new(),
                run: Vec::new(),
                hooks: Vec::new(),
                sessions: false,
            }
        );
    }

    #[test]
    fn permissions_an_empty_block_grants_nothing() {
        assert_eq!(
            permissions_of(&package("{}")),
            Permissions {
                fs: Fs::None,
                env: Env::None,
                net: Vec::new(),
                run: Vec::new(),
                hooks: Vec::new(),
                sessions: false,
            }
        );
    }

    #[test]
    fn permissions_lists_keep_their_roots_unresolved() {
        let permissions = permissions_of(&package(
            r#"{"fs": {"read": [".", "./a/b", "~", "~/n", "/abs"], "write": ["out"]}}"#,
        ));
        assert_eq!(
            permissions.fs,
            Fs::Lists {
                read: vec![
                    Root::Session(PathBuf::new()),
                    Root::Session(PathBuf::from("a/b")),
                    Root::Home(PathBuf::new()),
                    Root::Home(PathBuf::from("n")),
                    Root::Absolute(PathBuf::from("/abs")),
                ],
                write: vec![Root::Session(PathBuf::from("out"))],
            }
        );
        assert_eq!(permissions.env, Env::None);
    }

    #[test]
    fn permissions_env_true_grants_every_name() {
        assert_eq!(permissions_of(&package(r#"{"env": true}"#)).env, Env::All);
    }

    #[test]
    fn permissions_env_false_grants_none() {
        assert_eq!(permissions_of(&package(r#"{"env": false}"#)).env, Env::None);
    }

    #[test]
    fn permissions_env_list_keeps_its_names() {
        assert_eq!(
            permissions_of(&package(r#"{"env": ["EDITOR", "_x9"]}"#)).env,
            Env::Names(vec!["EDITOR".to_string(), "_x9".to_string()])
        );
    }

    #[test]
    fn permissions_unknown_key_names_it() {
        assert_eq!(
            refused(&package(r#"{"gpu": true}"#)),
            "unknown permission `gpu`"
        );
    }

    #[test]
    fn call_gate_hooks_permission_keeps_its_names() {
        assert_eq!(
            permissions_of(&package(r#"{"hooks": ["tool.call", "turn.end"]}"#)).hooks,
            vec!["tool.call".to_owned(), "turn.end".to_owned()]
        );
        assert!(permissions_of(&package("{}")).hooks.is_empty());
    }

    #[test]
    fn call_gate_an_unknown_hook_name_names_it() {
        let reason = refused(&package(r#"{"hooks": ["tool.clal"]}"#));
        assert!(reason.contains("tool.clal"), "{reason}");
        assert!(reason.contains("`hooks`"), "{reason}");
        assert!(reason.contains("tool.call"), "{reason}");
    }

    #[test]
    fn call_gate_hooks_permission_must_be_a_list_of_names() {
        let reason = refused(&package(r#"{"hooks": "tool.call"}"#));
        assert!(reason.contains("`hooks`"), "{reason}");
        let reason = refused(&package(r#"{"hooks": [7]}"#));
        assert!(reason.contains("`hooks`"), "{reason}");
    }

    #[test]
    fn permissions_unknown_fs_key_names_it() {
        let reason = refused(&package(r#"{"fs": {"execute": ["out"]}}"#));
        assert!(reason.contains("execute"), "{reason}");
        assert!(reason.contains("fs"), "{reason}");
    }

    #[test]
    fn permissions_bad_fs_type_names_it() {
        let reason = refused(&package(r#"{"fs": 7}"#));
        assert!(reason.contains("`fs`"), "{reason}");
        assert!(reason.contains("read"), "{reason}");
        let reason = refused(&package(r#"{"fs": "execute"}"#));
        assert!(reason.contains("`fs`"), "{reason}");
        let reason = refused(&package(r#"{"fs": {"read": "."}}"#));
        assert!(reason.contains("`fs.read`"), "{reason}");
    }

    #[test]
    fn permissions_env_name_must_be_an_identifier() {
        let reason = refused(&package(r#"{"env": ["1X"]}"#));
        assert!(reason.contains("`env`"), "{reason}");
        assert!(reason.contains("1X"), "{reason}");
        let reason = refused(&package(r#"{"env": "EDITOR"}"#));
        assert!(reason.contains("`env`"), "{reason}");
    }

    #[test]
    fn permissions_a_dotdot_component_refuses() {
        let reason = refused(&package(r#"{"fs": {"read": ["../etc"]}}"#));
        assert!(reason.contains("`fs.read`"), "{reason}");
        assert!(reason.contains("up a folder"), "{reason}");
        let reason = refused(&package(r#"{"fs": {"write": ["~/../x"]}}"#));
        assert!(reason.contains("`fs.write`"), "{reason}");
        assert!(reason.contains("up a folder"), "{reason}");
        assert!(refused(&package(r#"{"fs": {"read": ["/a/../b"]}}"#)).contains("up a folder"));
    }

    #[test]
    fn permissions_an_empty_path_refuses() {
        let reason = refused(&package(r#"{"fs": {"read": [""]}}"#));
        assert!(reason.contains("`fs.read`"), "{reason}");
        assert!(reason.contains("empty"), "{reason}");
    }

    #[test]
    fn permissions_another_users_home_refuses() {
        let reason = refused(&package(r#"{"fs": {"read": ["~bob"]}}"#));
        assert!(reason.contains("`fs.read`"), "{reason}");
        assert!(reason.contains("~bob"), "{reason}");
    }

    #[test]
    fn net_hosts_keep_their_names() {
        assert_eq!(
            permissions_of(&package(r#"{"net": ["api.github.com", "localhost"]}"#)).net,
            vec![
                Host::Name("api.github.com".into()),
                Host::Name("localhost".into()),
            ]
        );
    }

    #[test]
    fn net_wildcard_keeps_the_part_after_the_dot() {
        assert_eq!(
            permissions_of(&package(r#"{"net": ["*.example.com"]}"#)).net,
            vec![Host::Wildcard("example.com".into())]
        );
    }

    #[test]
    fn net_ips_keep_their_address() {
        assert_eq!(
            permissions_of(&package(
                r#"{"net": ["127.0.0.1", "::1", "[::1]", "[::ffff:1.2.3.4]"]}"#
            ))
            .net,
            vec![
                Host::Ip("127.0.0.1".parse().unwrap()),
                Host::Ip("::1".parse().unwrap()),
                Host::Ip("::1".parse().unwrap()),
                Host::Ip("::ffff:1.2.3.4".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn net_uppercase_is_lowered() {
        assert_eq!(
            permissions_of(&package(r#"{"net": ["API.GitHub.COM", "*.Example.COM"]}"#)).net,
            vec![
                Host::Name("api.github.com".into()),
                Host::Wildcard("example.com".into()),
            ]
        );
    }

    #[test]
    fn net_missing_grants_no_host() {
        assert_eq!(permissions_of(&package("{}")).net, Vec::new());
    }

    #[test]
    fn net_url_refuses() {
        let reason = refused(&package(r#"{"net": ["https://api.github.com/x"]}"#));
        assert_eq!(
            reason,
            "permission `net` entry \"https://api.github.com/x\" must be a host, an IP address or `*.` and a host"
        );
    }

    #[test]
    fn net_port_refuses() {
        let reason = refused(&package(r#"{"net": ["example.com:443"]}"#));
        assert!(reason.contains("example.com:443"), "{reason}");
        assert!(reason.contains("must be a host"), "{reason}");
    }

    #[test]
    fn net_empty_string_refuses() {
        let reason = refused(&package(r#"{"net": [""]}"#));
        assert!(reason.contains("permission `net`"), "{reason}");
        assert!(reason.contains("must be a host"), "{reason}");
    }

    #[test]
    fn net_star_is_any_host() {
        assert_eq!(
            permissions_of(&package(
                r#"{"net": ["*", "api.github.com", "*.example.com", "127.0.0.1"]}"#
            ))
            .net,
            vec![
                Host::Any,
                Host::Name("api.github.com".into()),
                Host::Wildcard("example.com".into()),
                Host::Ip("127.0.0.1".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn net_star_double_refuses() {
        for entry in ["**", "*x", "*.*"] {
            let reason = refused(&package(&format!(r#"{{"net": ["{entry}"]}}"#)));
            assert!(reason.contains("must be a host"), "{reason}");
            assert!(reason.contains(entry), "{reason}");
        }
    }

    #[test]
    fn net_star_dot_alone_refuses() {
        let reason = refused(&package(r#"{"net": ["*."]}"#));
        assert!(reason.contains("\"*.\""), "{reason}");
        assert!(reason.contains("must be a host"), "{reason}");
    }

    #[test]
    fn net_not_an_array_refuses() {
        let reason = refused(&package(r#"{"net": "example.com"}"#));
        assert!(reason.contains("permission `net`"), "{reason}");
        assert!(reason.contains("list"), "{reason}");
    }

    #[test]
    fn run_missing_grants_no_program() {
        assert_eq!(permissions_of(&package("{}")).run, Vec::new());
    }

    #[test]
    fn run_names_and_paths_are_kept() {
        assert_eq!(
            permissions_of(&package(r#"{"run": ["git", "/usr/bin/env"]}"#)).run,
            vec![
                Program::Name("git".into()),
                Program::Path(PathBuf::from("/usr/bin/env")),
            ]
        );
    }

    #[test]
    fn run_a_relative_path_refuses() {
        assert_eq!(
            refused(&package(r#"{"run": ["./x"]}"#)),
            "permission `run` entry \"./x\" must be a program name or an absolute path"
        );
    }

    #[test]
    fn run_an_entry_with_a_space_refuses() {
        assert_eq!(
            refused(&package(r#"{"run": ["git status"]}"#)),
            "permission `run` entry \"git status\" must be a program name or an absolute path"
        );
    }

    #[test]
    fn run_a_non_list_refuses() {
        let reason = refused(&package(r#"{"run": "git"}"#));
        assert!(reason.contains("permission `run`"), "{reason}");
        assert!(reason.contains("list"), "{reason}");
    }

    #[test]
    fn run_an_empty_string_refuses() {
        assert_eq!(
            refused(&package(r#"{"run": [""]}"#)),
            "permission `run` entry \"\" must be a program name or an absolute path"
        );
    }

    #[test]
    fn gateway_role_parses() {
        let text = r#"{"name": "p", "reagent": {"entry": "index.js", "role": "gateway"}}"#;
        match read_manifest(text) {
            Found::Plugin(manifest) => assert_eq!(manifest.role, Role::Gateway),
            Found::Bad(reason) => panic!("refused a gateway plugin: {reason}"),
            Found::Skipped => panic!("skipped a gateway plugin"),
        }
    }

    #[test]
    fn gateway_role_is_normal_when_unset() {
        assert_eq!(role_of(&package("{}")), Role::Normal);
    }

    #[test]
    fn gateway_unknown_role_names_it() {
        let text = r#"{"name": "p", "reagent": {"entry": "index.js", "role": "worker"}}"#;
        let reason = refused(text);
        assert!(reason.contains("worker"), "{reason}");
        assert!(reason.contains("role"), "{reason}");
    }

    #[tokio::test]
    async fn gateway_plugin_is_not_loaded_by_a_normal_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let plugin = root.join("p");
        std::fs::create_dir_all(&plugin).expect("makes the plugin folder");
        std::fs::write(
            plugin.join("package.json"),
            r#"{"name": "p", "reagent": {"entry": "index.js", "role": "gateway"}}"#,
        )
        .expect("writes the manifest");
        std::fs::write(
            plugin.join("index.js"),
            "export default function (reagent) { reagent.registerTool({ name: \"from_gateway\", run: () => \"\" }); }\n",
        )
        .expect("writes the entry");
        let (normal, notes) = Plugins::load_in(root, root, root).await;
        assert!(normal.is_none(), "a normal run loaded a gateway plugin");
        assert!(notes.is_empty(), "{notes:?}");
        let (gateway, notes) = Plugins::load_gateway(root, root, None, answers()).await;
        assert!(notes.is_empty(), "{notes:?}");
        let gateway = gateway.expect("the gateway plugin loads");
        assert_eq!(gateway.tools().len(), 1);
    }

    /// The role a plugin names, which must load.
    fn role_of(text: &str) -> Role {
        match read_manifest(text) {
            Found::Plugin(manifest) => manifest.role,
            Found::Bad(reason) => panic!("refused a plugin it should load: {reason}"),
            Found::Skipped => panic!("skipped a plugin it should load"),
        }
    }

    /// The channel a gateway plugin's answers go to, with no gateway behind it.
    fn answers() -> tokio::sync::mpsc::UnboundedSender<AnswerAsk> {
        let (answers, mut asks) = tokio::sync::mpsc::unbounded_channel();
        // The requests are dropped: a load asks for nothing.
        tokio::spawn(async move { while asks.recv().await.is_some() {} });
        answers
    }

    #[test]
    fn permissions_must_be_an_object() {
        let text = r#"{"name": "p", "reagent": {"entry": "index.js", "permissions": 3}}"#;
        let reason = refused(text);
        assert!(reason.contains("`permissions`"), "{reason}");
        assert!(reason.contains("object"), "{reason}");
    }
}

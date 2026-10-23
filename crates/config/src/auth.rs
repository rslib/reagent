//! The saved logins: `auth.json` in the state folder.
//!
//! The file maps a provider name to a login, and it holds keys, so it is
//! written 0600 and a wider file is refused. Its shape is
//! `{"<provider>": {"type": "key", "key": "<key>"}}`, or
//! `{"<provider>": {"type": "oauth", "client_id": "...", ...}}` for a
//! browser login.
//!
//! An entry it cannot read is skipped, so a login written by a newer reagent
//! never costs the others.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Secret;

/// One saved login. The `type` field names the kind.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Login {
    /// An API key.
    Key {
        /// The key, which its `Debug` hides.
        key: Secret,
    },
    /// A browser login whose access token expires and is refreshed.
    Oauth {
        /// The client id the provider issued, reused by a later login.
        client_id: String,
        /// The `urn:uuid:` id of this agent host, made by the first login.
        host_id: String,
        /// The access token, which its `Debug` hides.
        access: Secret,
        /// The refresh token, which its `Debug` hides.
        refresh: Secret,
        /// The signed identity of the account, which its `Debug` hides.
        id_token: Secret,
        /// The time the access token stops being usable, since the epoch.
        expires_at: u64,
        /// The account id the tokens belong to.
        subject: String,
        /// The account address, when the account shared one.
        email: Option<String>,
    },
    /// A device-code login whose access token expires and is refreshed at
    /// the token endpoint it names.
    Device {
        /// The provider the login is for, as `reagent login` names it.
        provider: String,
        /// The client id the login signed in with.
        client_id: String,
        /// The endpoint a refresh posts to.
        token_url: String,
        /// The access token, which its `Debug` hides.
        access: Secret,
        /// The refresh token, which its `Debug` hides.
        refresh: Secret,
        /// The time the access token stops being usable, since the epoch.
        expires_at: u64,
    },
}

impl Login {
    /// The key this login carries, or `None` for a browser login.
    pub fn key(&self) -> Option<&Secret> {
        match self {
            Login::Key { key } => Some(key),
            Login::Oauth { .. } | Login::Device { .. } => None,
        }
    }
}

/// A login as the file writes it.
///
/// `Secret` has no `Serialize` of its own, so a settings struct serialized
/// later can never print a key; the write path goes through this plain `&str`
/// field instead.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Saved<'a> {
    /// An API key.
    Key {
        /// The key, as plain text, for this write only.
        key: &'a str,
    },
    /// A browser login.
    Oauth {
        /// The issued client id.
        client_id: &'a str,
        /// The agent host id.
        host_id: &'a str,
        /// The access token, as plain text, for this write only.
        access: &'a str,
        /// The refresh token, as plain text, for this write only.
        refresh: &'a str,
        /// The signed identity, as plain text, for this write only.
        id_token: &'a str,
        /// The time the access token stops being usable.
        expires_at: u64,
        /// The account id.
        subject: &'a str,
        /// The account address.
        email: Option<&'a str>,
    },
    /// A device-code login.
    Device {
        /// The provider name.
        provider: &'a str,
        /// The client id.
        client_id: &'a str,
        /// The refresh endpoint.
        token_url: &'a str,
        /// The access token, as plain text, for this write only.
        access: &'a str,
        /// The refresh token, as plain text, for this write only.
        refresh: &'a str,
        /// The time the access token stops being usable.
        expires_at: u64,
    },
}

impl<'a> Saved<'a> {
    /// The login as the file writes it, with its secrets borrowed for the
    /// write only.
    fn of(login: &'a Login) -> Self {
        match login {
            Login::Key { key } => Saved::Key { key: key.expose() },
            Login::Oauth {
                client_id,
                host_id,
                access,
                refresh,
                id_token,
                expires_at,
                subject,
                email,
            } => Saved::Oauth {
                client_id,
                host_id,
                access: access.expose(),
                refresh: refresh.expose(),
                id_token: id_token.expose(),
                expires_at: *expires_at,
                subject,
                email: email.as_deref(),
            },
            Login::Device {
                provider,
                client_id,
                token_url,
                access,
                refresh,
                expires_at,
            } => Saved::Device {
                provider,
                client_id,
                token_url,
                access: access.expose(),
                refresh: refresh.expose(),
                expires_at: *expires_at,
            },
        }
    }
}

/// The saved logins, by provider name.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Logins(BTreeMap<String, Login>);

impl Logins {
    /// The login of `name`, if one is saved.
    pub fn get(&self, name: &str) -> Option<&Login> {
        self.0.get(name)
    }

    /// Saves `login` under `name`, replacing any earlier one.
    pub fn set(&mut self, name: &str, login: Login) {
        self.0.insert(name.to_string(), login);
    }

    /// Drops the login of `name`; true when one was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.0.remove(name).is_some()
    }

    /// Whether no login is saved.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The auth file of the folder `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("auth.json")
}

/// The folder of the saved logins: `$XDG_STATE_HOME/reagent`, else
/// `$HOME/.local/state/reagent`.
///
/// Not the config folder, which dotfile tools manage and may keep in a git
/// repo. An auth file left in the config folder by an older reagent moves
/// here on the first call that finds none here.
pub fn dir() -> Result<PathBuf, String> {
    let dir = dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .ok_or_else(|| "no state folder; set HOME or XDG_STATE_HOME".to_string())?;
    if let Some(old) = crate::global_dir() {
        move_old(&old, &dir)?;
    }
    Ok(dir)
}

fn dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home.filter(|value| !value.is_empty())?).join(".local/state"),
    };
    Some(base.join("reagent"))
}

/// Moves the auth file of `old` to `new` when only `old` has one.
///
/// Read and write go through [`read`] and [`write`], so a wide file is
/// refused and the new one is 0600 even across file systems.
#[expect(
    clippy::disallowed_methods,
    reason = "the auth file moves before the runtime starts"
)]
fn move_old(old: &Path, new: &Path) -> Result<(), String> {
    let from = path(old);
    if old == new || path(new).exists() || !from.exists() {
        return Ok(());
    }
    write(new, &read(old)?)?;
    std::fs::remove_file(&from).map_err(|error| format!("{}: {error}", from.display()))?;
    let _ = std::fs::remove_file(old.join("auth.json.lock"));
    Ok(())
}

/// Reads the logins of the folder `dir`. A missing file holds none.
#[expect(
    clippy::disallowed_types,
    reason = "the auth file is read before the runtime starts"
)]
pub fn read(dir: &Path) -> Result<Logins, String> {
    use std::io::Read;

    let path = path(dir);
    let fail = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Logins::default());
        }
        Err(error) => return Err(fail(error)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata().map_err(fail)?.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{0}: readable by others; run chmod 600 {0}",
                path.display()
            ));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    parse(&path, &text)
}

/// The logins of `text`, with the entries this version cannot read left out.
///
/// The error names the file and the place of the bad byte, never the text: it
/// may hold a key.
fn parse(path: &Path, text: &str) -> Result<Logins, String> {
    let entries: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(text).map_err(|error| {
            format!(
                "{}: not valid JSON at line {} column {}",
                path.display(),
                error.line(),
                error.column()
            )
        })?;
    let mut logins = Logins::default();
    for (name, entry) in entries {
        // A type this version does not know, or a broken entry, stays out of
        // the map, and the rest of the file still loads.
        if let Ok(login) = serde_json::from_value::<Login>(entry) {
            logins.set(&name, login);
        }
    }
    Ok(logins)
}

/// Writes `logins` to the auth file of `dir`, readable by you only.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "the auth file is written before the runtime starts"
)]
pub fn write(dir: &Path, logins: &Logins) -> Result<(), String> {
    use std::io::Write;

    let path = path(dir);
    let fail = |error: std::io::Error| format!("{}: {error}", path.display());
    let text = serialize(&path, logins)?;
    if !dir.is_dir() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let temp = dir.join(format!("auth.json.tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(fail)?;
    drop(file);
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        fail(error)
    })
}

/// The whole file as JSON, pretty, one entry per line.
fn serialize(path: &Path, logins: &Logins) -> Result<String, String> {
    let saved: BTreeMap<&str, Saved<'_>> = logins
        .0
        .iter()
        .map(|(name, login)| (name.as_str(), Saved::of(login)))
        .collect();
    serde_json::to_string_pretty(&saved)
        .map(|text| text + "\n")
        .map_err(|_| format!("{}: cannot write the logins", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn write_raw(dir: &Path, text: &str) -> PathBuf {
        let path = path(dir);
        std::fs::create_dir_all(dir).expect("makes the folder");
        std::fs::write(&path, text).expect("writes the file");
        path
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmods");
    }

    #[cfg(unix)]
    #[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).expect("stats").permissions().mode() & 0o777
    }

    fn key(text: &str) -> Login {
        Login::Key {
            key: Secret::from(text),
        }
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_saves_and_reads_a_key_with_mode_600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = Logins::default();
        logins.set("openrouter", key("sk-or-1"));
        write(dir.path(), &logins).expect("writes");
        let path = path(dir.path());
        assert_eq!(mode(&path), 0o600);
        let read_back = read(dir.path()).expect("reads");
        assert_eq!(read_back, logins);
        assert_eq!(read_back.get("openrouter"), Some(&key("sk-or-1")));
        assert_eq!(
            read_back
                .get("openrouter")
                .unwrap()
                .key()
                .expect("a key")
                .expose(),
            "sk-or-1"
        );
        assert!(read_back.get("nope").is_none());
    }

    #[test]
    fn auth_file_debug_hides_the_key() {
        let mut logins = Logins::default();
        logins.set("openrouter", key("sk-or-secret"));
        let text = format!("{logins:?}");
        assert!(!text.contains("sk-or-secret"), "{text}");
        assert!(text.contains("openrouter"), "{text}");
    }

    #[test]
    fn auth_file_missing_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let logins = read(dir.path()).expect("reads");
        assert!(logins.is_empty());
        assert_eq!(logins, Logins::default());
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_readable_by_others_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(
            dir.path(),
            "{\"openrouter\": {\"type\": \"key\", \"key\": \"sk-1\"}}\n",
        );
        chmod(&path, 0o600);
        assert!(read(dir.path()).is_ok());
        chmod(&path, 0o644);
        let error = read(dir.path()).unwrap_err();
        assert!(error.contains("auth.json"), "{error}");
        assert!(error.contains("chmod 600"), "{error}");
        assert!(!error.contains("sk-1"), "{error}");
    }

    #[test]
    fn auth_file_remove_then_write_drops_the_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = Logins::default();
        logins.set("openrouter", key("sk-or-1"));
        logins.set("other", key("sk-2"));
        write(dir.path(), &logins).expect("writes");
        let mut logins = read(dir.path()).expect("reads");
        assert!(logins.remove("openrouter"));
        assert!(!logins.remove("openrouter"));
        write(dir.path(), &logins).expect("writes");
        let logins = read(dir.path()).expect("reads");
        assert!(logins.get("openrouter").is_none());
        assert_eq!(logins.get("other"), Some(&key("sk-2")));
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_skips_an_unknown_type_and_keeps_the_others() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(
            dir.path(),
            concat!(
                "{\n",
                "  \"future\": {\"type\": \"oauth\", \"token\": \"t\"},\n",
                "  \"broken\": {\"type\": \"key\"},\n",
                "  \"openrouter\": {\"type\": \"key\", \"key\": \"sk-or-1\"}\n",
                "}\n",
            ),
        );
        chmod(&path, 0o600);
        let logins = read(dir.path()).expect("reads");
        assert!(logins.get("future").is_none());
        assert!(logins.get("broken").is_none());
        assert_eq!(logins.get("openrouter"), Some(&key("sk-or-1")));
        let mut expected = Logins::default();
        expected.set("openrouter", key("sk-or-1"));
        assert_eq!(logins, expected);
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_skips_an_entry_that_is_not_an_object() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(
            dir.path(),
            concat!(
                "{\n",
                "  \"scalar\": 5,\n",
                "  \"openrouter\": {\"type\": \"key\", \"key\": \"sk-or-1\"}\n",
                "}\n",
            ),
        );
        chmod(&path, 0o600);
        let logins = read(dir.path()).expect("reads");
        assert!(logins.get("scalar").is_none());
        assert_eq!(logins.get("openrouter"), Some(&key("sk-or-1")));
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_bad_json_names_the_line_and_not_the_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(
            dir.path(),
            "{\n  \"openrouter\": {\"type\": \"key\" \"key\": \"sk-secret\"}\n}\n",
        );
        chmod(&path, 0o600);
        let error = read(dir.path()).unwrap_err();
        assert!(error.contains("auth.json"), "{error}");
        assert!(error.contains("not valid JSON"), "{error}");
        assert!(error.contains("line 2 column "), "{error}");
        assert!(!error.contains("sk-secret"), "{error}");
        assert!(!error.contains("openrouter"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_reads_escapes_and_writes_them_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(dir.path(), r#"{"a\"b":{"type":"key","key":"sk-\u00e9\n"}}"#);
        chmod(&path, 0o600);
        let logins = read(dir.path()).expect("reads");
        assert_eq!(
            logins
                .get("a\"b")
                .and_then(|login| login.key())
                .map(|key| key.expose()),
            Some("sk-\u{e9}\n")
        );
        write(dir.path(), &logins).expect("writes");
        assert_eq!(read(dir.path()).expect("reads"), logins);
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_write_creates_the_folder_with_mode_700() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("reagent");
        let mut logins = Logins::default();
        logins.set("openrouter", key("sk-or-1"));
        write(&folder, &logins).expect("writes");
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(read(&folder).expect("reads"), logins);
    }

    /// A browser login, with the tokens that its `Debug` must hide.
    fn oauth(access: &str, refresh: &str, id_token: &str) -> Login {
        Login::Oauth {
            client_id: "oaiapp_1".into(),
            host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
            access: Secret::from(access),
            refresh: Secret::from(refresh),
            id_token: Secret::from(id_token),
            expires_at: 1_800_000_000,
            subject: "user-1".into(),
            email: Some("a@b.c".into()),
        }
    }

    #[cfg(unix)]
    #[test]
    #[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
    fn device_login_round_trips_and_hides_its_tokens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = Logins::default();
        let login = Login::Device {
            provider: "radius".into(),
            client_id: "pi-gateway".into(),
            token_url: "https://radius.pi.dev/v1/oauth/token".into(),
            access: Secret::from("access-1"),
            refresh: Secret::from("refresh-1"),
            expires_at: 1_800_000_000,
        };
        assert!(!format!("{login:?}").contains("access-1"));
        assert_eq!(login.key(), None);
        logins.set("radius", login);
        write(dir.path(), &logins).expect("writes");
        assert_eq!(mode(&path(dir.path())), 0o600);
        let text = std::fs::read_to_string(path(dir.path())).expect("reads the file");
        assert!(text.contains("\"type\": \"device\""), "{text}");
        assert_eq!(read(dir.path()).expect("reads"), logins);
    }

    #[cfg(unix)]
    #[test]
    #[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
    fn auth_file_oauth_round_trips_with_plain_token_strings() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = Logins::default();
        logins.set("chatgpt", oauth("access-1", "refresh-1", "id-1"));
        write(dir.path(), &logins).expect("writes");
        let path = path(dir.path());
        assert_eq!(mode(&path), 0o600);
        let text = std::fs::read_to_string(&path).expect("reads the file");
        assert!(text.contains("\"type\": \"oauth\""), "{text}");
        assert!(text.contains("\"access\": \"access-1\""), "{text}");
        assert!(text.contains("\"refresh\": \"refresh-1\""), "{text}");
        assert!(text.contains("\"expires_at\": 1800000000"), "{text}");
        let read_back = read(dir.path()).expect("reads");
        assert_eq!(read_back, logins);
        let Some(Login::Oauth {
            client_id,
            host_id,
            access,
            refresh,
            id_token,
            expires_at,
            subject,
            email,
        }) = read_back.get("chatgpt")
        else {
            panic!("an oauth login");
        };
        assert_eq!(client_id, "oaiapp_1");
        assert!(host_id.starts_with("urn:uuid:"), "{host_id}");
        assert_eq!(access.expose(), "access-1");
        assert_eq!(refresh.expose(), "refresh-1");
        assert_eq!(id_token.expose(), "id-1");
        assert_eq!(*expires_at, 1_800_000_000);
        assert_eq!(subject, "user-1");
        assert_eq!(email.as_deref(), Some("a@b.c"));
        assert!(read_back.get("chatgpt").unwrap().key().is_none());
    }

    #[test]
    fn auth_file_oauth_debug_and_errors_hold_no_token() {
        let mut logins = Logins::default();
        logins.set(
            "chatgpt",
            oauth("access-secret", "refresh-secret", "id-secret"),
        );
        let text = format!("{logins:?}");
        for secret in ["access-secret", "refresh-secret", "id-secret"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("chatgpt"), "{text}");
        assert!(text.contains("oaiapp_1"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_oauth_a_file_of_keys_still_loads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_raw(
            dir.path(),
            concat!(
                "{\n",
                "  \"openrouter\": {\"type\": \"key\", \"key\": \"sk-or-1\"},\n",
                "  \"chatgpt\": {\"type\": \"oauth\", \"access\": \"a\"}\n",
                "}\n",
            ),
        );
        chmod(&path, 0o600);
        let logins = read(dir.path()).expect("reads");
        // A browser login with fields missing holds no usable token, so it is
        // skipped and the key next to it still loads.
        assert!(logins.get("chatgpt").is_none());
        assert_eq!(logins.get("openrouter"), Some(&key("sk-or-1")));
    }
    #[test]
    fn auth_state_dir_prefers_xdg_state_home() {
        assert_eq!(
            dir_from(Some("/s".into()), Some("/h".into())),
            Some(PathBuf::from("/s/reagent"))
        );
        assert_eq!(
            dir_from(Some("".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.local/state/reagent"))
        );
        assert_eq!(
            dir_from(None, Some("/h".into())),
            Some(PathBuf::from("/h/.local/state/reagent"))
        );
        assert_eq!(dir_from(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn auth_state_dir_moves_the_old_file_once() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let old = tmp.path().join("config/reagent");
        let new = tmp.path().join("state/reagent");
        let from = write_raw(&old, r#"{"openrouter":{"type":"key","key":"k1"}}"#);
        chmod(&from, 0o600);
        move_old(&old, &new).expect("moves");
        assert!(!from.exists());
        assert_eq!(mode(&path(&new)), 0o600);
        let logins = read(&new).expect("reads");
        assert!(
            matches!(logins.get("openrouter"), Some(Login::Key { key }) if key.expose() == "k1")
        );

        let again = write_raw(&old, r#"{"openrouter":{"type":"key","key":"k2"}}"#);
        chmod(&again, 0o600);
        move_old(&old, &new).expect("keeps the new file");
        assert!(again.exists(), "a file already in the state folder wins");
        let logins = read(&new).expect("reads");
        assert!(
            matches!(logins.get("openrouter"), Some(Login::Key { key }) if key.expose() == "k1")
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_state_dir_refuses_to_move_a_wide_file() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let old = tmp.path().join("config/reagent");
        let new = tmp.path().join("state/reagent");
        let from = write_raw(&old, r#"{}"#);
        chmod(&from, 0o644);
        assert!(move_old(&old, &new).unwrap_err().contains("chmod 600"));
        assert!(from.exists());
        assert!(!path(&new).exists());
    }
}

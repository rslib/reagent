//! The lockfile of an installed plugin: where it came from, the sha256 of
//! every file it holds and the files its bare imports name.
//!
//! `reagent plugin add` writes it; each load checks every file against it, so
//! a plugin runs only the bytes its install fetched. A plugin folder with no
//! lockfile is one the user wrote, and loads as before.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The name of the lockfile in a plugin's folder.
pub const FILE: &str = "reagent-lock.json";

/// The format this build writes and reads.
pub const VERSION: u32 = 1;

/// One plugin's lockfile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    /// The format of the file, [`VERSION`].
    pub version: u32,
    /// What the user asked to install, as typed, such as `npm:name@^1.2` or
    /// `git:github.com/owner/repo@v1.2`: an update fetches it again.
    pub requested: String,
    /// The exact source it resolved to, such as `npm:name@1.2.3` or
    /// `git:github.com/owner/repo@v1.2`.
    pub source: String,
    /// The commit a git source resolved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The sha256 (lowercase hex) of every file of the plugin, by its path
    /// relative to the plugin's folder, with `/` between parts.
    pub files: BTreeMap<String, String>,
    /// The file each bare import name loads, as a path relative to the
    /// plugin's folder: `zod` or `zod/v4` to the module the install fetched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub imports: BTreeMap<String, String>,
}

impl Lock {
    /// Checks every file of the lock against the bytes on disk in `folder`.
    ///
    /// The error names the first file that is missing, unreadable or changed,
    /// and a path that would leave the folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread or in spawn_blocking, off the async runtime"
    )]
    pub fn verify(&self, folder: &Path) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!("{FILE}: version {} is not {VERSION}", self.version));
        }
        for (path, want) in &self.files {
            if !inside(path) {
                return Err(format!(
                    "{FILE}: the path {path:?} leaves the plugin folder"
                ));
            }
            let bytes =
                std::fs::read(folder.join(path)).map_err(|error| format!("{path}: {error}"))?;
            if hash(&bytes) != *want {
                return Err(format!("{path}: the file differs from {FILE}"));
            }
        }
        for (name, path) in &self.imports {
            if !self.files.contains_key(path) {
                return Err(format!(
                    "{FILE}: the import {name:?} names {path:?}, which the lock does not pin"
                ));
            }
        }
        Ok(())
    }

    /// The file, relative to the plugin's folder, that the bare import `name`
    /// loads.
    pub fn import(&self, name: &str) -> Option<&str> {
        self.imports.get(name).map(String::as_str)
    }
}

/// Reads the lockfile of the plugin in `folder`: `Ok(None)` when it has none.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the plugin thread or in spawn_blocking, off the async runtime"
)]
pub fn read(folder: &Path) -> Result<Option<Lock>, String> {
    let bytes = match std::fs::read(folder.join(FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{FILE}: {error}")),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("{FILE}: {error}"))
}

/// Writes `lock` into `folder`, under a temp name renamed in, so a reader sees
/// the old file or the new one.
#[expect(
    clippy::disallowed_methods,
    reason = "runs in spawn_blocking, off the async runtime"
)]
pub fn write(folder: &Path, lock: &Lock) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(lock).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let temp = folder.join(format!("{FILE}.tmp"));
    std::fs::write(&temp, &bytes)?;
    std::fs::rename(&temp, folder.join(FILE))
}

/// The sha256 of `bytes`, as lowercase hex.
pub fn hash(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
    out
}

/// Whether `path` is a relative path that stays inside the folder it is
/// joined to: parts split by `/`, none empty, `.` or `..`, no root and no
/// `\`.
pub fn inside(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("reagent-lock-")
            .tempdir()
            .expect("a temp folder")
    }

    fn lock_of(files: &[(&str, &[u8])]) -> Lock {
        Lock {
            version: VERSION,
            requested: "npm:demo@^1".to_owned(),
            source: "npm:demo@1.0.0".to_owned(),
            commit: None,
            files: files
                .iter()
                .map(|(path, bytes)| ((*path).to_owned(), hash(bytes)))
                .collect(),
            imports: BTreeMap::new(),
        }
    }

    #[test]
    fn lock_hash_is_the_sha256_in_hex() {
        assert_eq!(
            hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn lock_a_folder_without_one_reads_none() {
        let dir = folder();
        assert_eq!(read(dir.path()), Ok(None));
    }

    #[test]
    fn lock_written_reads_back_the_same() {
        let dir = folder();
        let mut lock = lock_of(&[("index.js", b"export default 1")]);
        lock.commit = Some("0123abc".to_owned());
        lock.imports
            .insert("zod".to_owned(), "deps/zod.mjs".to_owned());
        write(dir.path(), &lock).expect("writes");
        assert_eq!(read(dir.path()), Ok(Some(lock)));
        assert!(!dir.path().join(format!("{FILE}.tmp")).exists());
    }

    #[test]
    fn lock_verify_passes_on_the_fetched_bytes() {
        let dir = folder();
        std::fs::create_dir_all(dir.path().join("deps")).unwrap();
        std::fs::write(dir.path().join("index.js"), "export default 1").unwrap();
        std::fs::write(dir.path().join("deps/zod.mjs"), "export const z = 1").unwrap();
        let mut lock = lock_of(&[
            ("index.js", b"export default 1"),
            ("deps/zod.mjs", b"export const z = 1"),
        ]);
        lock.imports
            .insert("zod".to_owned(), "deps/zod.mjs".to_owned());
        assert_eq!(lock.verify(dir.path()), Ok(()));
        assert_eq!(lock.import("zod"), Some("deps/zod.mjs"));
        assert_eq!(lock.import("yaml"), None);
    }

    #[test]
    fn lock_verify_names_a_changed_file() {
        let dir = folder();
        std::fs::write(dir.path().join("index.js"), "export default 2").unwrap();
        let lock = lock_of(&[("index.js", b"export default 1")]);
        let error = lock.verify(dir.path()).unwrap_err();
        assert!(error.starts_with("index.js:"), "{error}");
    }

    #[test]
    fn lock_verify_names_a_missing_file() {
        let dir = folder();
        let lock = lock_of(&[("index.js", b"export default 1")]);
        let error = lock.verify(dir.path()).unwrap_err();
        assert!(error.starts_with("index.js:"), "{error}");
    }

    #[test]
    fn lock_verify_refuses_a_path_out_of_the_folder() {
        let dir = folder();
        for path in [
            "../x.js",
            "/etc/passwd",
            "a/../../x.js",
            "a//b.js",
            "./a.js",
            "a\\b.js",
            "",
        ] {
            let lock = lock_of(&[(path, b"x")]);
            let error = lock.verify(dir.path()).unwrap_err();
            assert!(
                error.contains("leaves the plugin folder"),
                "{path}: {error}"
            );
        }
    }

    #[test]
    fn lock_verify_refuses_an_import_the_lock_does_not_pin() {
        let dir = folder();
        std::fs::write(dir.path().join("index.js"), "x").unwrap();
        let mut lock = lock_of(&[("index.js", b"x")]);
        lock.imports
            .insert("zod".to_owned(), "deps/zod.mjs".to_owned());
        let error = lock.verify(dir.path()).unwrap_err();
        assert!(error.contains("does not pin"), "{error}");
    }

    #[test]
    fn lock_verify_refuses_another_version() {
        let dir = folder();
        let mut lock = lock_of(&[]);
        lock.version = 2;
        assert!(lock.verify(dir.path()).is_err());
    }
}

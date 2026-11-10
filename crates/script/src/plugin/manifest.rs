//! Reading a plugin folder's `package.json`.

use std::path::{Component, Path, PathBuf};

/// What one folder under the plugins root holds.
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
    let Some(entry) = object
        .get("reagent")
        .and_then(|reagent| reagent.get("entry"))
        .and_then(|entry| entry.as_str())
    else {
        return Found::Skipped;
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
    })
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
mod tests {
    use super::*;

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
}

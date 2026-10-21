//! Lexical path handling.

use std::path::{Component, Path, PathBuf};

/// `path` with `.` and `..` resolved by text, so `src/../secrets` is
/// `secrets`. Symbolic links are not followed.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(path: &str) -> PathBuf {
        normalize(Path::new(path))
    }

    #[test]
    fn resolves_dots() {
        assert_eq!(norm("src/../secrets"), PathBuf::from("secrets"));
        assert_eq!(norm("./a/./b"), PathBuf::from("a/b"));
        assert_eq!(norm("/a/b/../../c"), PathBuf::from("/c"));
    }

    #[test]
    fn parent_of_root_stays_at_root() {
        assert_eq!(norm("/../a"), PathBuf::from("/a"));
    }

    #[test]
    fn leading_parents_of_a_relative_path_are_dropped() {
        assert_eq!(norm("../a"), PathBuf::from("a"));
    }

    #[test]
    fn empty_and_dot_are_empty() {
        assert_eq!(norm(""), PathBuf::new());
        assert_eq!(norm("."), PathBuf::new());
    }
}

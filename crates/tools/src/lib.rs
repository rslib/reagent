//! Native tools: read, write, edit, bash, grep, find.

use std::path::{Path, PathBuf};

mod atomic;
mod blocking;
mod gitignore;
mod process_group;
mod walk;

pub mod bash;
pub mod bash_read;
pub mod edit;
pub mod find;
pub mod grep;
pub mod read;
pub mod repair;
pub mod spill;
pub mod workspace;
pub mod write;

/// Resolves a tool path: an absolute path as is, a relative one under
/// `working_dir`.
fn resolve(working_dir: &Path, path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("invalid arguments: path must not be empty".into());
    }
    Ok(working_dir.join(path))
}

#[cfg(test)]
mod tests {
    use reagent_agent::tool::{EffectClass, Tool as _};

    use crate::bash::Bash;
    use crate::edit::Edit;
    use crate::find::Find;
    use crate::grep::Grep;
    use crate::read::Read;
    use crate::write::Write;

    #[test]
    fn tool_effect_classes() {
        // A path inside a temp folder that the test never makes.
        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path().join("absent");
        assert_eq!(Read::new(&dir).effect(), EffectClass::Read);
        assert_eq!(Grep::new(&dir).effect(), EffectClass::Read);
        assert_eq!(Find::new(&dir).effect(), EffectClass::Read);
        assert_eq!(Edit::new(&dir).effect(), EffectClass::LocalReversible);
        assert_eq!(Write::new(&dir).effect(), EffectClass::LocalReversible);
        assert_eq!(Bash::new(&dir).effect(), EffectClass::LocalReversible);
    }
}

//! Native tools: read, write, edit, bash, grep, find.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

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

/// The files under `root` that git would not ignore: the path of each relative
/// to `root`, with `/` between parts, at most `cap` of them, and whether the
/// list was cut.
///
/// The walk's own skip list and limit apply, so a caller that asks for more
/// than [`walk::MAX_FILES`] still gets no more.
pub async fn list_files(root: &Path, cap: usize) -> (Vec<String>, bool) {
    let listing = match walk::walk(root, &walk::Skip::default(), &CancellationToken::new()).await {
        Ok(listing) => listing,
        Err(walk::Cancelled) => return (Vec::new(), false),
    };
    let cut = listing.truncated || listing.files.len() > cap;
    let mut files = Vec::with_capacity(listing.files.len().min(cap));
    files.extend(listing.files.into_iter().take(cap).map(|file| file.rel));
    (files, cut)
}

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

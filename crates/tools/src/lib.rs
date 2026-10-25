//! Native tools: read, write, edit, bash, grep, find.

use std::path::{Path, PathBuf};

mod atomic;
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

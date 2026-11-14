//! The system prompt template for a headless run.

use std::path::{Path, PathBuf};

use reagent_ai::Text;
use reagent_util::text;

/// The prompt template, baked in at build time from `prompts/system.md`.
const TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/system.md"
));

/// Builds the system prompt for a run rooted at `cwd`, filling the template.
#[cfg(test)]
pub fn system(cwd: &str) -> Text {
    Text::from(TEMPLATE.replace("{cwd}", cwd))
}

/// The most bytes of instruction text added to the system prompt.
const INSTRUCTIONS_LIMIT: usize = 32 * 1024;

/// The note that ends the instructions when the limit cut them.
const CUT_NOTE: &str = "\n[instructions cut at 32 KB]";

/// The label shown for the global instructions file.
const GLOBAL_LABEL: &str = "~/.config/reagent/AGENTS.md";

/// Builds the system prompt for a run rooted at `cwd`, followed by `instructions`.
///
/// An empty `instructions` gives the same text as [`system`].
pub fn system_with(cwd: &str, instructions: &str) -> Text {
    let mut text = TEMPLATE.replace("{cwd}", cwd);
    text.push_str(instructions);
    Text::from(text)
}

/// Collects the project instructions for a run, as one string to append to the system prompt.
///
/// The `global` file comes first, labelled `~/.config/reagent/AGENTS.md`. Then, for each
/// folder from `repo_root` down to `cwd`, that folder's `AGENTS.md`, else its `CLAUDE.md`.
/// With no `repo_root`, only `cwd` is read. Each file becomes a section headed by its path,
/// relative to `repo_root` (or `cwd`). Unreadable files are skipped. The added text stops at
/// 32 KB: the last section is cut and a note ends the text.
pub async fn instructions(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> String {
    let global = global.map(Path::to_path_buf);
    let repo_root = repo_root.map(Path::to_path_buf);
    // The repo root is absolute, so a relative --cwd must be too to sit below it.
    let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    tokio::task::spawn_blocking(move || collect(global.as_deref(), repo_root.as_deref(), &cwd))
        .await
        .unwrap_or_default()
}

fn collect(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> String {
    let mut out = String::new();
    if let Some(path) = global
        && add_section(&mut out, GLOBAL_LABEL, path)
    {
        return out;
    }
    let (base, folders) = match repo_root.and_then(|root| folders_below(root, cwd)) {
        Some(folders) => (repo_root.unwrap_or(cwd), folders),
        None => (cwd, vec![cwd.to_path_buf()]),
    };
    for folder in folders {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = folder.join(name);
            if !path.is_file() {
                continue;
            }
            let label = label_of(base, &path);
            if add_section(&mut out, &label, &path) {
                return out;
            }
            break;
        }
    }
    out
}

/// The folders from `root` down to `cwd`, `None` when `cwd` is not below `root`.
pub(crate) fn folders_below(root: &Path, cwd: &Path) -> Option<Vec<PathBuf>> {
    let rest = cwd.strip_prefix(root).ok()?;
    let mut folders = vec![root.to_path_buf()];
    let mut dir = root.to_path_buf();
    for part in rest.components() {
        dir.push(part);
        folders.push(dir.clone());
    }
    Some(folders)
}

fn label_of(base: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}

/// Appends the section for `path`; returns true when the limit cut it.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn add_section(out: &mut String, label: &str, path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let section = format!(
        "\n\n# Instructions from {label}\n\n{}",
        String::from_utf8_lossy(&bytes)
    );
    let room = INSTRUCTIONS_LIMIT - out.len();
    if section.len() <= room {
        out.push_str(&section);
        return false;
    }
    out.push_str(text::head(&section, room));
    out.push_str(CUT_NOTE);
    true
}

/// A rough token estimate: bytes divided by 4.
///
/// The budget test uses this so a change to the prompt or the tool specs
/// cannot silently grow the per-turn cost.
#[cfg(test)]
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes / 4
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;

    #[test]
    fn prompt_turn_hints_are_in_the_system_prompt() {
        assert!(TEMPLATE.contains("in one turn"), "{TEMPLATE}");
        assert!(TEMPLATE.contains("one bash command"), "{TEMPLATE}");
        assert!(TEMPLATE.contains("one edit call"), "{TEMPLATE}");
    }

    #[test]
    fn prompt_file_tool_hint_is_in_the_system_prompt() {
        assert!(TEMPLATE.contains("Use read, grep and find"), "{TEMPLATE}");
        assert!(
            TEMPLATE.contains("not cat, sed or grep in bash"),
            "{TEMPLATE}"
        );
    }

    struct Dir(tempfile::TempDir);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("reagent-prompt-{name}-"))
                .tempdir()
                .expect("makes a temp folder");
            Self(dir)
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[tokio::test]
    async fn root_and_subfolder_instructions_in_order() {
        let d = Dir::new("order");
        d.write("AGENTS.md", "root rules");
        d.write("sub/CLAUDE.md", "sub rules");
        let got = instructions(None, Some(d.path()), &d.path().join("sub")).await;
        assert_eq!(
            got,
            "\n\n# Instructions from AGENTS.md\n\nroot rules\
             \n\n# Instructions from sub/CLAUDE.md\n\nsub rules"
        );
    }

    #[tokio::test]
    async fn agents_md_wins_over_claude_md_in_one_folder() {
        let d = Dir::new("wins");
        d.write("AGENTS.md", "agents");
        d.write("CLAUDE.md", "claude");
        let got = instructions(None, Some(d.path()), d.path()).await;
        assert!(got.contains("agents"));
        assert!(!got.contains("claude"));
    }

    #[tokio::test]
    async fn the_global_file_comes_first_with_its_label() {
        let d = Dir::new("global");
        d.write("g/AGENTS.md", "global rules");
        d.write("repo/AGENTS.md", "repo rules");
        let got = instructions(
            Some(&d.path().join("g/AGENTS.md")),
            Some(&d.path().join("repo")),
            &d.path().join("repo"),
        )
        .await;
        assert_eq!(
            got,
            "\n\n# Instructions from ~/.config/reagent/AGENTS.md\n\nglobal rules\
             \n\n# Instructions from AGENTS.md\n\nrepo rules"
        );
    }

    #[test]
    fn an_empty_instructions_leaves_the_template_alone() {
        assert_eq!(system_with("/work", ""), system("/work"));
    }

    #[tokio::test]
    async fn no_files_add_nothing() {
        let d = Dir::new("none");
        let got = instructions(None, Some(d.path()), d.path()).await;
        assert_eq!(got, "");
        assert_eq!(system_with("/work", &got), system("/work"));
    }

    #[tokio::test]
    async fn long_instructions_are_cut_at_32_kb() {
        let d = Dir::new("long");
        d.write("AGENTS.md", &"x".repeat(40 * 1024));
        d.write("sub/AGENTS.md", "never read");
        let got = instructions(None, Some(d.path()), &d.path().join("sub")).await;
        assert!(got.len() <= INSTRUCTIONS_LIMIT + CUT_NOTE.len());
        assert!(got.ends_with("\n[instructions cut at 32 KB]"));
        assert!(!got.contains("never read"));
    }

    #[tokio::test]
    async fn folders_off_the_path_are_ignored() {
        let d = Dir::new("off");
        d.write("other/AGENTS.md", "other rules");
        d.write("sub/AGENTS.md", "sub rules");
        let got = instructions(None, Some(d.path()), &d.path().join("sub")).await;
        assert!(got.contains("sub rules"));
        assert!(!got.contains("other rules"));
    }

    #[tokio::test]
    async fn without_a_repo_only_cwd_is_read() {
        let d = Dir::new("norepo");
        d.write("AGENTS.md", "parent rules");
        d.write("sub/AGENTS.md", "sub rules");
        let got = instructions(None, None, &d.path().join("sub")).await;
        assert_eq!(got, "\n\n# Instructions from AGENTS.md\n\nsub rules");
    }
}

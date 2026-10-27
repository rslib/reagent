//! `find`: list files whose path matches a glob or contains a text.

use std::borrow::Cow;
use std::path::PathBuf;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{EffectClass, Effects, Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::gitignore::wildmatch;
use crate::resolve;
use crate::walk::{self, Cancelled, MAX_FILES, Skip};

/// At most this many paths are printed.
const MAX_PRINTED: usize = 200;

/// Arguments for one `find` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    #[serde(borrow)]
    pattern: Cow<'a, str>,
    #[serde(borrow)]
    path: Option<Cow<'a, str>>,
}

/// Lists files by path, matched against a glob or a text.
#[derive(Debug)]
pub struct Find {
    spec: ToolSpec,
    working_dir: PathBuf,
    skip: Skip,
}

impl Find {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
            skip: Skip::default(),
        }
    }

    /// Also skips folders with these names, on top of the built-in list.
    #[must_use]
    pub fn with_skip(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.skip = Skip::new(names);
        self
    }

    async fn run_inner(
        &self,
        args: &Args<'_>,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let root = match args.path.as_deref() {
            Some(path) => resolve(&self.working_dir, path)?,
            None => self.working_dir.clone(),
        };
        let listing = match walk::walk_folder(&root, &self.skip, cancel).await {
            Ok(Some(listing)) => listing,
            Ok(None) => {
                let shown = args.path.as_deref().unwrap_or(".");
                return Err(format!("not a folder: {shown}"));
            }
            Err(Cancelled) => return Err("cancelled".into()),
        };

        let pattern = args.pattern.as_ref();
        let is_glob = pattern.contains(['*', '?', '[']);
        let matches = |rel: &str| {
            if is_glob {
                wildmatch(pattern.as_bytes(), rel.as_bytes())
            } else {
                rel.contains(pattern)
            }
        };
        let prefix = args.path.as_deref().map(|path| path.trim_end_matches('/'));

        let mut out = String::new();
        let mut total = 0_usize;
        for file in listing.files.iter().filter(|file| matches(&file.rel)) {
            total += 1;
            if total > MAX_PRINTED {
                continue;
            }
            if let Some(prefix) = prefix {
                out.push_str(prefix);
                out.push('/');
            }
            out.push_str(&file.rel);
            out.push('\n');
        }
        if total == 0 {
            out.push_str("no matches\n");
        }
        if total > MAX_PRINTED {
            out.push_str(&format!(
                "[{total} files matched; showing the first {MAX_PRINTED}. Narrow the search with path or a longer pattern.]\n"
            ));
        }
        if listing.truncated {
            out.push_str(&format!("[the walk stopped after {MAX_FILES} files]\n"));
        }
        Ok(out)
    }
}

impl Tool for Find {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn main_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn effects(&self, args: &str) -> Effects {
        let Ok(arguments) = parse_args::<Args<'_>>(args) else {
            return Effects::default();
        };
        let path = match arguments.path.as_deref() {
            Some(path) => match resolve(&self.working_dir, path) {
                Ok(path) => path,
                Err(_) => return Effects::default(),
            },
            None => self.working_dir.clone(),
        };
        Effects {
            reads: vec![path],
            ..Effects::default()
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Read
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            match self.run_inner(&arguments, cancel).await {
                Ok(text) => ToolOutput::ok(text),
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

/// The name of this tool.
pub const NAME: &str = "find";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Find files by path under a folder (default: the working directory): ",
            "a glob such as `**/*.rs`, or text the path contains. ",
            "Skips ignored files. Prints paths, sorted, at most 200."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob, or text the path must contain." },
                "path": { "type": "string", "description": "Folder to search." }
            },
            "required": ["pattern"]
        }),
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    async fn find(tmp: &tempfile::TempDir, args: &str) -> ToolOutput {
        Find::new(tmp.path())
            .run(args, &CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn a_glob_matches_the_whole_path() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/a.rs");
        write(tmp.path(), "src/b.txt");
        write(tmp.path(), "c.rs");
        let output = find(&tmp, r#"{"pattern":"**/*.rs"}"#).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), "c.rs\nsrc/a.rs\n");
    }

    #[tokio::test]
    async fn find_skips_configured_folders() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "data/x.rs");
        write(tmp.path(), "src/a.rs");
        let output = Find::new(tmp.path())
            .with_skip(["data".to_string()])
            .run(r#"{"pattern":"**/*.rs"}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), "src/a.rs\n");
    }

    #[tokio::test]
    async fn a_star_glob_stays_in_one_folder() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/a.rs");
        write(tmp.path(), "c.rs");
        let output = find(&tmp, r#"{"pattern":"*.rs"}"#).await;
        assert_eq!(output.content.as_ref(), "c.rs\n");
    }

    #[tokio::test]
    async fn text_matches_part_of_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/config.rs");
        write(tmp.path(), "src/main.rs");
        let output = find(&tmp, r#"{"pattern":"config"}"#).await;
        assert_eq!(output.content.as_ref(), "src/config.rs\n");
    }

    #[tokio::test]
    async fn ignored_files_are_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs");
        write(tmp.path(), "b.log");
        write(tmp.path(), "target/c.rs");
        std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        let output = find(&tmp, r#"{"pattern":"**/*"}"#).await;
        assert_eq!(output.content.as_ref(), ".gitignore\na.rs\n");
    }

    #[tokio::test]
    async fn output_over_the_cap_ends_with_the_total() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..250 {
            write(tmp.path(), &format!("f{i:03}.txt"));
        }
        let output = find(&tmp, r#"{"pattern":".txt"}"#).await;
        let lines: Vec<&str> = output.content.lines().collect();
        assert_eq!(lines.len(), 201, "{}", output.content);
        assert_eq!(lines[0], "f000.txt");
        assert_eq!(lines[199], "f199.txt");
        assert_eq!(
            lines[200],
            "[250 files matched; showing the first 200. Narrow the search with path or a longer pattern.]"
        );
    }

    #[tokio::test]
    async fn a_path_limits_the_search_and_prints_from_the_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/a.rs");
        write(tmp.path(), "src/deep/b.rs");
        write(tmp.path(), "c.rs");
        let output = find(&tmp, r#"{"pattern":"**/*.rs","path":"src"}"#).await;
        assert_eq!(output.content.as_ref(), "src/a.rs\nsrc/deep/b.rs\n");
    }

    #[tokio::test]
    async fn no_matches_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs");
        let output = find(&tmp, r#"{"pattern":"zzz"}"#).await;
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), "no matches\n");
    }

    #[tokio::test]
    async fn a_file_path_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs");
        let output = find(&tmp, r#"{"pattern":"a","path":"a.rs"}"#).await;
        assert!(output.is_error);
        assert_eq!(output.content.as_ref(), "not a folder: a.rs");
    }

    /// A folder of three files, no subfolder, costs two jobs: the root's own
    /// checks with the ignore chain above it, then the folder. It cost 19
    /// before, here: the root stat, a `.gitignore` read and a `.git` look in
    /// each of the seven folders above the temp folder, and the folder.
    #[tokio::test]
    async fn one_hop_find_flat() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs");
        write(tmp.path(), "b.rs");
        write(tmp.path(), "c.rs");
        crate::blocking::reset_hops();
        let output = find(&tmp, r#"{"pattern":"**/*.rs"}"#).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), "a.rs\nb.rs\nc.rs\n");
        assert_eq!(crate::blocking::hops(), 2);
    }

    #[test]
    fn find_effects_read_the_folder() {
        let find = Find::new("/work");
        assert_eq!(find.main_arg(), Some("path"));
        assert_eq!(
            find.effects(r#"{"pattern":"x"}"#).reads,
            vec![PathBuf::from("/work")]
        );
        let effects = find.effects(r#"{"pattern":"x","path":"src"}"#);
        assert_eq!(effects.reads, vec![PathBuf::from("/work/src")]);
        assert!(effects.writes.is_empty());
    }

    #[test]
    fn find_bad_args_have_no_effects() {
        assert_eq!(Find::new("/work").effects("nope"), Effects::default());
    }
}

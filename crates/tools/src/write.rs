//! `write`: write content to a file atomically.

use std::borrow::Cow;
use std::path::PathBuf;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Effects, Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::resolve;

/// Arguments for one `write` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    /// The file to write, relative to the working directory.
    #[serde(borrow)]
    path: Cow<'a, str>,
    /// The content to write.
    #[serde(borrow)]
    content: Cow<'a, str>,
}

/// Writes a file, creating parent folders and replacing an existing file.
#[derive(Debug)]
pub struct Write {
    spec: ToolSpec,
    working_dir: PathBuf,
}

impl Write {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
        }
    }

    async fn run_inner(&self, args: &Args<'_>) -> Result<ToolOutput, String> {
        let path = resolve(&self.working_dir, &args.path)?;
        let bytes = args.content.as_bytes();

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                format!("cannot create parent folder {}: {error}", parent.display())
            })?;
        }

        crate::atomic::write(&path, bytes).await?;
        Ok(ToolOutput::ok(format!(
            "wrote {} byte(s) to {}",
            bytes.len(),
            path.display()
        )))
    }
}

impl Tool for Write {
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
        let Ok(path) = resolve(&self.working_dir, &arguments.path) else {
            return Effects::default();
        };
        Effects {
            writes: vec![path],
            ..Effects::default()
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            match self.run_inner(&arguments).await {
                Ok(output) => output,
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

/// The name of this tool.
pub const NAME: &str = "write";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: "Write a file, replacing it if it exists. Missing parent folders are created."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file, relative to the working directory." },
                "content": { "type": "string", "description": "The full content to write." }
            },
            "required": ["path", "content"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(tmp: &tempfile::TempDir) -> Write {
        Write::new(tmp.path())
    }

    #[tokio::test]
    async fn writes_content_and_reports_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"a.txt","content":"hello\nworld\n"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("12 byte"), "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("a.txt"))
            .await
            .unwrap();
        assert_eq!(got, "hello\nworld\n");
    }

    #[tokio::test]
    async fn creates_missing_parent_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"deep/nested/dir/f.txt","content":"x"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("deep/nested/dir/f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "x");
    }

    #[tokio::test]
    async fn replaces_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "old")
            .await
            .unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","content":"new"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "new");
    }

    #[tokio::test]
    async fn no_temp_files_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","content":"hi"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        assert_eq!(names, vec!["f.txt".to_string()], "{names:?}");
    }

    #[test]
    fn write_effects_are_a_write() {
        let write = Write::new("/work");
        assert_eq!(write.main_arg(), Some("path"));
        let effects = write.effects(r#"{"path":"a.txt","content":"x"}"#);
        assert_eq!(effects.writes, vec![PathBuf::from("/work/a.txt")]);
        assert!(effects.reads.is_empty());
    }

    #[test]
    fn write_bad_args_have_no_effects() {
        assert_eq!(Write::new("/work").effects("{}"), Effects::default());
    }
}

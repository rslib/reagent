//! The pre-run environment snapshot.
//!
//! Before the very first turn, run `prompts/snapshot.sh` through the same Bash
//! tool the agent uses, so it inherits its timeout, its output capping and its
//! process-group kill. The captured text is appended to the first user message
//! so the model can see what it has to work with.
//!
//! The snapshot must never fail a run: on a tool error or a timeout it returns
//! an empty string and the caller skips it.

use std::path::Path;

use reagent_agent::tool::Tool;
use reagent_ai::Text;
use reagent_tools::bash::Bash;
use tokio_util::sync::CancellationToken;

/// The snapshot script, baked in at build time from `prompts/snapshot.sh`.
const SNAPSHOT_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/snapshot.sh"
));

/// How long the snapshot may run before it is killed.
const TIMEOUT_MS: u64 = 15_000;

/// Captures the environment for `cwd`, or returns an empty string on failure.
///
/// Runs through [`Bash`], the same tool the agent calls, so the timeout, the
/// output cap and the process-group kill are all reused.
pub async fn capture_environment(cwd: &Path, cancel: &CancellationToken) -> String {
    let bash = Bash::new(cwd);
    let args = serde_json::json!({
        "command": SNAPSHOT_SCRIPT,
        "timeout_ms": TIMEOUT_MS,
    })
    .to_string();
    let output = bash.run(&args, cancel).await;
    if output.is_error {
        return String::new();
    }
    let mut text = output.content.to_string();
    // Bash always puts its outcome on the last line. Keep the snapshot only
    // when the script exited; a timeout or cancel skips it.
    let Some(index) = text.rfind("exit code:") else {
        return String::new();
    };
    if text[index..].contains('\n') {
        return String::new();
    }
    text.truncate(index);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The header placed above a snapshot before it is appended to the first user
/// message.
pub fn header() -> Text {
    Text::from("== environment ==")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder in the system temp dir, unique per process.
    async fn tmp(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reagent-snapshot-{}-{name}", std::process::id()));
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn captures_the_environment() {
        let dir = tmp("capture").await;
        let text = capture_environment(&dir, &CancellationToken::new()).await;
        assert!(!text.trim().is_empty(), "the snapshot should run");
        assert!(text.contains("== "), "the script prints a header: {text}");
        assert!(text.contains("pwd"), "{text}");
        assert!(text.contains("--- OS ---"), "{text}");
        assert!(text.contains("python3"), "{text}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_missing_folder_is_skipped_safely() {
        // An absent working directory must not fail the run: the snapshot
        // returns empty and the caller skips it.
        let dir =
            std::env::temp_dir().join(format!("reagent-snapshot-missing-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let text = capture_environment(&dir, &CancellationToken::new()).await;
        assert!(
            text.is_empty(),
            "a bad working directory is skipped: {text}"
        );
    }
}

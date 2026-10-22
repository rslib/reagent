//! End-to-end test: output over the cut spills into a folder of this run, and
//! the run deletes that folder when it exits.
//!
//! These tests drive the binary with a scripted fake provider, so they exist
//! only when the `testing` feature is on.
#![cfg(feature = "testing")]

use std::path::Path;

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// Runs `reagent` with `$TMPDIR` set to `tmp`, which makes the spill root its
/// own.
async fn run(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        // The user's real config must not change the test.
        .env("XDG_CONFIG_HOME", "/nonexistent/reagent-test-config")
        .env_remove("REAGENT_PROFILE")
        .env("TMPDIR", tmp)
        .args(args)
        .current_dir(tmp)
        .output()
        .await
        .expect("the binary runs")
}

/// The names of the run folders under the spill root of a `$TMPDIR` of `tmp`.
async fn run_folders(tmp: &Path) -> Vec<String> {
    let mut names = match tokio::fs::read_dir(tmp.join("reagent-spill")).await {
        Ok(names) => names,
        Err(_) => return Vec::new(),
    };
    let mut found = Vec::new();
    while let Some(entry) = names.next_entry().await.expect("reading the spill root") {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("run-") {
            found.push(name);
        }
    }
    found
}

/// A spill goes into a folder of the run, and the run deletes that folder when
/// it exits.
#[tokio::test]
async fn cli_spill_deletes_the_run_folder_at_exit() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let spill = tmp.path().join("reagent-spill");
    // A plain run prints only its final text, so the tool result that names the
    // spill file is not in the output: the shell copies the folder and the size
    // of the file aside while the run still holds them.
    let seen = tmp.path().join("seen.txt");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        format!(
            r#"{{"replies": [
              {{"text": "spilling", "calls": [
                {{"name": "bash", "arguments": {{"command": "head -c 40000 /dev/zero | tr '\\0' x"}}}}
              ]}},
              {{"text": "listing", "calls": [
                {{"name": "bash", "arguments": {{"command": "ls -1 '{spill}' > '{seen}'; cat '{spill}'/run-*/bash-*.log | wc -c >> '{seen}'"}}}}
              ]}},
              {{"text": "done"}}
            ]}}"#,
            spill = spill.display(),
            seen = seen.display(),
        ),
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let output = run(tmp.path(), &["--provider", &script_arg, "-p", "spill"]).await;

    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = tokio::fs::read_to_string(&seen)
        .await
        .expect("the listing call ran");
    let mut lines = listing.lines();
    let folder = lines.next().unwrap_or_default();
    assert!(
        folder.starts_with("run-"),
        "the spill is not in a run folder: {listing}"
    );
    assert_eq!(
        lines.next().unwrap_or_default().trim(),
        "40000",
        "the spill file does not hold the whole output: {listing}"
    );
    assert!(
        run_folders(tmp.path()).await.is_empty(),
        "the run folder outlived the run: {:?}",
        run_folders(tmp.path()).await
    );
}

/// A run that spills nothing makes no folder and no lock.
#[tokio::test]
async fn cli_spill_a_run_without_spill_makes_no_folder() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [
          {"text": "small", "calls": [
            {"name": "bash", "arguments": {"command": "echo hi"}}
          ]},
          {"text": "done"}
        ]}"#,
    )
    .await
    .expect("write script");

    let script_arg = format!("fake:{}", script.display());
    let output = run(tmp.path(), &["--provider", &script_arg, "-p", "no spill"]).await;

    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        run_folders(tmp.path()).await.is_empty(),
        "a run that spilled nothing made a folder: {:?}",
        run_folders(tmp.path()).await
    );
}

/// Writes a dead run's folder under the spill root: a lock file 2 minutes old
/// that no process holds.
async fn dead_folder(tmp: &Path) -> std::path::PathBuf {
    let folder = tmp.join("reagent-spill").join("run-1-0");
    tokio::fs::create_dir_all(&folder)
        .await
        .expect("dead folder");
    let lock = tokio::fs::File::create(folder.join("lock"))
        .await
        .expect("lock file")
        .into_std()
        .await;
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    lock.set_modified(old).expect("backdate the lock");
    folder
}

#[tokio::test]
async fn cli_spill_a_run_without_spill_leaves_the_sweep_to_the_next_spill() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dead = dead_folder(tmp.path()).await;
    let script = tmp.path().join("script.json");
    tokio::fs::write(
        &script,
        r#"{"replies": [
          {"text": "small", "calls": [
            {"name": "bash", "arguments": {"command": "echo hi"}}
          ]},
          {"text": "done"}
        ]}"#,
    )
    .await
    .expect("write script");
    let script_arg = format!("fake:{}", script.display());

    let output = run(tmp.path(), &["--provider", &script_arg, "-p", "no spill"]).await;
    assert!(output.status.success(), "exit {}", output.status);
    // A run that spilled nothing does no work at exit.
    assert!(
        tokio::fs::try_exists(&dead).await.expect("stat"),
        "a run without spill swept"
    );

    tokio::fs::write(
        &script,
        r#"{"replies": [
          {"text": "big", "calls": [
            {"name": "bash", "arguments": {"command": "head -c 40000 /dev/zero | tr '\\0' x"}}
          ]},
          {"text": "done"}
        ]}"#,
    )
    .await
    .expect("write script");
    let output = run(tmp.path(), &["--provider", &script_arg, "-p", "spill"]).await;
    assert!(output.status.success(), "exit {}", output.status);
    assert!(
        run_folders(tmp.path()).await.is_empty(),
        "the dead folder or the run's own is left: {:?}",
        run_folders(tmp.path()).await
    );
}

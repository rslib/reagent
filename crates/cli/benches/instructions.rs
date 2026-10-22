//! Gungraun bench: exact Callgrind instruction counts for the reagent binary
//! printing a long answer in `--json` mode.
//!
//! The binary runs with the scripted fake provider the `testing` feature adds
//! (`--provider fake:<file>`), so no network and no real model are involved and
//! the count is the process's own cost: flags, config, the loop, the JSON line
//! rendering and the write of an answer in [`DELTAS`] deltas. Output goes to
//! `/dev/null` (the null device), so the bench times the binary and not the
//! terminal. The script file and its folder are made once, while the command is
//! built, which is outside the measured part.
//!
//! Run with `cargo bench -p reagent --features testing --bench instructions`,
//! or with `-- --test` to run the bench once without Callgrind.
//! `bench/instructions.sh` runs every bench target against a base commit.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::path::PathBuf;

use gungraun::prelude::*;

/// Characters of answer text the scripted model sends.
const DELTAS: usize = 10_000;

/// The folder the scripted session runs in, so nothing is written in the tree.
fn work_dir() -> PathBuf {
    std::env::temp_dir().join(format!("reagent-instructions-{}", std::process::id()))
}

/// Writes a scripted reply of [`DELTAS`] one-character deltas and returns its
/// path, so the run writes one `text_delta` line per delta, as a streamed
/// answer does.
fn script() -> PathBuf {
    let dir = work_dir();
    std::fs::create_dir_all(&dir).expect("the bench folder is created");
    let path = dir.join("script.json");
    let document = serde_json::json!({
        "replies": [
            {
                "deltas": vec!["x"; DELTAS],
                "usage": {
                    "input": 10, "output": DELTAS, "cache_read": 0, "cache_write": 0, "reasoning": 0
                }
            }
        ]
    });
    std::fs::write(
        &path,
        serde_json::to_vec(&document).expect("the script serializes"),
    )
    .expect("the script is written");
    path
}

#[binary_benchmark]
#[bench::json()]
fn reagent_json() -> gungraun::Command {
    let script = script();
    let provider = format!("fake:{}", script.display());
    gungraun::Command::new(env!("CARGO_BIN_EXE_reagent"))
        // The user's real config must not change the run.
        .env("XDG_CONFIG_HOME", work_dir().join("config"))
        .current_dir(work_dir())
        .args(["--json", "--provider", &provider, "-p", "answer"])
        .stdout(gungraun::Stdio::Null)
        .stderr(gungraun::Stdio::Null)
        .build()
}

binary_benchmark_group!(name = binary, benchmarks = [reagent_json]);

main!(binary_benchmark_groups = binary);

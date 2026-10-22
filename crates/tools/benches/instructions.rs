//! Gungraun benches: exact Callgrind instruction counts for the hot paths of
//! the native tools: an `edit` that replaces many CRLF matches, a `read` of a
//! long file, a `find` over a tree that a root `.gitignore` filters, a `grep`
//! over 500 files, and a `bash` call whose output spills 8 MB.
//!
//! Every file and folder is built in a `setup` function, so only the tool call
//! is counted. Each bench call runs once: the `edit` bench rewrites its file,
//! which is why it can be run only once on a fixture.
//!
//! The `bash` bench starts a shell, so Callgrind counts this process (the read
//! loop, the tail, the spill writes) and not the child that writes the bytes.
//!
//! Run with `cargo bench -p reagent-tools --bench instructions`, or with
//! `cargo bench -p reagent-tools --bench instructions -- --test` to run each
//! bench once without Callgrind. `bench/instructions.sh` runs both bench
//! targets against a base commit.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::hint::black_box;

use gungraun::prelude::*;
use reagent_agent::tool::Tool;
use reagent_tools::bash::Bash;
use reagent_tools::edit::Edit;
use reagent_tools::find::Find;
use reagent_tools::grep::Grep;
use reagent_tools::read::Read;
use tempfile::TempDir;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

/// Lines in the file the `edit` bench rewrites, one CRLF match per line.
const CRLF_LINES: usize = 10_000;
/// Lines in the file the `read` bench reads.
const READ_LINES: usize = 10_000;
/// Files the `find` bench walks.
const FIND_FILES: usize = 5_000;
/// Folders under `sub/` in the `find` bench's repository.
const FIND_FOLDERS: usize = 50;
/// Files the `grep` bench searches.
const GREP_FILES: usize = 500;
/// Bytes the `bash` bench makes the shell write, so the output is spilled.
const CAPTURE_BYTES: u64 = 8 * 1024 * 1024;

/// A current-thread runtime, the one the agent uses.
fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
}

struct EditCase {
    runtime: Runtime,
    /// Held for the bench: dropping it removes the files.
    _tmp: TempDir,
    tool: Edit,
    args: &'static str,
    cancel: CancellationToken,
}

/// A 10000-line CRLF file and an `edit` whose texts are written with LF, as a
/// model writes them, so every one of the 10000 matches needs the conversion.
fn edit_crlf() -> EditCase {
    let runtime = runtime();
    let tmp = tempfile::tempdir().expect("a temp dir");
    runtime.block_on(async {
        let text = "alpha\r\n".repeat(CRLF_LINES);
        tokio::fs::write(tmp.path().join("crlf.txt"), text)
            .await
            .expect("the file writes");
    });
    EditCase {
        runtime,
        tool: Edit::new(tmp.path()),
        _tmp: tmp,
        args: r#"{"path":"crlf.txt","old_string":"alpha\n","new_string":"beta\n","replace_all":true}"#,
        cancel: CancellationToken::new(),
    }
}

#[library_benchmark]
#[bench::crlf_replace_all(setup = edit_crlf)]
fn edit(case: EditCase) -> usize {
    let output = case
        .runtime
        .block_on(case.tool.run(case.args, &case.cancel));
    black_box(output.content.len())
}

struct ReadCase {
    runtime: Runtime,
    _tmp: TempDir,
    tool: Read,
    args: &'static str,
    cancel: CancellationToken,
}

/// A 10000-line file read with a limit that covers it: the numbered lines reach
/// the tool's 32 KB byte cap first, so the bench streams a long file until the
/// cap and then builds the hint that names the next offset.
fn read_lines() -> ReadCase {
    let runtime = runtime();
    let tmp = tempfile::tempdir().expect("a temp dir");
    runtime.block_on(async {
        let text = "x\n".repeat(READ_LINES);
        tokio::fs::write(tmp.path().join("lines.txt"), text)
            .await
            .expect("the file writes");
    });
    ReadCase {
        runtime,
        tool: Read::new(tmp.path()),
        _tmp: tmp,
        args: r#"{"path":"lines.txt","limit":10000}"#,
        cancel: CancellationToken::new(),
    }
}

#[library_benchmark]
#[bench::lines(setup = read_lines)]
fn read(case: ReadCase) -> usize {
    let output = case
        .runtime
        .block_on(case.tool.run(case.args, &case.cancel));
    black_box(output.content.len())
}

struct FindCase {
    runtime: Runtime,
    _tmp: TempDir,
    tool: Find,
    args: &'static str,
    cancel: CancellationToken,
}

/// A repository with a root `.gitignore` and 5000 `.rs` files under `sub/`,
/// beside one ignored `.log` file per folder.
fn find_files() -> FindCase {
    let runtime = runtime();
    let tmp = tempfile::tempdir().expect("a temp dir");
    runtime.block_on(async {
        let root = tmp.path();
        tokio::fs::create_dir(root.join(".git"))
            .await
            .expect("the repository marker");
        tokio::fs::write(root.join(".gitignore"), "*.log\nbuild/\n")
            .await
            .expect("the ignore file writes");
        let per_folder = FIND_FILES / FIND_FOLDERS;
        for folder in 0..FIND_FOLDERS {
            let dir = root.join("sub").join(format!("m{folder:02}"));
            tokio::fs::create_dir_all(&dir).await.expect("the folder");
            for file in 0..per_folder {
                tokio::fs::write(dir.join(format!("f{file:03}.rs")), "fn main() {}\n")
                    .await
                    .expect("the file writes");
            }
            tokio::fs::write(dir.join("debug.log"), "")
                .await
                .expect("the ignored file writes");
        }
    });
    FindCase {
        runtime,
        tool: Find::new(tmp.path()),
        _tmp: tmp,
        args: r#"{"pattern":"**/*.rs","path":"sub"}"#,
        cancel: CancellationToken::new(),
    }
}

#[library_benchmark]
#[bench::glob(setup = find_files)]
fn find(case: FindCase) -> usize {
    let output = case
        .runtime
        .block_on(case.tool.run(case.args, &case.cancel));
    black_box(output.content.len())
}

library_benchmark_group!(name = tools, benchmarks = [edit, read, find, grep, capture]);

struct GrepCase {
    runtime: Runtime,
    _tmp: TempDir,
    tool: Grep,
    args: &'static str,
    cancel: CancellationToken,
}

/// 500 small files under `sub/`, each with one matching line: every file is
/// opened, probed, matched and reported.
fn grep_files() -> GrepCase {
    let runtime = runtime();
    let tmp = tempfile::tempdir().expect("a temp dir");
    runtime.block_on(async {
        let sub = tmp.path().join("sub");
        tokio::fs::create_dir(&sub).await.expect("the folder");
        for n in 0..GREP_FILES {
            tokio::fs::write(sub.join(format!("f{n:03}.txt")), "needle\n")
                .await
                .expect("the file writes");
        }
    });
    GrepCase {
        runtime,
        tool: Grep::new(tmp.path()),
        _tmp: tmp,
        args: r#"{"pattern":"needle","path":"sub"}"#,
        cancel: CancellationToken::new(),
    }
}

#[library_benchmark]
#[bench::files(setup = grep_files)]
fn grep(case: GrepCase) -> usize {
    let output = case
        .runtime
        .block_on(case.tool.run(case.args, &case.cancel));
    black_box(output.content.len())
}

struct CaptureCase {
    runtime: Runtime,
    _tmp: TempDir,
    tool: Bash,
    args: String,
    cancel: CancellationToken,
}

/// A `bash` call whose output is 8 MB of `x`: the whole stream passes the
/// head and the tail and goes to the spill file in whole buffers.
fn capture_spill() -> CaptureCase {
    let tmp = tempfile::tempdir().expect("a temp dir");
    CaptureCase {
        runtime: runtime(),
        tool: Bash::new(tmp.path()),
        _tmp: tmp,
        args: format!(
            r#"{{"command":"head -c {CAPTURE_BYTES} /dev/zero | tr '\\000' x","timeout_ms":60000}}"#
        ),
        cancel: CancellationToken::new(),
    }
}

#[library_benchmark]
#[bench::spill(setup = capture_spill)]
fn capture(case: CaptureCase) -> usize {
    let output = case
        .runtime
        .block_on(case.tool.run(&case.args, &case.cancel));
    black_box(output.content.len())
}

main!(library_benchmark_groups = tools);

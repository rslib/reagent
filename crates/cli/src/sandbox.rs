//! The hidden `--sandbox-exec` helper: runs one program inside a birdcage
//! sandbox and exits with its code.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[expect(
    clippy::disallowed_types,
    reason = "birdcage has its own Command; the helper runs before any runtime starts"
)]
use birdcage::process::Command;
use birdcage::process::Stdio;
use birdcage::{Birdcage, Exception, Sandbox};

/// The exit code that tells the bash tool the sandbox could not start.
const UNAVAILABLE: u8 = 125;

/// What the helper was asked to run and what it may touch.
#[derive(Debug, PartialEq)]
pub struct Policy {
    /// Whether the program may open network connections.
    pub network: bool,
    /// Folders the program may write, besides what it can read.
    pub write: Vec<PathBuf>,
    /// The program to run.
    pub program: OsString,
    /// The arguments of the program.
    pub args: Vec<OsString>,
}

/// Parses the helper arguments after `--sandbox-exec`:
/// `[--network] [--write DIR]... -- PROGRAM ARGS...`.
pub fn parse_policy(args: impl IntoIterator<Item = OsString>) -> Result<Policy, String> {
    let mut args = args.into_iter();
    let mut network = false;
    let mut write = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            let program = args
                .next()
                .ok_or_else(|| "no program after --".to_string())?;
            return Ok(Policy {
                network,
                write,
                program,
                args: args.collect(),
            });
        } else if arg == "--network" {
            network = true;
        } else if arg == "--write" {
            let dir = args
                .next()
                .ok_or_else(|| "missing value for --write".to_string())?;
            write.push(PathBuf::from(dir));
        } else {
            return Err(format!(
                "unknown sandbox argument: {}",
                arg.to_string_lossy()
            ));
        }
    }
    Err("missing -- before the program".into())
}

/// Finds `program` on `PATH` the way a shell would, for a bare name.
fn on_path(program: &Path) -> bool {
    if program.components().count() != 1 {
        return program.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Runs the helper: sandboxes itself as birdcage does, spawns the program,
/// waits, and returns the program's exit code (128 + signal for a signal).
pub fn helper(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match run(args) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("reagent: sandbox unavailable: {error}");
            ExitCode::from(UNAVAILABLE)
        }
    }
}

#[expect(
    clippy::disallowed_types,
    reason = "birdcage has its own Command; the helper runs before any runtime starts"
)]
fn run(args: impl IntoIterator<Item = OsString>) -> Result<u8, String> {
    let mut policy = parse_policy(args)?;
    if policy.program == "bash" && !on_path(Path::new("bash")) {
        policy.program = "sh".into();
    }
    let mut sandbox = Birdcage::new();
    let mut allow = |exception: Exception| {
        sandbox
            .add_exception(exception)
            .map(|_| ())
            .map_err(|error| error.to_string())
    };
    allow(Exception::ExecuteAndRead("/".into()))?;
    for dir in policy.write.iter().filter(|dir| dir.exists()) {
        allow(Exception::WriteAndRead(dir.clone()))?;
    }
    allow(Exception::FullEnvironment)?;
    if policy.network {
        allow(Exception::Networking)?;
    }
    let mut command = Command::new(&policy.program);
    command
        .args(&policy.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = sandbox.spawn(command).map_err(|error| error.to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .map_or(1, |code| u8::try_from(code & 0xff).unwrap_or(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Policy, String> {
        parse_policy(args.iter().map(OsString::from))
    }

    #[test]
    fn sandbox_policy_reads_flags_and_the_program() {
        let policy = parse(&[
            "--network",
            "--write",
            "/a",
            "--write",
            "/b",
            "--",
            "bash",
            "-c",
            "x",
        ])
        .expect("parses");
        assert_eq!(
            policy,
            Policy {
                network: true,
                write: vec!["/a".into(), "/b".into()],
                program: "bash".into(),
                args: vec!["-c".into(), "x".into()],
            }
        );
    }

    #[test]
    fn sandbox_policy_defaults_to_no_network_and_no_write() {
        let policy = parse(&["--", "sh"]).expect("parses");
        assert!(!policy.network && policy.write.is_empty() && policy.args.is_empty());
    }

    #[test]
    fn sandbox_policy_keeps_flags_after_the_separator_for_the_program() {
        let policy = parse(&["--", "ls", "--network"]).expect("parses");
        assert!(!policy.network);
        assert_eq!(policy.args, ["--network"]);
    }

    #[test]
    fn sandbox_policy_rejects_bad_arguments() {
        assert!(parse(&["--write"]).unwrap_err().contains("--write"));
        assert!(parse(&["--network"]).unwrap_err().contains("--"));
        assert!(parse(&["--"]).unwrap_err().contains("program"));
        assert!(
            parse(&["--bogus", "--", "sh"])
                .unwrap_err()
                .contains("--bogus")
        );
    }
}

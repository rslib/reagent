//! The hidden `--sandbox-exec` helper: sandboxes itself, replaces itself with
//! the program and so exits with the program's code and signal. macOS uses a
//! Seatbelt profile, Linux uses Landlock.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The exit code that tells the bash tool the sandbox could not start.
const UNAVAILABLE: u8 = 125;

/// What the helper was asked to run and what it may touch.
#[derive(Debug, PartialEq)]
pub struct Policy {
    /// Whether the program may open network connections.
    pub network: bool,
    /// Whether the program may not leave the process group the helper put it
    /// in: `setsid` and `setpgid` answer `EPERM` where that can be refused.
    pub keep_group: bool,
    /// Folders the program may write, besides what it can read, as the caller
    /// named them: they are resolved per run below.
    pub write: Vec<PathBuf>,
    /// Write folders the caller already resolved: canonical and existing, so
    /// the helper uses them as they are.
    pub resolved_write: Vec<PathBuf>,
    /// The `bash` the caller resolved, when it did: the helper runs it as it
    /// is instead of searching `PATH` for it on every start.
    pub shell: Option<PathBuf>,
    /// Whether the program starts with no environment at all.
    pub clear_env: bool,
    /// The variables the program gets, in order, on top of `clear_env`.
    pub env: Vec<(String, String)>,
    /// The program to run.
    pub program: OsString,
    /// The arguments of the program.
    pub args: Vec<OsString>,
}

/// Parses the helper arguments after `--sandbox-exec`:
/// `[--network] [--keep-group] [--shell PATH] [--write DIR]...
/// [--write-resolved DIR]... [--env-clear] [--env NAME=VALUE]...
/// -- PROGRAM ARGS...`.
pub fn parse_policy(args: impl IntoIterator<Item = OsString>) -> Result<Policy, String> {
    let mut args = args.into_iter();
    let mut network = false;
    let mut keep_group = false;
    let mut write = Vec::new();
    let mut resolved_write = Vec::new();
    let mut shell = None;
    let mut clear_env = false;
    let mut env = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            let program = args
                .next()
                .ok_or_else(|| "no program after --".to_string())?;
            return Ok(Policy {
                network,
                keep_group,
                write,
                resolved_write,
                shell,
                clear_env,
                env,
                program,
                args: args.collect(),
            });
        } else if arg == "--network" {
            network = true;
        } else if arg == "--keep-group" {
            keep_group = true;
        } else if arg == "--env-clear" {
            clear_env = true;
        } else if arg == "--env" {
            let pair = args
                .next()
                .ok_or_else(|| "missing value for --env".to_string())?;
            env.push(env_pair(&pair)?);
        } else if arg == "--shell" {
            let path = args
                .next()
                .ok_or_else(|| "missing value for --shell".to_string())?;
            shell = Some(PathBuf::from(path));
        } else if arg == "--write-resolved" {
            let dir = args
                .next()
                .ok_or_else(|| "missing value for --write-resolved".to_string())?;
            resolved_write.push(PathBuf::from(dir));
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

/// One `--env NAME=VALUE` pair, or the reason the helper refuses it: a loader
/// variable runs code in the helper, before any sandbox is on, so it never
/// reaches a program through here.
fn env_pair(pair: &OsString) -> Result<(String, String), String> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = pair.as_bytes();
    let shown = pair.to_string_lossy();
    let Some(at) = bytes.iter().position(|byte| *byte == b'=') else {
        return Err(format!("--env needs NAME=VALUE: {shown}"));
    };
    let (name, value) = bytes.split_at(at);
    let value = &value[1..];
    let name = std::str::from_utf8(name)
        .map_err(|_| format!("--env has a name that is not UTF-8: {shown}"))?;
    let value = std::str::from_utf8(value)
        .map_err(|_| format!("--env has a value that is not UTF-8: {shown}"))?;
    if name.is_empty() {
        return Err(format!("--env has no name: {shown}"));
    }
    if name.contains('=') || name.contains('\0') || value.contains('\0') {
        return Err(format!("--env has a name with `=` or NUL in it: {shown}"));
    }
    if name.starts_with("LD_") || name.starts_with("DYLD_") || name == "GCONV_PATH" {
        return Err(format!(
            "--env may not set {name}: its code would run before the sandbox is on"
        ));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Finds `program` on `PATH` the way a shell would, for a bare name.
fn on_path(program: &Path) -> bool {
    if program.components().count() != 1 {
        return program.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Runs the helper: sandboxes itself and replaces itself with the program, so
/// the program keeps the helper's pid, signals included. Only a failed exec
/// comes back, as the reason the sandbox could not start.
pub fn helper(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    // `exec` never returns on success, so the error is all that comes back.
    let error = match run(args) {
        Err(error) => error,
        Ok(never) => match never {},
    };
    eprintln!("reagent: sandbox unavailable: {error}");
    ExitCode::from(UNAVAILABLE)
}

#[expect(
    clippy::disallowed_types,
    reason = "the helper runs before any runtime starts"
)]
fn run(args: impl IntoIterator<Item = OsString>) -> Result<Infallible, String> {
    let mut policy = parse_policy(args)?;
    if policy.program == "bash" {
        // A resolved shell needs no search; without one, a missing bash still
        // falls back to sh.
        // A shell removed since the run started falls back to the search.
        match policy.shell.take().filter(|shell| shell.is_file()) {
            Some(shell) => policy.program = shell.into_os_string(),
            None if !on_path(Path::new("bash")) => policy.program = "sh".into(),
            None => {}
        }
    }
    let mut write = policy.resolved_write;
    write.extend(write_folders(&policy.write));
    reagent_sandbox::restrict(policy.network, &write)?;
    if policy.keep_group {
        reagent_sandbox::keep_group()?;
    }
    let mut command = std::process::Command::new(&policy.program);
    command.args(&policy.args);
    // A program the caller sends an environment for gets nothing else: what it
    // is given here is all it sees.
    if policy.clear_env {
        command.env_clear();
    }
    for (name, value) in &policy.env {
        command.env(name, value);
    }
    // `exec` keeps the pid and passes a signal on to the caller as a signal,
    // so nothing waits for a child and no 128 + signal mapping is needed.
    let error = command.exec();
    Err(format!(
        "cannot run {}: {error}",
        policy.program.to_string_lossy()
    ))
}

/// The write folders that exist, canonical, since both backends match the
/// resolved path (`/tmp` is `/private/tmp` on macOS).
#[expect(
    clippy::disallowed_methods,
    reason = "the helper runs before any runtime starts"
)]
fn write_folders(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .filter(|dir| dir.is_dir())
        .collect()
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
                keep_group: false,
                write: vec!["/a".into(), "/b".into()],
                resolved_write: Vec::new(),
                shell: None,
                clear_env: false,
                env: Vec::new(),
                program: "bash".into(),
                args: vec!["-c".into(), "x".into()],
            }
        );
    }

    #[test]
    fn sandbox_policy_env_clear_and_pairs() {
        let policy = parse(&[
            "--env-clear",
            "--env",
            "A=1",
            "--env",
            "B=two=three",
            "--",
            "sh",
        ])
        .expect("parses");
        assert!(policy.clear_env);
        assert_eq!(
            policy.env,
            [
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two=three".to_string()),
            ]
        );
        // Without the flags nothing changes for the bash tool.
        let plain = parse(&["--", "sh"]).expect("parses");
        assert!(!plain.clear_env && plain.env.is_empty());
        assert!(parse(&["--env"]).unwrap_err().contains("--env"));
    }

    #[test]
    fn sandbox_policy_env_pair_needs_a_name_and_a_value() {
        for pair in ["NAME", "=value", "\u{0}=x", "A=\u{0}"] {
            let refused = parse(&["--env", pair, "--", "sh"]).unwrap_err();
            assert!(refused.contains("--env"), "{pair}: {refused}");
        }
    }

    #[test]
    fn sandbox_policy_env_refuses_loader_names() {
        for pair in [
            "LD_PRELOAD=/tmp/x.so",
            "LD_LIBRARY_PATH=/tmp",
            "DYLD_INSERT_LIBRARIES=/tmp/x.dylib",
            "DYLD_FRAMEWORK_PATH=/tmp",
            "GCONV_PATH=/tmp/x",
        ] {
            let refused = parse(&["--env", pair, "--", "sh"]).unwrap_err();
            assert!(
                refused.contains(pair.split('=').next().unwrap()),
                "{refused}"
            );
        }
        assert!(parse(&["--env", "PATH=/bin", "--", "sh"]).is_ok());
    }

    #[test]
    fn sandbox_policy_keep_group() {
        let policy = parse(&["--keep-group", "--", "sh"]).expect("parses");
        assert!(policy.keep_group);
        assert!(!parse(&["--", "sh"]).expect("parses").keep_group);
    }

    #[test]
    fn perf_helper_parses_a_resolved_shell_and_write_folders() {
        let policy = parse(&[
            "--shell",
            "/bin/bash",
            "--write-resolved",
            "/resolved",
            "--write",
            "/later",
            "--",
            "bash",
            "-c",
            "x",
        ])
        .expect("parses");
        assert_eq!(policy.shell, Some(PathBuf::from("/bin/bash")));
        assert_eq!(policy.resolved_write, [PathBuf::from("/resolved")]);
        assert_eq!(policy.write, [PathBuf::from("/later")]);
        assert!(parse(&["--shell"]).unwrap_err().contains("--shell"));
        assert!(
            parse(&["--write-resolved", "/r", "--"])
                .unwrap_err()
                .contains("program")
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

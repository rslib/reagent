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

/// The run's network proxy: the address a program's proxy variables name and
/// the port the sandbox profile allows, for a run whose `[sandbox] network`
/// names hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetProxy {
    /// The `127.0.0.1:PORT` text the program's variables get.
    pub addr: String,
    /// The port of `addr`.
    pub port: u16,
}

impl NetProxy {
    /// Parses `--net-proxy`: a local address, since the proxy listens on this
    /// machine only.
    fn parse(text: &std::ffi::OsStr) -> Result<NetProxy, String> {
        let addr = text
            .to_str()
            .ok_or_else(|| format!("--net-proxy is not UTF-8: {}", text.to_string_lossy()))?;
        let port = addr
            .strip_prefix("127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("--net-proxy must be 127.0.0.1:PORT: {addr}"))?;
        Ok(NetProxy {
            addr: addr.to_string(),
            port,
        })
    }

    /// The value the `HTTP_PROXY` family gets.
    fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The value the `ALL_PROXY` family gets. `socks5h` leaves the name to the
    /// proxy, which resolves it at connect time.
    fn socks(&self) -> String {
        format!("socks5h://{}", self.addr)
    }
}

/// What the helper was asked to run and what it may touch.
#[derive(Debug, PartialEq)]
pub struct Policy {
    /// Whether the program may open network connections.
    pub network: bool,
    /// The run's network proxy, when `[sandbox] network` names hosts: the
    /// program reaches those hosts through it and nothing else.
    pub net_proxy: Option<NetProxy>,
    /// The unix socket of that proxy, which the bridge of a Linux network
    /// namespace connects to. macOS needs no bridge: its profile allows the
    /// proxy's own port.
    pub net_proxy_unix: Option<PathBuf>,
    /// The local ports the program may reach without the proxy.
    pub net_local: Vec<u16>,
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

impl Policy {
    /// What the program may reach on the network.
    fn net(&self) -> reagent_sandbox::Net<'_> {
        match &self.net_proxy {
            Some(proxy) => reagent_sandbox::Net::Proxy {
                proxy: proxy.port,
                local: &self.net_local,
            },
            None if self.network => reagent_sandbox::Net::All,
            None => reagent_sandbox::Net::Off,
        }
    }
}

/// The fence the helper applies for `policy`. On Linux a host list is a fence
/// only once this process sits in a network namespace of its own with the
/// bridge up, so the namespace is made first and its failure is the
/// `network = false` fence, never a whole network.
#[cfg(target_os = "linux")]
fn fence(policy: &Policy) -> reagent_sandbox::Net<'_> {
    fence_with(policy, namespace_for)
}

/// [`fence`] with the namespace step as `namespace`, so a failed step can be
/// tried without putting this process into a namespace.
#[cfg(target_os = "linux")]
fn fence_with<'a>(
    policy: &'a Policy,
    namespace: impl FnOnce(&Policy) -> Result<(), String>,
) -> reagent_sandbox::Net<'a> {
    // A bool keeps today's fence, byte for byte: no namespace is made.
    if policy.net_proxy.is_none() {
        return policy.net();
    }
    match namespace(policy) {
        Ok(()) => policy.net(),
        Err(_) => reagent_sandbox::Net::Off,
    }
}

/// The network namespace and its bridge to the run's proxy, before Landlock.
#[cfg(target_os = "linux")]
fn namespace_for(policy: &Policy) -> Result<(), String> {
    let Some(proxy) = &policy.net_proxy else {
        return Ok(());
    };
    let socket = policy
        .net_proxy_unix
        .as_deref()
        .ok_or("a host list needs --net-proxy-unix here")?;
    reagent_sandbox::proxy_namespace(proxy.port, &policy.net_local, socket)
}

/// macOS fences a host list with its profile, which names the proxy's own port
/// and each local port, so the helper needs no namespace.
#[cfg(not(target_os = "linux"))]
fn fence(policy: &Policy) -> reagent_sandbox::Net<'_> {
    policy.net()
}

/// Parses the helper arguments after `--sandbox-exec`:
/// `[--network] [--net-proxy ADDR] [--net-proxy-unix PATH] [--net-local PORT]...
/// [--keep-group] [--shell PATH] [--write DIR]...
/// [--write-resolved DIR]... [--env-clear] [--env NAME=VALUE]...
/// -- PROGRAM ARGS...`.
pub fn parse_policy(args: impl IntoIterator<Item = OsString>) -> Result<Policy, String> {
    let mut args = args.into_iter();
    let mut network = false;
    let mut net_proxy = None;
    let mut net_proxy_unix = None;
    let mut net_local = Vec::new();
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
                net_proxy,
                net_proxy_unix,
                net_local,
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
        } else if arg == "--net-proxy" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --net-proxy".to_string())?;
            net_proxy = Some(NetProxy::parse(&value)?);
        } else if arg == "--net-proxy-unix" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --net-proxy-unix".to_string())?;
            let path = PathBuf::from(&value);
            if !path.is_absolute() {
                return Err(format!(
                    "--net-proxy-unix needs a full path: {}",
                    value.to_string_lossy()
                ));
            }
            net_proxy_unix = Some(path);
        } else if arg == "--net-local" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --net-local".to_string())?;
            let shown = value.to_string_lossy();
            let port = value
                .to_str()
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("--net-local needs a port 1-65535: {shown}"))?;
            net_local.push(port);
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
    let mut write = std::mem::take(&mut policy.resolved_write);
    write.extend(write_folders(&policy.write));
    // The fence comes first: on Linux the namespace it makes has to be there
    // before Landlock, and a step that fails is the no-network one.
    let net = fence(&policy);
    reagent_sandbox::restrict(net, &write)?;
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
    // The proxy is the one way out of a sandbox with a host list, so the
    // program is told to use it, and never to skip it for a host.
    if let Some(proxy) = &policy.net_proxy {
        let http = proxy.http();
        let socks = proxy.socks();
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            command.env(name, &http);
        }
        for name in ["ALL_PROXY", "all_proxy"] {
            command.env(name, &socks);
        }
        command.env("REAGENT_PROXY", &proxy.addr);
        command.env_remove("NO_PROXY");
        command.env_remove("no_proxy");
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
                net_proxy: None,
                net_proxy_unix: None,
                net_local: Vec::new(),
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
    fn host_list_policy_reads_the_proxy_and_the_local_ports() {
        let policy = parse(&[
            "--net-proxy",
            "127.0.0.1:41234",
            "--net-local",
            "8080",
            "--net-local",
            "9000",
            "--",
            "sh",
        ])
        .expect("parses");
        let proxy = policy.net_proxy.clone().expect("a proxy");
        assert_eq!(proxy.addr, "127.0.0.1:41234");
        assert_eq!(proxy.port, 41234);
        assert_eq!(policy.net_local, [8080, 9000]);
        assert!(!policy.network);
        assert_eq!(
            policy.net(),
            reagent_sandbox::Net::Proxy {
                proxy: 41234,
                local: &[8080, 9000],
            }
        );
        // The variables point at the proxy, and the SOCKS5 form leaves the
        // name to the proxy.
        assert_eq!(proxy.http(), "http://127.0.0.1:41234");
        assert_eq!(proxy.socks(), "socks5h://127.0.0.1:41234");
        // Without a list the net is the plain bool, byte for byte.
        assert_eq!(
            parse(&["--", "sh"]).unwrap().net(),
            reagent_sandbox::Net::Off
        );
        assert_eq!(
            parse(&["--network", "--", "sh"]).unwrap().net(),
            reagent_sandbox::Net::All
        );
    }

    #[test]
    fn host_list_policy_refuses_a_bad_proxy_or_port() {
        for value in [
            "192.168.1.1:41234",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:port",
            "localhost:41234",
        ] {
            let refused = parse(&["--net-proxy", value, "--", "sh"]).unwrap_err();
            assert!(refused.contains("--net-proxy"), "{value}: {refused}");
            assert!(refused.contains(value), "{value}: {refused}");
        }
        for value in ["0", "port", "70000", ""] {
            let refused = parse(&["--net-local", value, "--", "sh"]).unwrap_err();
            assert!(refused.contains("--net-local"), "{value}: {refused}");
        }
        assert!(parse(&["--net-proxy"]).unwrap_err().contains("--net-proxy"));
        assert!(parse(&["--net-local"]).unwrap_err().contains("--net-local"));
    }

    #[test]
    fn host_list_policy_reads_the_unix_socket_of_the_proxy() {
        let policy = parse(&[
            "--net-proxy",
            "127.0.0.1:41234",
            "--net-proxy-unix",
            "/tmp/reagent-proxy-41234/proxy.sock",
            "--",
            "sh",
        ])
        .expect("parses");
        assert_eq!(
            policy.net_proxy_unix,
            Some(PathBuf::from("/tmp/reagent-proxy-41234/proxy.sock"))
        );
        assert!(
            parse(&["--", "sh"])
                .expect("parses")
                .net_proxy_unix
                .is_none()
        );
        for value in ["proxy.sock", "./proxy.sock"] {
            let refused = parse(&["--net-proxy-unix", value, "--", "sh"]).unwrap_err();
            assert!(refused.contains("--net-proxy-unix"), "{value}: {refused}");
        }
        assert!(
            parse(&["--net-proxy-unix"])
                .unwrap_err()
                .contains("--net-proxy-unix")
        );
    }

    /// A namespace step that fails is the `network = false` fence and never the
    /// whole network, and a list with no socket to bridge to is that fence too.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_list_fence_falls_back_to_no_network() {
        let policy = parse(&[
            "--net-proxy",
            "127.0.0.1:41234",
            "--net-proxy-unix",
            "/tmp/reagent-proxy-41234/proxy.sock",
            "--net-local",
            "8080",
            "--",
            "sh",
        ])
        .expect("parses");
        assert_eq!(
            fence_with(&policy, |_| Ok(())),
            reagent_sandbox::Net::Proxy {
                proxy: 41234,
                local: &[8080],
            }
        );
        assert_eq!(
            fence_with(&policy, |_| Err("no user namespaces".into())),
            reagent_sandbox::Net::Off
        );
        let bridgeless = parse(&["--net-proxy", "127.0.0.1:41234", "--", "sh"]).expect("parses");
        assert_eq!(
            fence_with(&bridgeless, |_| Err("no socket".into())),
            reagent_sandbox::Net::Off
        );
        // A bool keeps its own fence, and asks for no namespace at all.
        let off = parse(&["--", "sh"]).expect("parses");
        assert_eq!(
            fence_with(&off, |_| panic!("a bool needs no namespace")),
            reagent_sandbox::Net::Off
        );
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

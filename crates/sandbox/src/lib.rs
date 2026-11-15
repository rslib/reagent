//! OS sandboxes for one process and its children: a Seatbelt profile on macOS
//! and Landlock on Linux, with a seccomp filter when the network is off. One
//! more seccomp filter keeps a child in the process group it was put in.

use std::path::PathBuf;

/// What a sandboxed program may reach on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Net<'a> {
    /// No connection at all.
    Off,
    /// Any connection.
    All,
    /// Only the run's proxy on `proxy` and these local ports, which is how a
    /// run whose `[sandbox] network` names hosts reaches them. On Linux the
    /// helper first puts the program in a network namespace of its own with
    /// only loopback, and bridges `proxy` and each local port to the run's
    /// proxy; a namespace that cannot be made gives [`Net::Off`].
    Proxy { proxy: u16, local: &'a [u16] },
}

impl Net<'_> {
    /// Whether the program may open any connection it likes.
    pub fn is_all(&self) -> bool {
        matches!(self, Net::All)
    }
}

/// The Seatbelt profile: read and run anywhere, write only in `write` and a
/// few devices, signal only processes of this sandbox.
pub fn seatbelt_profile(net: Net<'_>, write: &[PathBuf]) -> Result<String, String> {
    let mut profile = String::from(concat!(
        "(version 1)\n",
        "(import \"system.sb\")\n",
        "(deny default)\n",
        "(allow process-fork)\n",
        "(allow process-exec)\n",
        "(allow process-info*)\n",
        "(allow signal (target same-sandbox))\n",
        "(allow mach*)\n",
        "(allow ipc*)\n",
        "(allow sysctl*)\n",
        "(allow system*)\n",
        "(allow pseudo-tty)\n",
        "(allow file-ioctl)\n",
        "(allow file-read*)\n",
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\")",
        " (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))\n",
    ));
    for dir in write {
        let text = dir
            .to_str()
            .ok_or_else(|| format!("write folder is not UTF-8: {}", dir.display()))?;
        profile.push_str("(allow file-write* (subpath \"");
        for ch in text.chars() {
            if ch == '"' || ch == '\\' {
                profile.push('\\');
            }
            profile.push(ch);
        }
        profile.push_str("\"))\n");
    }
    match net {
        Net::All => profile.push_str("(allow network*)\n"),
        Net::Off => profile.push_str("(system-network)\n"),
        // Seatbelt names a host of a `remote ip` filter only as `*` or
        // `localhost`, so a list of remote hosts is reached through the
        // run's own proxy, which runs outside this sandbox.
        Net::Proxy { proxy, local } => {
            profile.push_str("(allow network-outbound");
            for port in std::iter::once(proxy).chain(local.iter().copied()) {
                // Writing to a `String` cannot fail.
                let _ = std::fmt::Write::write_fmt(
                    &mut profile,
                    format_args!(" (remote ip \"localhost:{port}\")"),
                );
            }
            profile.push_str(")\n");
            profile.push_str("(system-network)\n");
        }
    }
    Ok(profile)
}

/// Sandboxes this process and every child it starts: read and run anywhere,
/// write only in `write` (canonical folders), and open network connections
/// only where `net` allows them.
#[cfg(target_os = "macos")]
pub fn restrict(net: Net<'_>, write: &[PathBuf]) -> Result<(), String> {
    seatbelt::apply(&seatbelt_profile(net, write)?)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod seatbelt {
    use std::ffi::{CStr, CString, c_char, c_int};

    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    /// Applies `profile` to this process and every child it starts.
    pub fn apply(profile: &str) -> Result<(), String> {
        let profile = CString::new(profile).map_err(|_| "the profile holds a NUL".to_string())?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: `profile` is a valid C string that outlives the call, and
        // `error` is a valid out pointer that sandbox_init sets on failure.
        let result = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
        if result == 0 {
            return Ok(());
        }
        if error.is_null() {
            return Err("sandbox_init failed".into());
        }
        // SAFETY: on failure sandbox_init sets `error` to a C string that
        // stays valid until sandbox_free_error, which is called once here.
        let text = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `error` came from sandbox_init and is freed exactly once.
        unsafe { sandbox_free_error(error) };
        Err(text)
    }
}

/// Sandboxes this thread and every child it starts with Landlock: read and
/// run anywhere, write only in `write`. Unless `net` allows any connection,
/// Landlock refuses TCP and a seccomp filter refuses every socket but a local
/// one.
///
/// [`Net::Proxy`] fences nothing on the network here: `self::proxy_namespace`
/// has already put this process in a namespace whose only interface is
/// loopback, so a socket reaches nothing but the bridge. It may only be passed
/// after that setup said `Ok`; a failed one is [`Net::Off`].
#[cfg(target_os = "linux")]
pub fn restrict(net: Net<'_>, write: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

    let network = matches!(net, Net::All | Net::Proxy { .. });
    let abi = ABI::V5;
    let fail = |error: landlock::RulesetError| error.to_string();
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(fail)?;
    if !network {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(fail)?;
    }
    let status = ruleset
        .create()
        .map_err(fail)?
        .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))
        .map_err(fail)?
        // Terminals and /dev/null need write and ioctl.
        .add_rules(path_beneath_rules(["/dev"], AccessFs::from_all(abi)))
        .map_err(fail)?
        .add_rules(path_beneath_rules(write, AccessFs::from_all(abi)))
        .map_err(fail)?
        .restrict_self()
        .map_err(fail)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("this kernel has no Landlock".into());
    }
    if !network {
        seccomp::deny_ip_sockets()?;
    }
    Ok(())
}

/// Refuses a new session or process group inside a sandbox, so a child cannot
/// leave the process group reagent signals.
///
/// Linux on x86_64 and aarch64 installs one more seccomp filter, on top of the
/// network one, whose `setsid` and `setpgid` answer `EPERM`. The two filters
/// stack, so this keeps working whether the network filter is on or off. On
/// macOS nothing is installed and `Ok` is the answer: a Seatbelt profile
/// cannot refuse these calls. On every other Linux architecture nothing is
/// installed either, for the same reason as the network filter: no filter
/// exists there for these syscalls, and failing would drop the whole sandbox.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn keep_group() -> Result<(), String> {
    seccomp::deny_group_escape()
}

/// Whether this machine can make a user and a network namespace, which is
/// what a host list needs on Linux. The question is asked in a child that
/// tries it and exits with the answer, so this process never changes
/// namespace.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn netns_available() -> bool {
    // SAFETY: the child only calls async-signal-safe syscalls and exits, so it
    // holds no lock of this process; the parent waits for it.
    unsafe {
        let child = libc::fork();
        if child < 0 {
            return false;
        }
        if child == 0 {
            let (uid, gid) = (libc::getuid(), libc::getgid());
            let made = libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) == 0
                && netns::map_ids(uid, gid).is_ok();
            libc::_exit(if made { 0 } else { 1 });
        }
        let mut status = 0;
        loop {
            let waited = libc::waitpid(child, &mut status, 0);
            if waited == child {
                break;
            }
            if waited < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return false;
            }
        }
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }
}

/// The unix socket a host-list proxy listens on for the bridge of a network
/// namespace: in a folder named after the proxy's port, under the temp folder
/// of the run.
#[cfg(target_os = "linux")]
pub fn proxy_socket_path(port: u16) -> PathBuf {
    std::env::temp_dir()
        .join(format!("reagent-proxy-{port}"))
        .join("proxy.sock")
}

/// Binds the unix socket of a host-list proxy in a folder of mode 0700, and
/// answers it non-blocking. The socket a killed run may have left in that
/// folder is removed first.
#[cfg(target_os = "linux")]
pub fn proxy_socket(port: u16) -> Result<std::os::unix::net::UnixListener, String> {
    netns::unix_listener(&proxy_socket_path(port))
}

/// Removes the unix socket of a host-list proxy and its folder.
#[cfg(target_os = "linux")]
pub fn remove_proxy_socket(port: u16) {
    netns::remove_socket(&proxy_socket_path(port));
}

/// Puts this process in a network namespace of its own with only loopback,
/// binds the proxy port and each of `local` inside it, and forks one bridge
/// process that joins each of them, byte for byte, to the run's proxy on the
/// unix socket `socket`. The bridge dies with this process.
///
/// Every step that fails is an error, so the caller fences the program as
/// [`Net::Off`] instead of letting it keep a network.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn proxy_namespace(proxy: u16, local: &[u16], socket: &std::path::Path) -> Result<(), String> {
    // The ids are read first: inside a new user namespace with no map yet, this
    // process reads as the overflow id.
    // SAFETY: plain getters of this process's ids.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // SAFETY: plain syscalls of this process, before any thread starts.
    unsafe {
        if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
            return Err(format!(
                "no user and network namespace: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    netns::map_ids(uid, gid)
        .map_err(|code| format!("the id map: {}", std::io::Error::from_raw_os_error(code)))?;
    netns::loopback_up()?;
    let mut listeners = Vec::with_capacity(2 * (local.len() + 1));
    let bound = netns::bind_loopback(proxy, netns::Target::Proxy, &mut listeners)
        .map_err(|error| format!("port {proxy}: {error}"))
        .and_then(|()| {
            for port in local {
                // An address of this namespace only, so a port a program of the
                // run holds is not why this fails.
                netns::bind_loopback(*port, netns::Target::Local(*port), &mut listeners)
                    .map_err(|error| format!("port {port}: {error}"))?;
            }
            Ok(())
        });
    if let Err(error) = bound {
        netns::close_listeners(&listeners);
        return Err(error);
    }
    netns::fork_bridge(listeners, socket)
}

/// No way to refuse a new session exists on this system, so this does nothing.
/// See the Linux version for what the filter does where it exists.
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub fn keep_group() -> Result<(), String> {
    Ok(())
}

/// The seccomp filter of a sandbox with no network.
///
/// Landlock refuses TCP only: UDP rights come in Linux 7.2, and no Landlock
/// version controls raw, ICMP or packet sockets. So `socket()` takes only
/// AF_UNIX, which tools need, and AF_NETLINK, which an unprivileged process
/// can only read; io_uring, which makes sockets without `socket()`, and the
/// syscalls of another architecture (i386 `socketcall`, x32) are refused.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[allow(unsafe_code)]
mod seccomp {
    #[cfg(target_arch = "x86_64")]
    use libc::BPF_JGE;
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
        SECCOMP_RET_DATA, SECCOMP_RET_ERRNO, sock_filter, sock_fprog,
    };

    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

    // Offsets into `struct seccomp_data`: the syscall number, the audit
    // architecture and the low half of the first argument (little-endian).
    const NR: u32 = 0;
    const AUDIT_ARCH: u32 = 4;
    const ARG0_LOW: u32 = 16;

    /// The x32 syscalls on x86_64 carry this bit in their number.
    #[cfg(target_arch = "x86_64")]
    const X32_BIT: u32 = 0x4000_0000;

    fn load(offset: u32) -> sock_filter {
        sock_filter {
            code: (BPF_LD | BPF_W | BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: offset,
        }
    }

    /// A jump that skips `jt` instructions when the test holds, else `jf`.
    fn jump(test: u32, value: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: (BPF_JMP | test | BPF_K) as u16,
            jt,
            jf,
            k: value,
        }
    }

    fn ret(action: u32) -> sock_filter {
        sock_filter {
            code: (BPF_RET | BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: action,
        }
    }

    fn errno(code: i32) -> u32 {
        SECCOMP_RET_ERRNO | (code as u32 & SECCOMP_RET_DATA)
    }

    /// The instructions before the three returns, whose places the jumps
    /// count from.
    #[cfg(target_arch = "x86_64")]
    const HEAD: usize = 9;
    #[cfg(target_arch = "aarch64")]
    const HEAD: usize = 8;

    /// The program: the head of tests, then deny (EACCES), no io_uring
    /// (ENOSYS) and allow, in that order.
    pub(crate) fn program() -> Vec<sock_filter> {
        let (deny, no_ring, allow) = (HEAD, HEAD + 1, HEAD + 2);
        // A jump at `from` to `target` skips the instructions between them.
        let to = |from: usize, target: usize| (target - from - 1) as u8;
        let mut program = Vec::with_capacity(HEAD + 3);
        program.push(load(AUDIT_ARCH));
        program.push(jump(BPF_JEQ, ARCH, 0, to(1, deny)));
        program.push(load(NR));
        #[cfg(target_arch = "x86_64")]
        program.push(jump(BPF_JGE, X32_BIT, to(program.len(), deny), 0));
        let at = program.len();
        program.push(jump(
            BPF_JEQ,
            libc::SYS_io_uring_setup as u32,
            to(at, no_ring),
            0,
        ));
        let at = program.len();
        program.push(jump(BPF_JEQ, libc::SYS_socket as u32, 0, to(at, allow)));
        program.push(load(ARG0_LOW));
        let at = program.len();
        program.push(jump(BPF_JEQ, libc::AF_UNIX as u32, to(at, allow), 0));
        let at = program.len();
        program.push(jump(BPF_JEQ, libc::AF_NETLINK as u32, to(at, allow), 0));
        debug_assert_eq!(program.len(), HEAD);
        program.push(ret(errno(libc::EACCES)));
        program.push(ret(errno(libc::ENOSYS)));
        program.push(ret(SECCOMP_RET_ALLOW));
        program
    }

    /// The program of a sandbox whose children stay in their process group:
    /// `setsid` and `setpgid` answer `EPERM`, everything else is allowed.
    pub(crate) fn group_program() -> Vec<sock_filter> {
        // The instructions before the two returns.
        #[cfg(target_arch = "x86_64")]
        const HEAD: usize = 6;
        #[cfg(target_arch = "aarch64")]
        const HEAD: usize = 5;
        let (deny, allow) = (HEAD, HEAD + 1);
        let to = |from: usize, target: usize| (target - from - 1) as u8;
        let mut program = Vec::with_capacity(HEAD + 2);
        program.push(load(AUDIT_ARCH));
        program.push(jump(BPF_JEQ, ARCH, 0, to(1, deny)));
        program.push(load(NR));
        #[cfg(target_arch = "x86_64")]
        program.push(jump(BPF_JGE, X32_BIT, to(program.len(), deny), 0));
        let at = program.len();
        program.push(jump(BPF_JEQ, libc::SYS_setsid as u32, to(at, deny), 0));
        let at = program.len();
        // The last test sits right before the deny return, so a miss must skip
        // over it to the allow.
        program.push(jump(
            BPF_JEQ,
            libc::SYS_setpgid as u32,
            to(at, deny),
            to(at, allow),
        ));
        debug_assert_eq!(program.len(), HEAD);
        program.push(ret(errno(libc::EPERM)));
        program.push(ret(SECCOMP_RET_ALLOW));
        program
    }

    /// Installs one filter program on this thread, on top of what is there
    /// already; every child it starts keeps it.
    fn install(mut program: Vec<sock_filter>, what: &str) -> Result<(), String> {
        let filter = sock_fprog {
            len: program.len() as u16,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: plain prctl calls; `filter` points at `program`, which
        // outlives the call, and the kernel copies the program.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(format!("no_new_privs: {}", std::io::Error::last_os_error()));
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &filter as *const sock_fprog,
            ) != 0
            {
                return Err(format!("{what}: {}", std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// Installs the filter that refuses a new session or process group.
    pub(crate) fn deny_group_escape() -> Result<(), String> {
        install(group_program(), "the process group filter")
    }

    /// Installs the filter on this thread; every child it starts keeps it.
    pub(crate) fn deny_ip_sockets() -> Result<(), String> {
        install(program(), "the network filter")
    }
}

/// No network filter exists for this architecture. Landlock still fences the
/// files and TCP: failing here would make `auto` drop the whole sandbox.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
mod seccomp {
    pub(crate) fn deny_ip_sockets() -> Result<(), String> {
        Ok(())
    }
}

/// The network namespace of a host list, and the bridge that carries it.
///
/// Landlock filters TCP by port and never by host, so a host list needs a
/// network namespace whose only interface is loopback: a socket there reaches
/// nothing. The proxy port and each local port are bound inside it, and one
/// bridge process joins them, byte for byte, to the run's proxy on a unix
/// socket, which lives in the run's own namespace.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod netns {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::Arc;

    /// `SIOCGIFFLAGS` and `SIOCSIFFLAGS` of `<linux/sockios.h>`: read and set
    /// the flags of an interface.
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
    /// `IFF_UP` of `<net/if.h>`.
    const IFF_UP: libc::c_short = 0x1;
    /// The most connections one listener of the bridge has waiting.
    const BACKLOG: libc::c_int = 128;
    /// The bytes one direction of a bridged connection copies at a time.
    const BUFFER: usize = 16 * 1024;
    /// The most bytes of the proxy's reply head the bridge reads.
    const HEAD: usize = 1024;
    /// The end of the file descriptor table the bridge closes.
    const FDS: libc::c_int = 4096;

    const SETGROUPS: &[u8] = b"/proc/self/setgroups\0";
    const UID_MAP: &[u8] = b"/proc/self/uid_map\0";
    const GID_MAP: &[u8] = b"/proc/self/gid_map\0";

    /// `struct ifreq` of `<net/if.h>`: the name of an interface and the flags
    /// that follow it.
    #[repr(C)]
    struct IfReq {
        name: [libc::c_char; 16],
        flags: libc::c_short,
        rest: [u8; 22],
    }

    /// What a listener of the bridge hands a client to.
    #[derive(Debug, Clone, Copy)]
    pub(super) enum Target {
        /// The run's proxy itself: the bytes go to the unix socket as they are.
        Proxy,
        /// A local port of the list: the bridge opens a `CONNECT` tunnel to it
        /// through the proxy first, so the client sees the port's own bytes.
        Local(u16),
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// Writes `value` in decimal into `out` from `at`, and answers the offset
    /// after it. No allocation, so a forked child may call it.
    fn decimal_at(value: u32, out: &mut [u8], mut at: usize) -> usize {
        let start = at;
        let mut left = value;
        while left >= 10 {
            out[at] = b'0' + (left % 10) as u8;
            left /= 10;
            at += 1;
        }
        out[at] = b'0' + (left % 10) as u8;
        at += 1;
        out[start..at].reverse();
        at
    }

    /// `<id> <id> 1`, the map that keeps this process's own id inside its new
    /// user namespace.
    fn id_map(id: u32) -> ([u8; 32], usize) {
        let mut buffer = [0u8; 32];
        let mut at = decimal_at(id, &mut buffer, 0);
        buffer[at] = b' ';
        at += 1;
        at = decimal_at(id, &mut buffer, at);
        buffer[at] = b' ';
        buffer[at + 1] = b'1';
        (buffer, at + 2)
    }

    /// Writes `bytes` to the file named by `path`, a C string with its NUL.
    fn write_path(path: &[u8], bytes: &[u8]) -> Result<(), i32> {
        // SAFETY: `path` ends with a NUL and outlives the call; the fd is
        // closed here.
        unsafe {
            let fd = libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(errno());
            }
            let mut done = 0;
            while done < bytes.len() {
                let written = libc::write(fd, bytes[done..].as_ptr().cast(), bytes.len() - done);
                if written <= 0 {
                    let error = errno();
                    libc::close(fd);
                    return Err(error);
                }
                done += written as usize;
            }
            libc::close(fd);
        }
        Ok(())
    }

    /// Maps this process's own user and group into the user namespace it just
    /// entered, so the files it writes keep the ids of the user. `uid` and
    /// `gid` are the ids it had before the namespace: inside one with no map
    /// yet, this process reads as the overflow id.
    pub(super) fn map_ids(uid: u32, gid: u32) -> Result<(), i32> {
        // A kernel without a setgroups file has nothing to deny.
        match write_path(SETGROUPS, b"deny") {
            Ok(()) | Err(libc::ENOENT) => {}
            Err(error) => return Err(error),
        }
        let (buffer, len) = id_map(uid);
        write_path(UID_MAP, &buffer[..len])?;
        let (buffer, len) = id_map(gid);
        write_path(GID_MAP, &buffer[..len])
    }

    /// Brings `lo` up in the new namespace, where it starts down.
    pub(super) fn loopback_up() -> Result<(), String> {
        // SAFETY: plain socket and ioctl calls on a local `ifreq`.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(format!("lo: {}", std::io::Error::last_os_error()));
            }
            let mut request = IfReq {
                name: [0; 16],
                flags: 0,
                rest: [0; 22],
            };
            request.name[..2].copy_from_slice(&[b'l' as libc::c_char, b'o' as libc::c_char]);
            if libc::ioctl(fd, SIOCGIFFLAGS, &mut request) != 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("lo: {error}"));
            }
            request.flags |= IFF_UP;
            if libc::ioctl(fd, SIOCSIFFLAGS, &request) != 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("lo: {error}"));
            }
            libc::close(fd);
        }
        Ok(())
    }

    /// One non-blocking listening socket: `address` as its bytes and length.
    fn listen_at(domain: libc::c_int, address: &[u8]) -> Result<libc::c_int, String> {
        // SAFETY: plain socket calls; `address` holds a `sockaddr` of that
        // length, and the fd is closed on failure.
        unsafe {
            let fd = libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(format!("socket: {}", std::io::Error::last_os_error()));
            }
            let one: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
            if libc::bind(
                fd,
                address.as_ptr().cast(),
                address.len() as libc::socklen_t,
            ) != 0
            {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("{error}"));
            }
            if libc::listen(fd, BACKLOG) != 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("{error}"));
            }
            Ok(fd)
        }
    }

    /// The bytes of a `sockaddr_in` for `127.0.0.1:port`.
    fn loopback_v4(port: u16) -> [u8; size_of::<libc::sockaddr_in>()] {
        let address = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            // The four bytes of 127.0.0.1 in the order they sit in memory.
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: every byte of a `sockaddr_in` is readable and `address` is
        // one of them.
        unsafe {
            let mut bytes = [0u8; size_of::<libc::sockaddr_in>()];
            std::ptr::copy_nonoverlapping(
                (&address as *const libc::sockaddr_in).cast::<u8>(),
                bytes.as_mut_ptr(),
                bytes.len(),
            );
            bytes
        }
    }

    /// The bytes of a `sockaddr_in6` for `[::1]:port`.
    fn loopback_v6(port: u16) -> [u8; size_of::<libc::sockaddr_in6>()] {
        let mut address = libc::sockaddr_in6 {
            sin6_family: libc::AF_INET6 as libc::sa_family_t,
            sin6_port: port.to_be(),
            sin6_flowinfo: 0,
            sin6_addr: libc::in6_addr { s6_addr: [0; 16] },
            sin6_scope_id: 0,
        };
        address.sin6_addr.s6_addr[15] = 1;
        // SAFETY: every byte of a `sockaddr_in6` is readable and `address` is
        // one of them.
        unsafe {
            let mut bytes = [0u8; size_of::<libc::sockaddr_in6>()];
            std::ptr::copy_nonoverlapping(
                (&address as *const libc::sockaddr_in6).cast::<u8>(),
                bytes.as_mut_ptr(),
                bytes.len(),
            );
            bytes
        }
    }

    /// Binds `127.0.0.1:port` of the new namespace, and `[::1]:port` too when
    /// that binds, and adds each listener to `out` with `target`. A failed
    /// IPv4 bind is an error; the kernel's own answer for a port that is taken
    /// is why.
    pub(super) fn bind_loopback(
        port: u16,
        target: Target,
        out: &mut Vec<(libc::c_int, Target)>,
    ) -> Result<(), String> {
        out.push((listen_at(libc::AF_INET, &loopback_v4(port))?, target));
        // An IPv6 listener is an extra way in, never the only one, so a kernel
        // without it loses nothing.
        if let Ok(fd) = listen_at(libc::AF_INET6, &loopback_v6(port)) {
            out.push((fd, target));
        }
        Ok(())
    }

    /// Forks the bridge, which joins every listener to the unix socket
    /// `socket`. The parent closes its own copies of the listeners and goes on.
    pub(super) fn fork_bridge(
        listeners: Vec<(libc::c_int, Target)>,
        socket: &std::path::Path,
    ) -> Result<(), String> {
        let socket = CString::new(socket.as_os_str().as_bytes())
            .map_err(|_| "the proxy socket path holds a NUL".to_string())?;
        // SAFETY: plain calls; `parent` is the pid the bridge checks its own
        // parent against.
        let parent = unsafe { libc::getpid() };
        // SAFETY: the helper is one thread here, and the child only calls
        // syscalls before it starts threads of its own; the parent closes the
        // listeners it just passed on.
        unsafe {
            let child = libc::fork();
            if child < 0 {
                let error = std::io::Error::last_os_error();
                close_listeners(&listeners);
                return Err(format!("fork: {error}"));
            }
            if child == 0 {
                bridge(listeners, socket, parent);
            }
            close_listeners(&listeners);
        }
        Ok(())
    }

    /// Closes the listeners this process holds: its own copies once the bridge
    /// has them, or all of them when the bridge was never forked.
    pub(super) fn close_listeners(listeners: &[(libc::c_int, Target)]) {
        for (fd, _) in listeners {
            // SAFETY: this process owns the listener and closes it once.
            unsafe { libc::close(*fd) };
        }
    }

    /// The bridge itself, in the forked child: it never returns.
    fn bridge(listeners: Vec<(libc::c_int, Target)>, socket: CString, parent: libc::pid_t) -> ! {
        // SAFETY: plain calls of the child; a refused connection must not kill
        // it, and it must die with the program that started it.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        }
        // The program may have ended between the fork and the line above, and
        // the signal would not come.
        // SAFETY: a plain getter of the parent pid.
        if unsafe { libc::getppid() } != parent {
            // SAFETY: `_exit` ends the child with no unwinding.
            unsafe { libc::_exit(0) }
        }
        let keep: Vec<libc::c_int> = listeners.iter().map(|(fd, _)| *fd).collect();
        close_others(&keep);
        let socket = Arc::new(socket);
        let mut servers = Vec::with_capacity(listeners.len());
        for (fd, target) in listeners {
            let socket = Arc::clone(&socket);
            servers.push(std::thread::spawn(move || serve(fd, target, &socket)));
        }
        for server in servers {
            let _ = server.join();
        }
        // SAFETY: `_exit` ends the child with no unwinding and no lock held.
        unsafe { libc::_exit(0) }
    }

    /// Closes every file descriptor of the bridge it does not need, its own
    /// input and output included: a program that ends must not wait for the
    /// bridge to let go of its pipes.
    fn close_others(keep: &[libc::c_int]) {
        for fd in 0..FDS {
            if !keep.contains(&fd) {
                // SAFETY: closing a descriptor this process may not own is an
                // error the call reports.
                unsafe { libc::close(fd) };
            }
        }
    }

    /// Accepts on one listener and serves each connection on a thread of its
    /// own.
    fn serve(fd: libc::c_int, target: Target, socket: &Arc<CString>) {
        loop {
            // SAFETY: plain accept4 on a listener this thread owns.
            let client = unsafe {
                libc::accept4(
                    fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if client < 0 {
                // An interrupted accept asks again; any other failure ends the
                // listener, since nothing would make it work.
                if errno() == libc::EINTR {
                    continue;
                }
                return;
            }
            let socket = Arc::clone(socket);
            std::thread::spawn(move || connect_and_join(client, target, &socket));
        }
    }

    /// Opens the bridge's end of one client and joins the two.
    fn connect_and_join(client: libc::c_int, target: Target, socket: &CStr) {
        let upstream = match unix_connect(socket) {
            Ok(upstream) => upstream,
            Err(_) => {
                // SAFETY: this thread owns the client.
                unsafe { libc::close(client) };
                return;
            }
        };
        match target {
            Target::Proxy => join(client, upstream),
            Target::Local(port) => {
                let head = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
                if write_all(upstream, head.as_bytes()).is_err() || !tunnel(client, upstream) {
                    // SAFETY: this thread owns both sockets.
                    unsafe {
                        libc::close(client);
                        libc::close(upstream);
                    }
                    return;
                }
                join(client, upstream);
            }
        }
    }

    /// Reads the proxy's answer to a `CONNECT` and answers whether it holds a
    /// tunnel. Any bytes past the head are the target's own, and go to the
    /// client.
    fn tunnel(client: libc::c_int, upstream: libc::c_int) -> bool {
        let mut head = [0u8; HEAD];
        let mut read = 0;
        while read < head.len() {
            let got = match read_some(upstream, &mut head[read..]) {
                Some(0) | None => return false,
                Some(got) => got,
            };
            read += got;
            if let Some(end) = head[..read]
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                let end = end + 4;
                if !is_200(&head[..end]) {
                    return false;
                }
                return head[end..read].is_empty() || write_all(client, &head[end..read]).is_ok();
            }
        }
        false
    }

    /// Whether a reply head holds the status `200`.
    fn is_200(head: &[u8]) -> bool {
        let line = &head[..head
            .windows(2)
            .position(|window| window == b"\r\n")
            .unwrap_or(head.len())];
        let mut parts = line.split(|byte| *byte == b' ');
        parts
            .next()
            .is_some_and(|version| version.starts_with(b"HTTP/"))
            && parts.next() == Some(b"200")
    }

    /// The bridge's connection to the run's proxy, on the run's unix socket.
    fn unix_connect(socket: &CStr) -> Result<libc::c_int, String> {
        let path = socket.to_bytes();
        let mut address = libc::sockaddr_un {
            sun_family: libc::AF_UNIX as libc::sa_family_t,
            sun_path: [0; 108],
        };
        if path.len() >= address.sun_path.len() {
            return Err("the proxy socket path is too long".into());
        }
        for (at, byte) in path.iter().enumerate() {
            address.sun_path[at] = *byte as libc::c_char;
        }
        // SAFETY: plain socket calls; `address` is a `sockaddr_un` whose path
        // ends with a NUL, and the fd is closed on failure.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(format!("socket: {}", std::io::Error::last_os_error()));
            }
            let length = size_of::<libc::sa_family_t>() + path.len() + 1;
            if libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                length as libc::socklen_t,
            ) != 0
            {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("{error}"));
            }
            Ok(fd)
        }
    }

    /// Joins two sockets byte for byte, each direction on its own thread, and
    /// ends each one when the other closes.
    fn join(one: libc::c_int, other: libc::c_int) {
        let back = std::thread::spawn(move || {
            copy(other, one);
            // SAFETY: this thread writes nothing more to the other side.
            unsafe { libc::shutdown(one, libc::SHUT_WR) };
        });
        copy(one, other);
        // SAFETY: the far end of `other` reads its end of the stream now.
        unsafe { libc::shutdown(other, libc::SHUT_WR) };
        let _ = back.join();
        // SAFETY: both connections are done and this thread owns them.
        unsafe {
            libc::close(one);
            libc::close(other);
        }
    }

    /// Copies `from` to `to` until `from` closes, with one buffer for the whole
    /// direction.
    fn copy(from: libc::c_int, to: libc::c_int) {
        let mut buffer = vec![0u8; BUFFER];
        loop {
            let read = match read_some(from, &mut buffer) {
                Some(0) | None => return,
                Some(read) => read,
            };
            if write_all(to, &buffer[..read]).is_err() {
                return;
            }
        }
    }

    /// One read of `socket`, retried when a signal interrupts it: `None` on an
    /// error.
    fn read_some(socket: libc::c_int, buffer: &mut [u8]) -> Option<usize> {
        loop {
            // SAFETY: plain read into a buffer of the length given.
            let read = unsafe { libc::read(socket, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read >= 0 {
                return Some(read as usize);
            }
            if errno() != libc::EINTR {
                return None;
            }
        }
    }

    /// Writes every byte of `bytes` to `socket`.
    fn write_all(socket: libc::c_int, bytes: &[u8]) -> Result<(), ()> {
        let mut done = 0;
        while done < bytes.len() {
            // SAFETY: plain write from the rest of a buffer that outlives it.
            let written =
                unsafe { libc::write(socket, bytes[done..].as_ptr().cast(), bytes.len() - done) };
            if written < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                return Err(());
            }
            if written == 0 {
                return Err(());
            }
            done += written as usize;
        }
        Ok(())
    }

    /// Binds the unix socket of a host-list proxy, in a folder of mode 0700.
    pub(super) fn unix_listener(
        path: &std::path::Path,
    ) -> Result<std::os::unix::net::UnixListener, String> {
        let folder = path
            .parent()
            .ok_or_else(|| format!("{} has no folder", path.display()))?;
        let name = CString::new(folder.as_os_str().as_bytes())
            .map_err(|_| format!("{} holds a NUL", folder.display()))?;
        // SAFETY: plain mkdir of a path that outlives the call; a folder that
        // is there already, from a killed run, is the mode it wants.
        let made = unsafe { libc::mkdir(name.as_ptr(), 0o700) };
        if made != 0 && errno() != libc::EEXIST {
            return Err(format!(
                "{}: {}",
                folder.display(),
                std::io::Error::last_os_error()
            ));
        }
        unlink(path);
        let listener = std::os::unix::net::UnixListener::bind(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(listener)
    }

    /// Unlinks a socket and the folder that holds it, best effort.
    pub(super) fn remove_socket(path: &std::path::Path) {
        unlink(path);
        if let Some(folder) = path.parent() {
            remove_folder(folder);
        }
    }

    /// Unlinks a socket a killed run may have left at `path`.
    fn unlink(path: &std::path::Path) {
        let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: plain unlink of a path that outlives the call; a path that is
        // gone is an error this call does not report.
        unsafe { libc::unlink(name.as_ptr()) };
    }

    /// Removes an empty folder, best effort.
    fn remove_folder(folder: &std::path::Path) {
        let Ok(name) = CString::new(folder.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: plain rmdir of a path that outlives the call; a folder that
        // is not empty is an error this call does not report.
        unsafe { libc::rmdir(name.as_ptr()) };
    }
}

/// No sandbox exists on this system.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn restrict(_net: Net<'_>, _write: &[PathBuf]) -> Result<(), String> {
    Err("no sandbox on this system".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_exec_profile_grants_write_only_in_the_write_folders() {
        let profile = seatbelt_profile(Net::Off, &[PathBuf::from("/w/a\"b")]).expect("a profile");
        assert!(profile.contains("(allow process-exec)"), "{profile}");
        assert!(
            profile.contains("(allow file-write* (subpath \"/w/a\\\"b\"))"),
            "{profile}"
        );
        assert!(
            profile.contains("(allow signal (target same-sandbox))"),
            "{profile}"
        );
        assert!(!profile.contains("(allow network*)"), "{profile}");
        assert!(
            seatbelt_profile(Net::All, &[])
                .expect("a profile")
                .contains("(allow network*)\n")
        );
        assert!(
            seatbelt_profile(Net::Off, &[])
                .expect("a profile")
                .ends_with("(system-network)\n")
        );
    }

    #[test]
    fn host_list_profile_allows_the_proxy_and_the_local_ports_only() {
        let profile = seatbelt_profile(
            Net::Proxy {
                proxy: 5555,
                local: &[8080, 9000],
            },
            &[],
        )
        .expect("a profile");
        assert!(
            profile.contains(concat!(
                "(allow network-outbound (remote ip \"localhost:5555\")",
                " (remote ip \"localhost:8080\") (remote ip \"localhost:9000\"))\n",
            )),
            "{profile}"
        );
        assert!(!profile.contains("(allow network*)"), "{profile}");
        assert!(profile.contains("(system-network)\n"), "{profile}");
        // A proxy and no local port is still a fence, not the whole network.
        let alone = seatbelt_profile(
            Net::Proxy {
                proxy: 5555,
                local: &[],
            },
            &[],
        )
        .expect("a profile");
        assert!(
            alone.contains("(allow network-outbound (remote ip \"localhost:5555\"))\n"),
            "{alone}"
        );
    }

    /// The probe for a host list asks a child, so this process keeps its own
    /// namespace, and a machine where user namespaces are off answers no
    /// without failing.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_list_probe_answers_and_keeps_this_process_namespace() {
        let before = std::fs::read_link("/proc/self/ns/net");
        let answer = netns_available();
        let after = std::fs::read_link("/proc/self/ns/net");
        assert_eq!(before.ok(), after.ok(), "the probe moved this process");
        assert_eq!(answer, netns_available(), "the probe answered differently");
    }

    /// The unix socket of a host list sits in a folder of mode 0700 named after
    /// the proxy's port, a second bind replaces the socket a killed run left,
    /// and the folder goes when the run drops the proxy.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_list_socket_is_a_private_folder_named_after_the_port() {
        use std::os::unix::fs::PermissionsExt;

        // The port only names the folder here, so a live run's port is not
        // needed, and the id of this process keeps the name its own.
        let port = 40000 + (std::process::id() % 20000) as u16;
        let path = proxy_socket_path(port);
        let folder = path.parent().expect("a folder").to_path_buf();
        assert!(
            path.ends_with(format!("reagent-proxy-{port}/proxy.sock")),
            "{}",
            path.display()
        );
        let listener = proxy_socket(port).expect("the socket binds");
        let mode = folder.metadata().expect("the folder").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", folder.display());
        drop(listener);
        proxy_socket(port).expect("the socket binds over the old one");
        remove_proxy_socket(port);
        assert!(!folder.exists(), "{} stayed", folder.display());
    }

    /// Opens a socket and closes it, or returns the errno.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[allow(unsafe_code)]
    fn open(domain: i32, kind: i32, protocol: i32) -> Result<(), i32> {
        // SAFETY: plain syscalls; the fd is closed at once.
        let fd = unsafe { libc::socket(domain, kind | libc::SOCK_CLOEXEC, protocol) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        unsafe { libc::close(fd) };
        Ok(())
    }

    /// Runs `body` on a thread of its own with the network filter, so the rest
    /// of the test process keeps its sockets.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn filtered<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(move || {
            seccomp::deny_ip_sockets().expect("the filter installs");
            body()
        })
        .join()
        .expect("the filtered thread")
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn seccomp_refuses_every_ip_socket() {
        use libc::{AF_INET, AF_INET6, AF_PACKET, IPPROTO_ICMP, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM};
        // Without the filter UDP opens, so a refusal below is the filter's.
        assert_eq!(open(AF_INET, SOCK_DGRAM, 0), Ok(()));
        let refused = filtered(|| {
            [
                open(AF_INET, SOCK_DGRAM, 0),
                open(AF_INET6, SOCK_DGRAM, 0),
                open(AF_INET, SOCK_STREAM, 0),
                open(AF_INET6, SOCK_STREAM, 0),
                open(AF_INET, SOCK_DGRAM, IPPROTO_ICMP),
                open(AF_INET, SOCK_RAW, IPPROTO_ICMP),
                open(AF_PACKET, SOCK_RAW, 0),
            ]
        });
        for (index, result) in refused.iter().enumerate() {
            assert_eq!(*result, Err(libc::EACCES), "socket {index}");
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn seccomp_keeps_local_sockets() {
        let kept = filtered(|| {
            [
                open(libc::AF_UNIX, libc::SOCK_STREAM, 0),
                open(libc::AF_UNIX, libc::SOCK_DGRAM, 0),
                open(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE),
            ]
        });
        assert_eq!(kept, [Ok(()), Ok(()), Ok(())]);
    }

    /// Calls io_uring_setup for a one-entry ring: the errno, or `None` when a
    /// ring was made (and closed).
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[allow(unsafe_code)]
    fn ring_setup() -> Option<i32> {
        let mut params = [0u8; 120];
        // SAFETY: io_uring_setup reads and writes `params`, which is large
        // enough for `struct io_uring_params`; a ring fd made here is closed.
        let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) };
        if fd < 0 {
            return std::io::Error::last_os_error().raw_os_error();
        }
        unsafe { libc::close(fd as i32) };
        None
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn seccomp_refuses_io_uring_with_enosys() {
        // A kernel without io_uring already says ENOSYS, which proves nothing.
        if ring_setup() == Some(libc::ENOSYS) {
            return;
        }
        assert_eq!(filtered(ring_setup), Some(libc::ENOSYS));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    #[allow(unsafe_code)]
    fn seccomp_refuses_an_x32_syscall() {
        let errno = filtered(|| {
            // SAFETY: socket(2) through the x32 number; any fd made is closed.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_socket | 0x4000_0000,
                    libc::AF_UNIX,
                    libc::SOCK_STREAM,
                    0,
                )
            };
            if fd >= 0 {
                unsafe { libc::close(fd as i32) };
                return None;
            }
            std::io::Error::last_os_error().raw_os_error()
        });
        // Without the filter, a kernel without x32 says ENOSYS instead.
        assert_eq!(errno, Some(libc::EACCES));
    }

    /// Runs `body` on a thread of its own with the filter that keeps a child in
    /// its process group, so the rest of the test process keeps its own.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn grouped<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(move || {
            keep_group().expect("the filter installs");
            body()
        })
        .join()
        .expect("the filtered thread")
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    #[allow(unsafe_code)]
    fn keep_group_refuses_setsid() {
        let refused = grouped(|| {
            // SAFETY: plain syscalls that the filter answers with EPERM.
            let session = unsafe { libc::syscall(libc::SYS_setsid) };
            let session = if session < 0 {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            // SAFETY: setpgid takes two plain integers.
            let group = unsafe { libc::setpgid(0, 0) };
            let group = if group < 0 {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            (session, group)
        });
        assert_eq!(refused, (Some(libc::EPERM), Some(libc::EPERM)));
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    #[allow(unsafe_code)]
    fn keep_group_keeps_fork() {
        let (forked, status) = grouped(|| {
            // SAFETY: plain syscalls; the child leaves at once, with no lock of
            // the test process held and nothing to unwind.
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe { libc::_exit(0) }
            }
            if child < 0 {
                return (child, -1);
            }
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            (child, status)
        });
        assert!(forked > 0, "fork answered {forked}");
        assert_eq!(status, 0);
    }
}

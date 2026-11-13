//! OS sandboxes for one process and its children: a Seatbelt profile on macOS
//! and Landlock on Linux, with a seccomp filter when the network is off. One
//! more seccomp filter keeps a child in the process group it was put in.

use std::path::PathBuf;

/// The Seatbelt profile: read and run anywhere, write only in `write` and a
/// few devices, signal only processes of this sandbox.
pub fn seatbelt_profile(network: bool, write: &[PathBuf]) -> Result<String, String> {
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
    profile.push_str(if network {
        "(allow network*)\n"
    } else {
        "(system-network)\n"
    });
    Ok(profile)
}

/// Sandboxes this process and every child it starts: read and run anywhere,
/// write only in `write` (canonical folders), and open network connections
/// only when `network` is true.
#[cfg(target_os = "macos")]
pub fn restrict(network: bool, write: &[PathBuf]) -> Result<(), String> {
    seatbelt::apply(&seatbelt_profile(network, write)?)
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
/// run anywhere, write only in `write`. Unless `network` is true, Landlock
/// refuses TCP and a seccomp filter refuses every socket but a local one.
#[cfg(target_os = "linux")]
pub fn restrict(network: bool, write: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

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

/// No sandbox exists on this system.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn restrict(_network: bool, _write: &[PathBuf]) -> Result<(), String> {
    Err("no sandbox on this system".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_exec_profile_grants_write_only_in_the_write_folders() {
        let profile = seatbelt_profile(false, &[PathBuf::from("/w/a\"b")]).expect("a profile");
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
            seatbelt_profile(true, &[])
                .expect("a profile")
                .contains("(allow network*)")
        );
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

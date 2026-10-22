//! OS sandboxes for one process and its children: a Seatbelt profile on macOS
//! and Landlock on Linux.

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
/// run anywhere, write only in `write`, and no TCP unless `network` is true.
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
    Ok(())
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
}

//! Where sessions live and how their folders and files are named.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::part::{EXT, PART_ID_HEX};

/// Folder under the state home that holds every session.
pub const ROOT_DIR: &str = "reagent";
/// Folder under the root that holds every session.
pub const SESSIONS_DIR: &str = "sessions";
/// Name of the session list of one folder.
pub const LIST_FILE: &str = "sessions.list";
/// Most bytes a cwd key may take.
pub const KEY_MAX_BYTES: usize = 120;
/// Hex digits of the path hash a cut key ends with.
pub const KEY_HASH_DIGITS: usize = 8;
/// Characters of a session id.
pub const SESSION_ID_LEN: usize = 26;
/// Bits of a session id that hold the creation time.
pub const TIME_BITS: u32 = 48;
/// Bits of a session id that are random.
pub const RANDOM_BITS: u32 = 80;
/// Bits one character of a session id carries.
const ID_BITS: u32 = 5;
/// Bits of an FNV-1a hash.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// Multiplier of the FNV-1a hash.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Characters of the Crockford base32 alphabet, in order.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters that may stay in a cwd key.
fn keeps(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// `<state_home>/reagent/sessions`.
pub fn sessions_root(state_home: &Path) -> PathBuf {
    state_home.join(ROOT_DIR).join(SESSIONS_DIR)
}

/// The folder name of one working directory.
///
/// Every byte outside `[A-Za-z0-9._-]` becomes `-`. A key over
/// [`KEY_MAX_BYTES`] is cut and gets a `-` and eight hex digits of an FNV-1a
/// hash of the whole path, so two long paths stay apart. Pass an absolute
/// path; a relative one keys its own text.
pub fn cwd_key(cwd: &Path) -> String {
    let bytes = cwd.as_os_str().as_encoded_bytes();
    let mut key = String::with_capacity(KEY_MAX_BYTES);
    for byte in bytes {
        key.push(if keeps(*byte) { *byte as char } else { '-' });
    }
    if key.len() > KEY_MAX_BYTES {
        key.truncate(KEY_MAX_BYTES - KEY_HASH_DIGITS - 1);
        key.push('-');
        push_hex(&mut key, fnv1a(bytes) & 0xffff_ffff, KEY_HASH_DIGITS);
    }
    key
}

/// A session id: 48 bits of milliseconds then 80 random bits, in Crockford
/// base32, so ids sort by creation time.
pub fn session_id(ms: u64, random: u128) -> String {
    let time = u128::from(ms & ((1u64 << TIME_BITS) - 1));
    let noise = random & ((1u128 << RANDOM_BITS) - 1);
    let value = (time << RANDOM_BITS) | noise;
    let mut id = String::with_capacity(SESSION_ID_LEN);
    for index in (0..SESSION_ID_LEN as u32).rev() {
        let at = (value >> (index * ID_BITS)) & 0x1f;
        id.push(CROCKFORD[at as usize] as char);
    }
    id
}

/// A session id for now, with fresh random bits.
pub fn new_session_id() -> String {
    session_id(
        now_ms(),
        u128::from(random_u64()) << 64 | u128::from(random_u64()),
    )
}

/// A fresh random `u64`.
///
/// Two hashers a process seeds from the OS, mixed with a counter and the
/// clock: enough for names, not for keys.
pub fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u64(now_ms());
    hasher.finish()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// The file name of one part: sixteen hex digits and the part extension.
pub fn part_file_name(part_id: u64) -> String {
    let mut name = String::with_capacity(PART_ID_HEX + 1 + EXT.len());
    push_hex(&mut name, part_id, PART_ID_HEX);
    name.push('.');
    name.push_str(EXT);
    name
}

/// The path of one part in a session folder.
pub fn part_path(dir: &Path, part_id: u64) -> PathBuf {
    dir.join(part_file_name(part_id))
}

/// The part ids of a session folder, in the order the folder lists them.
#[expect(
    clippy::disallowed_methods,
    reason = "one folder listing when a session is opened"
)]
pub fn part_ids(dir: &Path) -> io::Result<Vec<u64>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(id) = part_id_of(entry.file_name().as_os_str()) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// The part id a file name carries, if it is a part file's.
pub fn part_id_of(name: &OsStr) -> Option<u64> {
    let text = name.to_str()?;
    let digits = text.strip_suffix(&format!(".{EXT}"))?;
    if digits.len() != PART_ID_HEX {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// The session folders under one cwd folder.
#[expect(
    clippy::disallowed_methods,
    reason = "one folder listing when the session list is rebuilt"
)]
pub fn session_dirs(folder: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// The session list of one cwd folder.
pub fn list_path(folder: &Path) -> PathBuf {
    folder.join(LIST_FILE)
}

/// Writes `value` as lowercase hex in at least `digits` characters.
fn push_hex(out: &mut String, value: u64, digits: usize) {
    let mut width = 1;
    while width < digits && value >> (width * 4) != 0 {
        width += 1;
    }
    for _ in width..digits {
        out.push('0');
    }
    for place in (0..width).rev() {
        let digit = ((value >> (place * 4)) & 0xf) as u8;
        out.push(if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        });
    }
}

/// FNV-1a over `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sessions_root_is_under_the_state_home() {
        let root = sessions_root(Path::new("/state"));
        assert_eq!(root, Path::new("/state/reagent/sessions"));
        assert_eq!(
            list_path(Path::new("/state/reagent/sessions/Users-x")),
            Path::new("/state/reagent/sessions/Users-x/sessions.list")
        );
    }

    #[test]
    fn cwd_key_replaces_every_byte_outside_the_set() {
        assert_eq!(cwd_key(Path::new("/Users/ray/Code")), "-Users-ray-Code");
        assert_eq!(
            cwd_key(Path::new("/a b/c:d(e)")),
            "-a-b-c-d-e-",
            "every byte outside the set becomes one dash"
        );
        assert_eq!(cwd_key(Path::new("/x.y_z-1")), "-x.y_z-1");
        assert_eq!(
            cwd_key(Path::new("/héllo")),
            "-h--llo",
            "a multi-byte character becomes one dash per byte"
        );
    }

    #[test]
    fn a_long_cwd_key_is_cut_and_hashed() {
        let long = Path::new(
            "/a/very/long/path/that/keeps/going/and/going/for/a/while/beyond/the/key/limit/of/one/hundred/and/twenty/bytes/and/more/still",
        );
        let key = cwd_key(long);
        assert_eq!(key.len(), KEY_MAX_BYTES);
        assert!(key.is_char_boundary(key.len()));
        let cut = KEY_MAX_BYTES - KEY_HASH_DIGITS - 1;
        assert_eq!(key.as_bytes()[cut], b'-');
        assert!(key[cut + 1..].chars().all(|c| c.is_ascii_hexdigit()));

        let mut other = long.to_path_buf();
        other.push("x");
        assert_ne!(cwd_key(&other), key, "the hash separates long paths");
        assert_eq!(cwd_key(long), key, "and it is stable");
    }

    #[test]
    fn session_ids_sort_by_time() {
        let early = session_id(1_700_000_000_000, 0);
        let later = session_id(1_700_000_000_001, 0);
        let last = session_id(1_700_000_000_001, u128::MAX);
        assert!(early < later, "{early} < {later}");
        assert!(later < last, "{later} < {last}");
    }

    #[test]
    fn a_session_id_is_26_crockford_characters() {
        let id = session_id(1_700_000_000_000, 12345);
        assert_eq!(id.len(), SESSION_ID_LEN);
        assert!(
            id.bytes().all(|byte| CROCKFORD.contains(&byte)),
            "{id} is Crockford base32"
        );
        assert_eq!(id, id.to_uppercase());
        assert_eq!(
            session_id(1_700_000_000_000, 12345),
            id,
            "the id depends on its inputs only"
        );
    }

    #[test]
    fn a_session_id_decodes_back_to_its_time() {
        let ms = 1_700_000_000_000u64;
        let id = session_id(ms, 0);
        let mut value: u128 = 0;
        for byte in id.bytes() {
            let digit = CROCKFORD
                .iter()
                .position(|known| *known == byte)
                .expect("a Crockford digit");
            value = value << ID_BITS | digit as u128;
        }
        assert_eq!((value >> RANDOM_BITS) as u64, ms);
        assert_eq!(value & ((1u128 << RANDOM_BITS) - 1), 0);
    }

    #[test]
    fn new_session_ids_change() {
        let first = new_session_id();
        let second = new_session_id();
        assert_ne!(first, second);
        assert_ne!(random_u64(), random_u64());
    }

    #[test]
    fn now_ms_is_past_2024() {
        assert!(now_ms() > 1_704_067_200_000, "the clock is set");
    }

    #[test]
    fn a_part_file_name_carries_the_id() {
        let name = part_file_name(0x0a0b_0c0d_0e0f_1011);
        assert_eq!(name, "0a0b0c0d0e0f1011.rsp");
        assert_eq!(part_id_of(OsStr::new(&name)), Some(0x0a0b_0c0d_0e0f_1011));
        assert_eq!(part_id_of(OsStr::new("sessions.list")), None);
        assert_eq!(part_id_of(OsStr::new("abc.rsp")), None);
        assert_eq!(part_id_of(OsStr::new("zzzzzzzzzzzzzzzz.rsp")), None);
        assert_eq!(
            part_path(Path::new("/s"), 1),
            Path::new("/s/0000000000000001.rsp")
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes its own fixture folder"
    )]
    fn a_folder_lists_its_parts_and_sessions() {
        let dir = TempDir::new().expect("temp dir");
        let folder = dir.path().to_path_buf();
        assert_eq!(part_ids(&folder).expect("parts"), Vec::<u64>::new());
        crate::testutil::write(&part_path(&folder, 2), b"");
        crate::testutil::write(&part_path(&folder, 1), b"");
        crate::testutil::write(&folder.join("sessions.list"), b"");
        assert_eq!(part_ids(&folder).expect("parts"), vec![1, 2]);

        std::fs::create_dir(folder.join("session-b")).expect("dir");
        let dirs = session_dirs(&folder).expect("dirs");
        assert_eq!(dirs.len(), 1, "the files are not sessions: {dirs:?}");
        assert!(dirs[0].ends_with("session-b"));
    }
}

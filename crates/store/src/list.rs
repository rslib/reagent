//! The session list of one working directory: an append-only cache.
//!
//! Every entry is a frame and the last frame for a session wins, so a torn
//! tail costs at most the newest entry. A list that is missing, too large or
//! torn is rebuilt from the part headers.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::codec::{Decoder, Encoder, Text, Value};
use crate::frame::{self, NO_SEQ};
use crate::kind;
use crate::part::{Commit, Header, invalid};
use crate::paths;
use crate::recover::Part;

/// Most bytes of a list file the store reads before rebuilding it.
pub const MAX_LIST_BYTES: u64 = 4 * 1024 * 1024;
/// Characters a title keeps.
pub const TITLE_CHARS: usize = 60;
/// The seed of a list frame: the list has no part id.
const LIST_SEED: u64 = 0;

/// Entry field numbers, fixed for the life of the format.
const FIELD_SESSION: u32 = 1;
const FIELD_UPDATED_MS: u32 = 2;
const FIELD_TURNS: u32 = 3;
const FIELD_TITLE: u32 = 4;
const FIELD_PROFILE: u32 = 5;
const FIELD_PROVIDER: u32 = 6;
const FIELD_MODEL: u32 = 7;
const FIELD_PART: u32 = 8;
const FIELD_STATE: u32 = 9;
const FIELD_HEADLESS: u32 = 10;

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The run is still going.
    Open,
    /// The run ended on its own.
    Done,
    /// The run was stopped or crashed.
    Interrupted,
}

impl State {
    /// The number this state is written as.
    fn code(self) -> u64 {
        match self {
            State::Open => 1,
            State::Done => 2,
            State::Interrupted => 3,
        }
    }

    /// The state a number stands for.
    fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(State::Open),
            2 => Some(State::Done),
            3 => Some(State::Interrupted),
            _ => None,
        }
    }
}

/// One session as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The session id.
    pub session: String,
    /// Time of the last record, in milliseconds.
    pub updated_ms: u64,
    /// Turns the session has.
    pub turns: u64,
    /// The first user message, cut.
    pub title: String,
    /// The profile the run used.
    pub profile: String,
    /// The provider the run used.
    pub provider: String,
    /// The model the run used.
    pub model: String,
    /// The part written last.
    pub part: u64,
    /// How the run ended.
    pub state: State,
    /// Whether a headless run wrote the part.
    pub headless: bool,
}

impl Entry {
    /// Writes the entry as a list frame payload.
    fn encode(&self, out: &mut Vec<u8>) {
        let mut enc = Encoder::new(out);
        enc.str(FIELD_SESSION, &self.session);
        enc.u64(FIELD_UPDATED_MS, self.updated_ms);
        enc.u64(FIELD_TURNS, self.turns);
        enc.str(FIELD_TITLE, &self.title);
        enc.str(FIELD_PROFILE, &self.profile);
        enc.str(FIELD_PROVIDER, &self.provider);
        enc.str(FIELD_MODEL, &self.model);
        enc.u64(FIELD_PART, self.part);
        enc.u64(FIELD_STATE, self.state.code());
        // An entry of a run with a terminal UI writes no flag, so an old list
        // and a new one cost the same bytes.
        if self.headless {
            enc.bool(FIELD_HEADLESS, true);
        }
    }

    /// Reads a list frame payload.
    fn decode(payload: &Bytes) -> io::Result<Self> {
        let mut decoder = Decoder::new(payload);
        let mut entry = Entry {
            session: String::new(),
            updated_ms: 0,
            turns: 0,
            title: String::new(),
            profile: String::new(),
            provider: String::new(),
            model: String::new(),
            part: 0,
            state: State::Open,
            headless: false,
        };
        let mut state = State::Open.code();
        while let Some((field, value)) = decoder.next_field().map_err(codec_error)? {
            match field {
                FIELD_SESSION => entry.session = text_of(&value)?.as_str().to_owned(),
                FIELD_UPDATED_MS => entry.updated_ms = varint_of(&value)?,
                FIELD_TURNS => entry.turns = varint_of(&value)?,
                FIELD_TITLE => entry.title = text_of(&value)?.as_str().to_owned(),
                FIELD_PROFILE => entry.profile = text_of(&value)?.as_str().to_owned(),
                FIELD_PROVIDER => entry.provider = text_of(&value)?.as_str().to_owned(),
                FIELD_MODEL => entry.model = text_of(&value)?.as_str().to_owned(),
                FIELD_PART => entry.part = varint_of(&value)?,
                FIELD_STATE => state = varint_of(&value)?,
                FIELD_HEADLESS => entry.headless = varint_of(&value)? != 0,
                _ => {}
            }
        }
        if entry.session.is_empty() {
            return Err(invalid("list entry without a session"));
        }
        entry.state = State::from_code(state).ok_or_else(|| invalid("list entry state"))?;
        Ok(entry)
    }
}

/// What a prefix lookup found.
#[derive(Debug)]
pub enum Lookup<'a> {
    /// No session id starts with the prefix.
    None,
    /// Exactly one session id starts with it.
    One(&'a Entry),
    /// Several do.
    Many(Vec<&'a Entry>),
}

/// The list of the sessions of one working directory.
///
/// Appends are one `write` with `O_APPEND` and no sync: the list is a cache,
/// and the parts are the truth.
#[derive(Debug)]
#[expect(
    clippy::disallowed_types,
    reason = "the list is one small cache file, kept open for appends"
)]
pub struct SessionList {
    path: PathBuf,
    file: std::fs::File,
    /// Newest first.
    entries: Vec<Entry>,
}

impl SessionList {
    /// Opens the list of `folder`, rebuilding it when it is missing or torn.
    ///
    /// `meta` turns a part header, and the commit it reached, into an entry;
    /// that is what a rebuild has to work with.
    pub fn open<F>(folder: &Path, meta: F) -> io::Result<Self>
    where
        F: Fn(&Header, Option<&Commit>) -> Entry,
    {
        let path = paths::list_path(folder);
        let mut entries = match read_entries(&path) {
            Ok(Some(entries)) => entries,
            Ok(None) | Err(_) => {
                let entries = rebuild(folder, &meta)?;
                rewrite(&path, &entries)?;
                entries
            }
        };
        sort(&mut entries);
        let file = open_append(&path)?;
        Ok(SessionList {
            path,
            file,
            entries,
        })
    }

    /// The file the list is kept in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records one session; the entry replaces what the list had for it.
    pub fn record(&mut self, entry: Entry) -> io::Result<()> {
        let mut payload = Vec::new();
        entry.encode(&mut payload);
        self.append(kind::LIST_ENTRY, &payload)?;
        match self
            .entries
            .iter_mut()
            .find(|known| known.session == entry.session)
        {
            Some(known) => *known = entry,
            None => self.entries.push(entry),
        }
        sort(&mut self.entries);
        Ok(())
    }

    /// Removes a session with a tombstone entry.
    pub fn remove(&mut self, session: &str) -> io::Result<()> {
        let mut payload = Vec::new();
        Encoder::new(&mut payload).str(FIELD_SESSION, session);
        self.append(kind::LIST_TOMBSTONE, &payload)?;
        self.entries.retain(|known| known.session != session);
        Ok(())
    }

    /// The session with the newest update.
    pub fn newest(&self) -> Option<&Entry> {
        self.entries.first()
    }

    /// The newest session of one profile.
    pub fn newest_of(&self, profile: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.profile == profile)
    }

    /// The session whose id starts with `prefix`.
    pub fn by_prefix(&self, prefix: &str) -> Lookup<'_> {
        let mut found: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.session.starts_with(prefix))
            .collect();
        match found.len() {
            0 => Lookup::None,
            1 => Lookup::One(found.remove(0)),
            _ => Lookup::Many(found),
        }
    }

    /// Every session, newest first.
    pub fn all(&self) -> &[Entry] {
        &self.entries
    }

    /// Appends one frame to the list file.
    fn append(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(frame::OVERHEAD + payload.len());
        frame::write(&mut buf, LIST_SEED, NO_SEQ, kind, payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        self.file.write_all(&buf)
    }
}

/// The first user message as a title: one line, at most [`TITLE_CHARS`].
pub fn title(text: &str) -> String {
    let mut cut = String::with_capacity(TITLE_CHARS);
    for (index, ch) in text.chars().enumerate() {
        if index == TITLE_CHARS {
            break;
        }
        cut.push(if ch.is_control() { ' ' } else { ch });
    }
    cut.trim_end().to_owned()
}

/// Reads the entries of a list file, or `None` when it has to be rebuilt.
fn read_entries(path: &Path) -> io::Result<Option<Vec<Entry>>> {
    let bytes = match read_list_file(path)? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let mut entries: Vec<Entry> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let frame = match frame::read(&bytes, at, LIST_SEED, NO_SEQ) {
            Ok(frame) => frame,
            Err(_) => return Ok(None),
        };
        at += frame::OVERHEAD + frame.payload.len();
        match frame.kind {
            kind::LIST_ENTRY => {
                let entry = match Entry::decode(&frame.payload) {
                    Ok(entry) => entry,
                    Err(_) => return Ok(None),
                };
                match entries
                    .iter_mut()
                    .find(|known| known.session == entry.session)
                {
                    Some(known) => *known = entry,
                    None => entries.push(entry),
                }
            }
            kind::LIST_TOMBSTONE => {
                let mut decoder = Decoder::new(&frame.payload);
                while let Some((field, value)) = match decoder.next_field() {
                    Ok(field) => field,
                    Err(_) => return Ok(None),
                } {
                    if field != FIELD_SESSION {
                        continue;
                    }
                    let session = match Text::new(match value.bytes() {
                        Some(bytes) => bytes.clone(),
                        None => return Ok(None),
                    }) {
                        Ok(text) => text,
                        Err(_) => return Ok(None),
                    };
                    entries.retain(|known| known.session != session.as_str());
                }
            }
            // A kind from a newer build: skip it.
            _ => {}
        }
    }
    Ok(Some(entries))
}

/// The bytes of a list file, or `None` when it is missing or too large.
fn read_list_file(path: &Path) -> io::Result<Option<Bytes>> {
    let mut file = match open_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_LIST_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(Bytes::from(bytes)))
}

/// Builds the list from the part headers of the folder.
fn rebuild<F>(folder: &Path, meta: &F) -> io::Result<Vec<Entry>>
where
    F: Fn(&Header, Option<&Commit>) -> Entry,
{
    let mut entries = Vec::new();
    for dir in paths::session_dirs(folder)? {
        if let Some(entry) = entry_from_parts(&dir, meta)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Reads the newest part of one session folder into an entry.
fn entry_from_parts<F>(dir: &Path, meta: &F) -> io::Result<Option<Entry>>
where
    F: Fn(&Header, Option<&Commit>) -> Entry,
{
    let mut newest: Option<(u64, u64)> = None;
    for id in paths::part_ids(dir)? {
        let path = paths::part_path(dir, id);
        let part = match Part::open(&path) {
            Ok(part) => part,
            // A part that cannot be read is not a session we can list.
            Err(_) => continue,
        };
        let created = part.header().created_ms;
        if newest.is_none_or(|(ms, known)| (created, id) > (ms, known)) {
            newest = Some((created, id));
        }
    }
    let (_, id) = match newest {
        Some(best) => best,
        None => return Ok(None),
    };
    let part = Part::open(&paths::part_path(dir, id))?;
    let commit = part.last_commit().map(|at| &at.commit);
    Ok(Some(meta(part.header(), commit)))
}

/// Writes the whole list again, replacing what the file had.
fn rewrite(path: &Path, entries: &[Entry]) -> io::Result<()> {
    let mut file = match create_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_parent(path)?;
            create_file(path)?
        }
        Err(error) => return Err(error),
    };
    let mut buf = Vec::new();
    for entry in entries {
        let mut payload = Vec::new();
        entry.encode(&mut payload);
        buf.clear();
        frame::write(&mut buf, LIST_SEED, NO_SEQ, kind::LIST_ENTRY, &payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        file.write_all(&buf)?;
    }
    file.flush()?;
    Ok(())
}

/// Puts the entries in newest-first order.
fn sort(entries: &mut [Entry]) {
    entries.sort_by(|left, right| {
        right
            .updated_ms
            .cmp(&left.updated_ms)
            .then_with(|| right.session.cmp(&left.session))
    });
}

fn codec_error(error: crate::codec::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn varint_of(value: &Value) -> io::Result<u64> {
    value
        .varint()
        .ok_or_else(|| invalid("list field of the wrong type"))
}

fn text_of(value: &Value) -> io::Result<Text> {
    let bytes = value
        .bytes()
        .ok_or_else(|| invalid("list field of the wrong type"))?;
    Text::new(bytes.clone()).map_err(|_| invalid("list field is not UTF-8"))
}

/// Opens the list file for reading.
#[expect(
    clippy::disallowed_types,
    reason = "the session list is one small cache file, read whole"
)]
fn open_read(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Opens the list file for appends.
#[expect(
    clippy::disallowed_types,
    reason = "the session list is one small cache file, appended to whole"
)]
fn open_append(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().append(true).open(path)
}

/// Creates the list file, replacing it.
#[expect(
    clippy::disallowed_types,
    reason = "the session list is one small cache file, rewritten whole"
)]
fn create_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// Makes the folder a list file lives in.
#[expect(
    clippy::disallowed_methods,
    reason = "the session folder is made once, when the list is rewritten"
)]
fn create_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) => std::fs::create_dir_all(dir),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::PartWriter;
    use crate::testutil;
    use tempfile::TempDir;

    fn entry(session: &str, updated_ms: u64, profile: &str) -> Entry {
        Entry {
            session: session.to_owned(),
            updated_ms,
            turns: 3,
            title: format!("title of {session}"),
            profile: profile.to_owned(),
            provider: "anthropic".to_owned(),
            model: "claude".to_owned(),
            part: updated_ms * 7,
            state: State::Done,
            headless: false,
        }
    }

    /// A part file of one session, with the entry as its meta.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes its own session folder"
    )]
    fn write_session(folder: &Path, entry: &Entry) {
        let dir = folder.join(&entry.session);
        std::fs::create_dir_all(&dir).expect("session folder");
        let mut meta = Vec::new();
        entry.encode(&mut meta);
        let header = Header {
            part_id: entry.part,
            session_id: entry.session.clone(),
            created_ms: entry.updated_ms,
            fork: None,
            meta: Bytes::from(meta),
        };
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header).expect("part");
        writer
            .append(10, &Bytes::from_static(b"a turn"), false)
            .expect("append");
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        testutil::write(&paths::part_path(&dir, entry.part), &out);
    }

    fn meta(header: &Header, _commit: Option<&Commit>) -> Entry {
        let mut entry = Entry::decode(&header.meta).expect("entry meta");
        entry.part = header.part_id;
        entry
    }

    #[test]
    fn the_newest_entry_is_the_last_update() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 100, "work")).expect("record");
        list.record(entry("01jbbb", 300, "work")).expect("record");
        list.record(entry("01jccc", 200, "home")).expect("record");
        assert_eq!(list.newest().expect("newest").session, "01jbbb");
        let order: Vec<&str> = list.all().iter().map(|e| e.session.as_str()).collect();
        assert_eq!(order, vec!["01jbbb", "01jccc", "01jaaa"]);

        list.record(entry("01jaaa", 400, "work")).expect("record");
        assert_eq!(list.newest().expect("newest").session, "01jaaa");
        assert_eq!(list.all().len(), 3, "the last entry per id wins");
        assert_eq!(list.all()[0].updated_ms, 400);
    }

    #[test]
    fn the_newest_entry_of_a_profile() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 500, "work")).expect("record");
        list.record(entry("01jbbb", 300, "work")).expect("record");
        list.record(entry("01jccc", 400, "home")).expect("record");
        let newest = list.newest_of("work").expect("a work session");
        assert_eq!(newest.session, "01jaaa");
        assert_eq!(list.newest_of("home").expect("home").session, "01jccc");
        assert!(list.newest_of("other").is_none());
    }

    #[test]
    fn a_prefix_is_unique_ambiguous_or_missing() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jabc", 100, "work")).expect("record");
        list.record(entry("01jabd", 200, "work")).expect("record");
        list.record(entry("01jxyz", 150, "work")).expect("record");

        match list.by_prefix("01jxyz") {
            Lookup::One(found) => assert_eq!(found.session, "01jxyz"),
            found => panic!("expected one, got {found:?}"),
        }
        match list.by_prefix("01jab") {
            Lookup::Many(found) => {
                let ids: Vec<&str> = found.iter().map(|e| e.session.as_str()).collect();
                assert_eq!(ids, vec!["01jabd", "01jabc"], "newest first");
            }
            found => panic!("expected many, got {found:?}"),
        }
        assert!(matches!(list.by_prefix("01jnope"), Lookup::None));
        assert!(matches!(list.by_prefix(""), Lookup::Many(_)));
    }

    #[test]
    fn a_tombstone_hides_a_session() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 100, "work")).expect("record");
        list.record(entry("01jbbb", 200, "work")).expect("record");
        list.remove("01jbbb").expect("remove");
        assert_eq!(list.newest().expect("newest").session, "01jaaa");
        assert!(list.newest_of("work").is_some());

        // The tombstone is on the disk too.
        let again = SessionList::open(dir.path(), meta).expect("reopen");
        assert_eq!(again.all().len(), 1, "a removed session stays removed");
        assert_eq!(again.all()[0].session, "01jaaa");
    }

    #[test]
    fn a_deleted_list_is_rebuilt_with_the_same_answers() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        for (session, updated, profile) in [
            ("01jaaa", 100, "work"),
            ("01jbbb", 300, "home"),
            ("01jccc", 200, "work"),
        ] {
            let entry = entry(session, updated, profile);
            write_session(dir.path(), &entry);
            list.record(entry).expect("record");
        }
        let before: Vec<String> = list.all().iter().map(|e| e.session.clone()).collect();

        remove_list_file(list.path());
        let rebuilt = SessionList::open(dir.path(), meta).expect("rebuild");
        let after: Vec<String> = rebuilt.all().iter().map(|e| e.session.clone()).collect();
        assert_eq!(after, before);
        assert_eq!(
            rebuilt.newest_of("work").map(|e| e.session.as_str()),
            Some("01jccc")
        );
        assert_eq!(
            rebuilt.all()[0].session,
            "01jbbb",
            "the rebuild sorts by update time"
        );
        assert_eq!(rebuilt.all()[0].title, "title of 01jbbb");
    }

    #[test]
    fn a_torn_last_entry_is_ignored() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        let first = entry("01jaaa", 100, "work");
        write_session(dir.path(), &first);
        list.record(first).expect("record");
        let path = list.path().to_path_buf();
        drop(list);

        // Cut the file in the middle of its only entry.
        let bytes = testutil::read(&path);
        let half = bytes[..bytes.len() - 4].to_vec();
        testutil::write(&path, &half);

        let list = SessionList::open(dir.path(), meta).expect("list");
        assert_eq!(list.all().len(), 1, "rebuilt from the part header");
        assert_eq!(list.newest().expect("newest").session, "01jaaa");
        assert_eq!(list.newest().expect("newest").title, "title of 01jaaa");
    }

    #[test]
    fn appends_keep_the_earlier_entries() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 100, "work")).expect("record");
        let len = testutil::len(list.path());
        list.record(entry("01jbbb", 200, "work")).expect("record");
        assert!(testutil::len(list.path()) > len, "one frame per entry");

        let again = SessionList::open(dir.path(), meta).expect("reopen");
        assert_eq!(again.all().len(), 2);
        assert_eq!(
            again.newest().expect("newest").session,
            "01jbbb",
            "an append is enough"
        );
    }

    #[test]
    fn headless_round_trips() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        let mut marked = entry("01jaaa", 100, "work");
        marked.headless = true;
        write_session(dir.path(), &marked);
        list.record(marked).expect("record");

        let again = SessionList::open(dir.path(), meta).expect("reopen");
        assert!(again.newest().expect("newest").headless);
        // A rebuild reads the part header, which carries the flag too.
        remove_list_file(again.path());
        let rebuilt = SessionList::open(dir.path(), meta).expect("rebuild");
        assert_eq!(rebuilt.all().len(), 1);
        assert!(rebuilt.newest().expect("newest").headless);
    }

    #[test]
    fn headless_absent_in_an_old_entry_reads_false() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 100, "work")).expect("record");
        let path = list.path().to_path_buf();
        drop(list);

        // An entry of a build before the flag writes no field 10 at all.
        let bytes = Bytes::from(testutil::read(&path));
        let frame = frame::read(&bytes, 0, LIST_SEED, NO_SEQ).expect("frame");
        let mut decoder = Decoder::new(&frame.payload);
        while let Some((field, _)) = decoder.next_field().expect("field") {
            assert_ne!(field, FIELD_HEADLESS, "an unmarked entry writes no flag");
        }

        let list = SessionList::open(dir.path(), meta).expect("reopen");
        assert!(
            !list.newest().expect("newest").headless,
            "an entry with no flag is not headless"
        );
    }

    #[test]
    fn a_title_is_one_line_of_sixty_characters() {
        assert_eq!(title("short"), "short");
        assert_eq!(title("two\nlines\there"), "two lines here");
        let long = "x".repeat(100);
        assert_eq!(title(&long).len(), TITLE_CHARS);
        let accented = "é".repeat(100);
        assert_eq!(title(&accented).chars().count(), TITLE_CHARS);
    }

    #[test]
    fn a_list_file_with_an_unknown_kind_still_reads() {
        let dir = TempDir::new().expect("temp dir");
        let mut list = SessionList::open(dir.path(), meta).expect("list");
        list.record(entry("01jaaa", 100, "work")).expect("record");
        let path = list.path().to_path_buf();
        drop(list);
        let mut bytes = testutil::read(&path);
        frame::write(&mut bytes, LIST_SEED, NO_SEQ, 200, b"from the future").expect("frame");
        testutil::write(&path, &bytes);
        let list = SessionList::open(dir.path(), meta).expect("list");
        assert_eq!(list.all().len(), 1);
    }

    #[test]
    fn a_list_file_over_the_cap_is_rebuilt() {
        let dir = TempDir::new().expect("temp dir");
        let entry = entry("01jaaa", 100, "work");
        write_session(dir.path(), &entry);
        let path = paths::list_path(dir.path());
        testutil::write(&path, &vec![0u8; MAX_LIST_BYTES as usize + 1]);
        let list = SessionList::open(dir.path(), meta).expect("rebuild");
        assert_eq!(list.all().len(), 1);
        assert!(
            testutil::len(&path) < MAX_LIST_BYTES,
            "the file is rewritten"
        );
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "a test removes its own fixture file"
    )]
    fn remove_list_file(path: &Path) {
        std::fs::remove_file(path).expect("remove the list");
    }
}

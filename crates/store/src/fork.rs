//! Reading a session: the chain of parts a fork joins, oldest part first.
//!
//! A part that forks names the part that owns the records before it, so a
//! chain is never longer than the real ancestry. Its length is capped at
//! [`MAX_CHAIN`] parts.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::part::{FIRST_SEQ, Header, invalid};

pub use crate::part::ForkPoint;
use crate::paths;
use crate::recover::{Part, PartRecords, Record};

/// Most parts one chain may have.
pub const MAX_CHAIN: usize = 64;

/// One part of a chain, with the window the fork gives it.
#[derive(Debug)]
struct Link {
    part: Part,
    /// File offset where this part's records stop.
    end: u64,
    /// Last record of this part a reader sees.
    limit: u64,
}

/// A session opened for reading.
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    /// The header of the newest part.
    header: Header,
    /// Oldest part first.
    chain: Vec<Link>,
    bytes_read: Arc<AtomicU64>,
}

impl Session {
    /// Opens the newest part of the session in `dir`, and its ancestors.
    ///
    /// Reading blocks: run it in `spawn_blocking`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let newest = newest_part(dir)?;
        Session::open_part(dir, newest)
    }

    /// Opens the chain that ends at `part_id`.
    pub fn open_part(dir: &Path, part_id: u64) -> io::Result<Self> {
        let bytes_read = Arc::new(AtomicU64::new(0));
        let mut chain = Vec::new();
        let mut current = part_id;
        let mut end = u64::MAX;
        let mut limit = u64::MAX;
        loop {
            if chain.len() >= MAX_CHAIN {
                return Err(invalid("session fork chain is too long"));
            }
            let path = find_part(dir, current)?;
            let part = Part::open_counted(&path, Arc::clone(&bytes_read))?;
            let fork = part.header().fork;
            chain.push(Link {
                end: part.end().min(end),
                limit: part.last_seq().min(limit),
                part,
            });
            match fork {
                Some(fork) => {
                    current = fork.part;
                    end = fork.offset;
                    limit = fork.seq;
                }
                None => break,
            }
        }
        chain.reverse();
        let header = chain
            .last()
            .map(|link| link.part.header().clone())
            .ok_or_else(|| invalid("session has no parts"))?;
        Ok(Session {
            dir: dir.to_path_buf(),
            header,
            chain,
            bytes_read,
        })
    }

    /// The folder this session was read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The header of the newest part.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// How many parts the chain has.
    pub fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// The newest base record of the chain: a resumed part writes none, so
    /// the base may be in a parent part, before the fork point.
    pub fn last_base(&self) -> Option<u64> {
        self.chain.iter().rev().find_map(|link| {
            let commit = link
                .part
                .commits()
                .iter()
                .rev()
                .find(|at| at.commit.last_seq <= link.limit)?
                .commit;
            commit
                .base
                .map(|(seq, _)| seq)
                .filter(|seq| *seq <= link.limit)
        })
    }

    /// Every base of the chain with the last record of the commit it starts:
    /// a base, the records it restates and their commit are written together.
    /// A later commit of the part names the same base, so only the commit that
    /// begins at the base says where the restatement ends.
    pub fn base_ends(&self) -> Vec<(u64, u64)> {
        let mut ends = Vec::new();
        for link in &self.chain {
            for at in link.part.commits() {
                let commit = &at.commit;
                if let Some((seq, _)) = commit.base
                    && commit.last_seq <= link.limit
                    && commit.last_seq + 1 == seq + commit.count
                {
                    ends.push((seq, commit.last_seq));
                }
            }
        }
        ends
    }

    /// Where a fork at `seq` reads its parent from.
    ///
    /// The fork names the part that owns the record, so a fork of a fork
    /// stays one link from the records. A `seq` past the log points at the
    /// end of the newest part.
    pub fn fork_point(&self, seq: u64) -> ForkPoint {
        for link in self.chain.iter().rev() {
            if seq < link.part.first_seq() {
                continue;
            }
            let at = link
                .part
                .commits()
                .iter()
                .find(|at| at.commit.last_seq >= seq);
            let offset = at.map_or_else(|| link.part.end(), |at| at.end);
            return ForkPoint {
                part: link.part.part_id(),
                seq,
                offset,
            };
        }
        let oldest = match self.chain.first() {
            Some(link) => link.part.header_end(),
            None => 0,
        };
        ForkPoint {
            part: self.chain.first().map_or(0, |link| link.part.part_id()),
            seq: 0,
            offset: oldest,
        }
    }

    /// The records from `from` on, oldest part first.
    ///
    /// When the newest commit of a part carries a base record at or before
    /// `from`, the read starts at that base instead of at the start of the
    /// part, which bounds what a resume takes from the disk.
    pub fn records_from(&self, from: u64) -> Records<'_> {
        Records {
            todo: self.chain.iter(),
            from,
            current: None,
            done: false,
        }
    }

    /// Every visible record of the session.
    pub fn records(&self) -> Records<'_> {
        self.records_from(FIRST_SEQ)
    }

    /// Bytes the readers of this session took from the disk.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }
}

/// The file of part `id`: in `dir`, or for a session forked from another
/// session, in a sibling session folder of the same cwd.
fn find_part(dir: &Path, id: u64) -> io::Result<std::path::PathBuf> {
    let own = paths::part_path(dir, id);
    if std::fs::exists(&own)? {
        return Ok(own);
    }
    if let Some(folder) = dir.parent() {
        for sibling in paths::session_dirs(folder)? {
            let path = paths::part_path(&sibling, id);
            if std::fs::exists(&path)? {
                return Ok(path);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("session part {} is missing", paths::part_file_name(id)),
    ))
}

/// The newest part of a session folder.
fn newest_part(dir: &Path) -> io::Result<u64> {
    let mut best: Option<(u64, u64)> = None;
    for id in paths::part_ids(dir)? {
        let path = paths::part_path(dir, id);
        // A run that died before its header was written leaves a part with
        // no header; the session list skips it the same way.
        let Ok(part) = Part::open(&path) else {
            continue;
        };
        let created = part.header().created_ms;
        if best.is_none_or(|(ms, known)| (created, id) > (ms, known)) {
            best = Some((created, id));
        }
    }
    best.map(|(_, id)| id)
        .ok_or_else(|| invalid("session folder has no parts"))
}

/// The records of a session, one part after the next.
#[derive(Debug)]
pub struct Records<'a> {
    /// Parts left to read, oldest first.
    todo: std::slice::Iter<'a, Link>,
    from: u64,
    current: Option<PartRecords>,
    /// Set by the first error: a reader stops there.
    done: bool,
}

impl Iterator for Records<'_> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if let Some(current) = self.current.as_mut() {
                match current.next() {
                    Some(record) => {
                        self.done = record.is_err();
                        return Some(record);
                    }
                    None => self.current = None,
                }
            }
            let link = self.todo.next()?;
            match link.part.records_between(self.from, link.limit, link.end) {
                Ok(records) => self.current = Some(records),
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::PartWriter;
    use crate::testutil;
    use bytes::Bytes;
    use tempfile::TempDir;

    fn header(part_id: u64, created_ms: u64, fork: Option<ForkPoint>) -> Header {
        Header {
            part_id,
            session_id: "01jsession".to_owned(),
            created_ms,
            fork,
            meta: Bytes::new(),
        }
    }

    /// Writes a part with these payloads, one commit per `per_commit` records.
    fn write_part(dir: &Path, header: Header, payloads: &[&'static str], per_commit: usize) {
        let path = paths::part_path(dir, header.part_id);
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header).expect("part");
        for group in payloads.chunks(per_commit.max(1)) {
            for payload in group {
                writer
                    .append(10, &Bytes::from_static(payload.as_bytes()), false)
                    .expect("append");
            }
            writer.commit().expect("commit");
        }
        writer.finish().expect("finish");
        testutil::write(&path, &out);
    }

    fn read(session: &Session, from: u64) -> Vec<Record> {
        session
            .records_from(from)
            .map(|record| record.expect("record"))
            .collect()
    }

    fn payloads(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .map(|record| String::from_utf8_lossy(&record.payload).into_owned())
            .collect()
    }

    #[test]
    fn records_come_back_across_the_chain() {
        let dir = TempDir::new().expect("temp dir");
        let parent = 0xaaaa;
        write_part(
            dir.path(),
            header(parent, 1, None),
            &["p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"],
            3,
        );
        let open_parent = Session::open_part(dir.path(), parent).expect("parent");
        let fork = open_parent.fork_point(5);
        assert_eq!(fork.part, parent);
        assert_eq!(fork.seq, 5);
        assert_eq!(open_parent.fork_point(0).seq, 0);

        let child = 0xbbbb;
        write_part(dir.path(), header(child, 2, Some(fork)), &["c6", "c7"], 2);

        let session = Session::open(dir.path()).expect("session");
        assert_eq!(session.header().part_id, child, "the newest part");
        assert_eq!(session.chain_len(), 2);
        let records = read(&session, 1);
        assert_eq!(
            payloads(&records),
            vec!["p1", "p2", "p3", "p4", "p5", "c6", "c7"]
        );
        let seqs: Vec<u64> = records.iter().map(|record| record.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_read_from_a_sequence_number_skips_what_came_before() {
        let dir = TempDir::new().expect("temp dir");
        let part = 1;
        write_part(dir.path(), header(part, 1, None), &["a", "b", "c", "d"], 2);
        let session = Session::open(dir.path()).expect("session");
        let records = read(&session, 3);
        assert_eq!(payloads(&records), vec!["c", "d"]);
        assert_eq!(records[0].seq, 3);
    }

    #[test]
    fn a_parent_stops_at_the_fork_point() {
        let dir = TempDir::new().expect("temp dir");
        let parent = 2;
        write_part(
            dir.path(),
            header(parent, 1, None),
            &["p1", "p2", "p3", "p4", "p5"],
            5,
        );
        let open_parent = Session::open_part(dir.path(), parent).expect("parent");
        let fork = open_parent.fork_point(2);
        write_part(dir.path(), header(3, 2, Some(fork)), &["c3", "c4", "c5"], 3);
        let session = Session::open_part(dir.path(), 3).expect("child");
        assert_eq!(
            payloads(&read(&session, 1)),
            vec!["p1", "p2", "c3", "c4", "c5"],
            "the parent's records after the fork are not replayed"
        );
    }

    #[test]
    fn a_fork_of_a_fork_points_straight_at_the_owner() {
        let dir = TempDir::new().expect("temp dir");
        let root = 10;
        write_part(
            dir.path(),
            header(root, 1, None),
            &["r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9"],
            3,
        );
        let open_root = Session::open_part(dir.path(), root).expect("root");
        write_part(
            dir.path(),
            header(20, 2, Some(open_root.fork_point(5))),
            &["b6", "b7"],
            2,
        );
        let open_branch = Session::open_part(dir.path(), 20).expect("branch");
        assert_eq!(open_branch.chain_len(), 2);

        let fork = open_branch.fork_point(3);
        assert_eq!(fork.part, root, "not the branch, which does not own r3");
        assert_eq!(fork.seq, 3);
        write_part(dir.path(), header(30, 3, Some(fork)), &["c4"], 1);

        let session = Session::open_part(dir.path(), 30).expect("fork of a fork");
        assert_eq!(session.chain_len(), 2, "the branch is skipped");
        assert_eq!(payloads(&read(&session, 1)), vec!["r1", "r2", "r3", "c4"]);
    }

    #[test]
    fn a_chain_of_64_parts_reads_and_65_is_an_error() {
        let dir = TempDir::new().expect("temp dir");
        let mut fork: Option<ForkPoint> = None;
        let mut ids = Vec::new();
        for index in 0..=MAX_CHAIN {
            let id = 1000 + index as u64;
            write_part(dir.path(), header(id, index as u64 + 1, fork), &["x"], 1);
            ids.push(id);
            if index < MAX_CHAIN {
                let open = Session::open_part(dir.path(), id).expect("part");
                assert_eq!(open.chain_len(), index + 1);
                fork = Some(open.fork_point(index as u64 + 1));
            }
        }
        let ok = Session::open_part(dir.path(), ids[MAX_CHAIN - 1]).expect("64 parts");
        assert_eq!(ok.chain_len(), MAX_CHAIN);
        let error =
            Session::open_part(dir.path(), ids[MAX_CHAIN]).expect_err("a chain over the cap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn last_base_is_read_from_the_newest_commit() {
        let dir = TempDir::new().expect("temp dir");
        let part = 5;
        let path = paths::part_path(dir.path(), part);
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header(part, 1, None)).expect("part");
        for index in 0..5 {
            writer
                .append(10, &Bytes::from_static(b"record"), index == 2)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        testutil::write(&path, &out);

        let session = Session::open(dir.path()).expect("session");
        assert_eq!(session.last_base(), Some(3), "the third record");
        assert_eq!(read(&session, 3).len(), 3, "a read starts at the base");
    }

    #[test]
    fn open_picks_the_newest_part() {
        let dir = TempDir::new().expect("temp dir");
        write_part(dir.path(), header(1, 1, None), &["old"], 1);
        write_part(dir.path(), header(2, 2, None), &["new"], 1);
        let session = Session::open(dir.path()).expect("session");
        assert_eq!(session.header().part_id, 2);
        assert_eq!(payloads(&read(&session, 1)), vec!["new"]);
        assert_eq!(session.dir(), dir.path());
    }

    #[test]
    fn an_empty_folder_is_an_error() {
        let dir = TempDir::new().expect("temp dir");
        let error = Session::open(dir.path()).expect_err("no parts");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_footer_part_reads_its_records_from_the_open_read() {
        let dir = TempDir::new().expect("temp dir");
        write_part(
            dir.path(),
            header(9, 1, None),
            &["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc"],
            3,
        );
        let session = Session::open(dir.path()).expect("session");
        let after_open = session.bytes_read();
        assert!(after_open > 0);
        assert_eq!(read(&session, 1).len(), 3);
        assert_eq!(session.bytes_read(), after_open, "no second read");
    }

    #[test]
    fn bytes_read_climbs_while_the_records_come_back() {
        let dir = TempDir::new().expect("temp dir");
        write_part(
            dir.path(),
            header(9, 1, None),
            &["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc"],
            3,
        );
        // Without its footer (a crash before the close) the part is scanned,
        // and the records are read from the file as they come back.
        let path = paths::part_path(dir.path(), 9);
        let bytes = testutil::read(&path);
        let trailer = &bytes[bytes.len() - crate::part::TRAILER_LEN..];
        let footer = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) as usize;
        testutil::write(
            &path,
            &bytes[..bytes.len() - crate::part::TRAILER_LEN - footer],
        );
        let session = Session::open(dir.path()).expect("session");
        let after_open = session.bytes_read();
        assert!(after_open > 0);
        let records = read(&session, 1);
        assert_eq!(records.len(), 3);
        assert!(session.bytes_read() > after_open);
    }

    #[test]
    fn a_part_without_a_header_is_skipped() {
        let dir = TempDir::new().expect("temp dir");
        write_part(dir.path(), header(1, 10, None), &["a", "b"], 2);
        testutil::write(&paths::part_path(dir.path(), 2), b"");
        let session = Session::open(dir.path()).expect("session");
        assert_eq!(session.header().part_id, 1);
        assert_eq!(session.records().count(), 2);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes its fixture folders"
    )]
    fn a_fork_into_another_session_reads_its_parent() {
        let folder = TempDir::new().expect("temp dir");
        let first = folder.path().join("01first");
        let second = folder.path().join("01second");
        std::fs::create_dir(&first).expect("first folder");
        std::fs::create_dir(&second).expect("second folder");
        write_part(&first, header(1, 10, None), &["a", "b", "c"], 1);
        let parent = Session::open(&first).expect("parent");
        let fork = parent.fork_point(2);
        write_part(&second, header(2, 20, Some(fork)), &["x"], 1);
        let child = Session::open(&second).expect("child");
        let payloads: Vec<Bytes> = child
            .records()
            .map(|record| record.expect("record").payload)
            .collect();
        assert_eq!(
            payloads,
            vec![Bytes::from("a"), Bytes::from("b"), Bytes::from("x")]
        );
        assert_eq!(
            parent.records().count(),
            3,
            "the parent session is unchanged"
        );
    }

    #[test]
    fn the_last_base_of_a_parent_part_is_found() {
        let dir = TempDir::new().expect("temp dir");
        let path = paths::part_path(dir.path(), 1);
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header(1, 10, None)).expect("part");
        writer
            .append(10, &Bytes::from_static(b"a"), false)
            .expect("append");
        writer.commit().expect("commit");
        writer
            .append(10, &Bytes::from_static(b"b"), true)
            .expect("base");
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        testutil::write(&path, &out);
        let parent = Session::open(dir.path()).expect("parent");
        assert_eq!(parent.last_base(), Some(2));
        let fork = parent.fork_point(2);
        write_part(dir.path(), header(2, 20, Some(fork)), &["c"], 1);
        let child = Session::open(dir.path()).expect("child");
        assert_eq!(child.header().part_id, 2);
        assert_eq!(child.last_base(), Some(2), "the base in the parent part");
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes its fixture folders"
    )]
    fn base_ends_reach_into_a_parent_session() {
        let folder = TempDir::new().expect("temp dir");
        let first = folder.path().join("01first");
        let second = folder.path().join("01second");
        std::fs::create_dir(&first).expect("first folder");
        std::fs::create_dir(&second).expect("second folder");
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header(1, 10, None)).expect("part");
        writer
            .append(10, &Bytes::from_static(b"a"), false)
            .expect("append");
        writer.commit().expect("commit");
        // A base and the two records it restates, committed together.
        writer
            .append(11, &Bytes::from_static(b"base"), true)
            .expect("base");
        for payload in [b"a" as &[u8], b"b"] {
            writer
                .append(10, &Bytes::copy_from_slice(payload), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer
            .append(10, &Bytes::from_static(b"c"), false)
            .expect("append");
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        testutil::write(&paths::part_path(&first, 1), &out);
        let parent = Session::open(&first).expect("parent");
        assert_eq!(
            parent.base_ends(),
            vec![(2, 4)],
            "the later commit is no base"
        );
        let fork = parent.fork_point(5);
        write_part(&second, header(2, 20, Some(fork)), &["x"], 1);
        let child = Session::open(&second).expect("child");
        assert_eq!(
            child.base_ends(),
            vec![(2, 4)],
            "the base in the parent session"
        );
    }
}

//! The journal: the record log one run appends to.
//!
//! A turn appends each message, a `CALL_START` before a tool that is not a
//! read runs, a `TURN_END` at each turn end, and a `BASE` at the start and
//! after each compaction. The store frames, compresses and syncs the records;
//! this module only encodes them.

use std::io;
use std::path::Path;

use bytes::Bytes;
use reagent_ai::{Message, Usage};
use reagent_store::{Header, Writer};

use crate::journal::record::{
    Approval, ApprovalAnswer, ApprovalKind, Given, ParkedAsk, Question, Settings, TurnEnd,
};
use crate::journal::state::State;
use crate::tool::EffectClass;

pub mod kinds;
pub mod record;
pub mod registry;
pub mod resume;
pub mod state;

#[cfg(test)]
mod write;

/// The most bytes the encoder buffer keeps once a record is handed over.
///
/// A record bigger than this, such as a large tool result, drops the buffer
/// instead of keeping megabytes for the next small message.
pub const MAX_BUFFER: usize = 64 * 1024;

/// The durable log of one run, written on the store's writer thread.
#[derive(Debug)]
pub struct Journal {
    writer: Writer,
    /// The encoder's output, reused by the next record.
    scratch: Vec<u8>,
    /// History messages a parent part already holds: a resumed run writes
    /// only the messages after them and no base at its start.
    recorded: Option<usize>,
    /// The number of the record this journal appended last: the part numbers
    /// its records from 1, and a part that continues another starts after the
    /// last record of the chain it forks from.
    records: u64,
}

impl Journal {
    /// Creates the part file in `dir` and starts the thread that writes it.
    ///
    /// Writing blocks on the file, so a caller runs this in `spawn_blocking`.
    pub fn create(dir: &Path, header: Header) -> io::Result<Self> {
        let records = header.fork.map_or(0, |fork| fork.seq);
        Ok(Self {
            writer: Writer::create(dir, header)?,
            scratch: Vec::new(),
            recorded: None,
            records,
        })
    }

    /// Creates the part of a resumed run whose parent part already holds the
    /// first `recorded` messages of the history it continues with.
    pub fn resumed(dir: &Path, header: Header, recorded: usize) -> io::Result<Self> {
        let mut journal = Self::create(dir, header)?;
        journal.recorded = Some(recorded);
        Ok(journal)
    }

    /// How many history messages a parent part already holds, for a resumed run.
    pub fn recorded(&self) -> Option<usize> {
        self.recorded
    }

    /// The part id of the file this journal writes.
    pub fn part_id(&self) -> u64 {
        self.writer.part_id()
    }

    /// Appends one message of the conversation.
    pub fn message(&mut self, message: &Message) -> io::Result<()> {
        self.scratch.clear();
        record::encode_message(&mut self.scratch, message);
        self.append(kinds::MESSAGE, false)
    }

    /// Appends the start of a tool call that is about to run.
    pub fn call_start(&mut self, id: &str, name: &str, class: EffectClass) -> io::Result<()> {
        self.scratch.clear();
        record::encode_call_start(&mut self.scratch, id, name, class);
        self.append(kinds::CALL_START, false)
    }

    /// Appends the end of a turn and the usage it spent.
    pub fn turn_end(&mut self, turn: u64, usage: Usage) -> io::Result<()> {
        self.scratch.clear();
        record::encode_turn_end(&mut self.scratch, &TurnEnd { turn, usage });
        self.append(kinds::TURN_END, false)
    }

    /// Appends a base: the agent state at a point a resume may start from.
    pub fn base(&mut self, state: &State) -> io::Result<()> {
        self.scratch.clear();
        record::encode_base(&mut self.scratch, state);
        self.append(kinds::BASE, true)
    }

    /// Appends a change of provider and model.
    pub fn model(&mut self, provider: &str, model: &str) -> io::Result<()> {
        self.model_config(provider, model, None)
    }

    /// Appends a change of provider and model, with the config provider id the
    /// switch used when one is known.
    pub fn model_config(
        &mut self,
        provider: &str,
        model: &str,
        config: Option<&str>,
    ) -> io::Result<()> {
        self.scratch.clear();
        record::encode_model(&mut self.scratch, provider, model, config);
        self.append(kinds::MODEL, false)
    }

    /// Appends the wait of a call for a client's answer, or the answer it got.
    ///
    /// The wait is on the disk before the call asks, so a resume knows a call
    /// without an answer never ran, whatever its call start says.
    pub fn approval(&mut self, id: &str, kind: ApprovalKind) -> io::Result<()> {
        self.scratch.clear();
        record::encode_approval(
            &mut self.scratch,
            &Approval {
                id: id.to_owned(),
                kind,
                parked: None,
            },
        );
        self.append(kinds::APPROVAL, false)
    }

    /// Appends the wait of a call for a client's answer that a later process
    /// may answer under `parked.qid`, with what that process shows of it.
    pub fn approval_parkable(&mut self, id: &str, parked: ParkedAsk) -> io::Result<()> {
        self.scratch.clear();
        record::encode_approval(
            &mut self.scratch,
            &Approval {
                id: id.to_owned(),
                kind: ApprovalKind::Wait,
                parked: Some(parked),
            },
        );
        self.append(kinds::APPROVAL, false)
    }

    /// Appends the answer a later process gave to the parked wait `qid`.
    pub fn approval_answered(
        &mut self,
        qid: &str,
        by: &str,
        answer: Given,
        reason: Option<&str>,
    ) -> io::Result<()> {
        self.scratch.clear();
        record::encode_approval_answer(
            &mut self.scratch,
            &ApprovalAnswer {
                qid: qid.to_owned(),
                by: by.to_owned(),
                answer,
                reason: reason.map(str::to_owned),
            },
        );
        self.append(kinds::APPROVAL, false)
    }

    /// Appends the wait of an `ask` call for a person's answer.
    ///
    /// The question is on the disk before it goes out, so a process that ends
    /// while it waits leaves a question a later process answers.
    pub fn question_asked(&mut self, call: &str, qid: &str, questions: &str) -> io::Result<()> {
        self.scratch.clear();
        record::encode_question(
            &mut self.scratch,
            &Question::Asked {
                call: call.to_owned(),
                qid: qid.to_owned(),
                questions: questions.to_owned(),
            },
        );
        self.append(kinds::QUESTION, false)
    }

    /// Appends the answer a person gave to question `qid`, as the JSON text of
    /// one list per question; `hop` is 0 for the owner, or the forward whose
    /// person answered.
    pub fn question_answered(
        &mut self,
        qid: &str,
        by: &str,
        answers: &str,
        hop: u64,
    ) -> io::Result<()> {
        self.scratch.clear();
        record::encode_question(
            &mut self.scratch,
            &Question::Answered {
                qid: qid.to_owned(),
                by: by.to_owned(),
                answers: answers.to_owned(),
                hop,
            },
        );
        self.append(kinds::QUESTION, false)
    }

    /// Appends a forward of a parked question, a [`Question::Forwarded`].
    pub fn question_forwarded(&mut self, forward: &Question) -> io::Result<()> {
        self.scratch.clear();
        record::encode_question(&mut self.scratch, forward);
        self.append(record::question_kind(forward), false)
    }

    /// Appends a change of the reasoning level, the permission mode, or both.
    /// A change of the model is a [`Journal::model`] record.
    pub fn settings(&mut self, level: Option<&str>, mode: Option<&str>) -> io::Result<()> {
        self.scratch.clear();
        record::encode_settings(
            &mut self.scratch,
            &Settings {
                level: level.map(str::to_owned),
                mode: mode.map(str::to_owned),
            },
        );
        self.append(kinds::SETTINGS, false)
    }

    /// The number of the record this journal appended last, or of the last
    /// record of the chain it continues before its first append.
    pub fn record(&self) -> u64 {
        self.records
    }

    /// The bytes the encoder buffer keeps for the next record.
    pub fn buffered_bytes(&self) -> usize {
        self.scratch.capacity()
    }

    /// Hands the records written so far to the writer thread to commit.
    pub fn commit(&mut self) -> io::Result<()> {
        self.writer.commit()
    }

    /// Waits until the writer thread has written every commit asked for so far,
    /// with no sync: a reader of the part finds them and a killed process keeps
    /// them.
    pub async fn written(&self) -> io::Result<()> {
        match self.writer.written().await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the journal writer stopped before the write",
            )),
        }
    }

    /// Commits and waits until the records are on the disk.
    pub async fn sync(&mut self) -> io::Result<()> {
        match self.writer.commit_and_sync().await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the journal writer stopped before the sync",
            )),
        }
    }

    /// Commits, syncs, unlocks the part file and stops the writer thread.
    pub fn close(self) -> io::Result<()> {
        self.writer.close()
    }

    /// Takes one encoded record and hands it to the writer thread.
    fn append(&mut self, kind: u8, base: bool) -> io::Result<()> {
        let payload = Bytes::copy_from_slice(&self.scratch);
        if self.scratch.capacity() > MAX_BUFFER {
            self.scratch = Vec::new();
        }
        self.writer.append(kind, payload, base)?;
        self.records += 1;
        Ok(())
    }
}

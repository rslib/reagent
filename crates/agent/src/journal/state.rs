//! The agent state a [`super::kinds::BASE`] record carries.
//!
//! The journal writes a base at the start of a run and after each compaction,
//! so a resume rebuilds this state without replaying the changes before it.

/// The state of a run at a base record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// The provider the run is using.
    pub provider: String,
    /// The model the run is using.
    pub model: String,
    /// How many compactions replaced the history so far.
    pub compaction: u32,
    /// The reread ledger's entries, so a read is noted as a repeat.
    pub ledger: Vec<LedgerEntry>,
    /// The files a tool wrote, relative to the working folder.
    pub touched: Vec<String>,
}

impl State {
    /// The state of a run with no ledger entry and no touched file yet.
    pub fn new(provider: impl Into<String>, model: impl Into<String>, compaction: u32) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            compaction,
            ledger: Vec::new(),
            touched: Vec::new(),
        }
    }
}

/// One entry of the reread ledger a base record carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The path as the model wrote it.
    pub path: String,
    /// The first line the read showed, 1-based.
    pub start: u64,
    /// The last line the read showed, 1-based.
    pub end: u64,
    /// The file's size at the read.
    pub size: u64,
    /// The file's modified time in milliseconds since the epoch.
    pub modified_ms: Option<u64>,
    /// How many times the same lines were shown again.
    pub repeats: u32,
}

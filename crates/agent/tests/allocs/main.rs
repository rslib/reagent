//! Heap block counts of the agent loop and the journal, built as one program.

use std::sync::Mutex;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Held by every test of every module for its whole body, since dhat allows
/// only one profiler per process and another test's allocations would land in
/// these numbers.
static SERIAL: Mutex<()> = Mutex::new(());

mod agent_loop;
mod journal;

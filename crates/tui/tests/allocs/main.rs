//! Heap block counts of the TUI, one module per area, built as one program.

use std::sync::Mutex;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Held by every test of every module for its whole body, since dhat allows
/// only one profiler per process and another test's allocations would land in
/// these numbers.
static SERIAL: Mutex<()> = Mutex::new(());

mod decode;
mod frame;
mod highlight;
mod markdown;
mod readline;

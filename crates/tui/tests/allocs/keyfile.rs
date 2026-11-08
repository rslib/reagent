//! Heap blocks of one key press through the keys file.
//!
//! A translation is a linear scan of the tables and of the rows the file moved,
//! so it takes no heap block whether the file moved the key or not: the chain of
//! sections, the keys and the default key an action's own key reads as are all
//! borrowed or copied.
//!
//! dhat allows one profiler per process, so the test locks [`SERIAL`] for its
//! whole body.

use std::sync::{MutexGuard, PoisonError};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dhat::{HeapStats, Profiler};
use reagent_tui::keyfile::{KEYS_CHAIN, KeyFile, LEADER_CHAIN, SHEET_CHAIN};

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many times a measurement runs. dhat counts the blocks of every thread of
/// the process, so one sample can carry a block that is not the work's; the
/// smallest of several is the work's own count.
const SAMPLES: usize = 3;

/// How many key presses one measurement translates.
const KEYS: usize = 100;

/// The smallest number of blocks `body` allocates over [`SAMPLES`] runs.
fn blocks(mut body: impl FnMut()) -> u64 {
    (0..SAMPLES)
        .map(|_| {
            let profiler = Profiler::builder().testing().build();
            body();
            let stats = HeapStats::get();
            drop(profiler);
            stats.total_blocks
        })
        .min()
        .expect("at least one sample")
}

/// [`KEYS`] presses of `name` read through `file`, the chain of the app's own.
fn translate(file: &KeyFile, name: KeyCode) {
    for _ in 0..KEYS {
        let event = KeyEvent::new(name, KeyModifiers::CONTROL);
        for chain in [KEYS_CHAIN, LEADER_CHAIN, SHEET_CHAIN] {
            let _ = file.translate(chain, event);
        }
    }
}

#[test]
fn keyfile_translate_allocates_no_block() {
    let _serial = serial();
    let (moved, notes) = KeyFile::parse("[keys]\nsend = \"ctrl+s\"\nsearch-forward = \"alt+s\"\n");
    assert!(notes.is_empty(), "{notes:?}");
    let (none, notes) = KeyFile::parse("");
    assert!(notes.is_empty(), "{notes:?}");

    let with = blocks(|| translate(&moved, KeyCode::Char('s')));
    eprintln!("blocks: {with} for {KEYS} presses through a moved key");
    assert_eq!(with, 0, "{with} blocks for {KEYS} presses of a moved key");

    let without = blocks(|| translate(&none, KeyCode::Char('s')));
    eprintln!("blocks: {without} for {KEYS} presses through the table");
    assert_eq!(
        without, 0,
        "{without} blocks for {KEYS} presses of a key no file moved"
    );
}

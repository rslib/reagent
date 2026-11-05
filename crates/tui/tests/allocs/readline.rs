//! Allocation counts for the keys of the text box that erase and kill.
//!
//! A typed character allocates nothing once the gap buffer and the undo log
//! have grown, and so does the erase that follows it: the log drops its oldest
//! step and the text that step took becomes the buffer the new step writes
//! into, so a run of [`KEYS`] erased characters costs no heap block. A kill
//! costs its ring entry, and a run of kills rebuilds the entry it joins rather
//! than copying the text before it.
//!
//! dhat allows one profiler per process, so every test locks [`SERIAL`] for its
//! whole body: a test that ran beside a profiler would be counted in its
//! numbers.

use std::sync::{MutexGuard, PoisonError};

use dhat::{HeapStats, Profiler};
use reagent_tui::editor::Editor;

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many times a measurement runs. dhat counts the blocks of every thread of
/// the process, so one sample can carry a block that is not the work's; the
/// smallest of several is the work's own count.
const SAMPLES: usize = 3;

/// The characters typed, and then erased one key at a time.
const KEYS: usize = 200;

/// The words killed, one Ctrl+W each.
const KILLS: usize = 20;

/// The blocks a warm editor spends on [`KEYS`] typed characters, the same count
/// of Backspaces and Deletes, and [`KILLS`] kills. The erases cost none: the
/// blocks are the two a kill spends on its ring entry and the text it is built
/// from.
const BLOCKS: u64 = 81;

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

/// [`KEYS`] typed characters, half of them erased with Backspace and half with
/// Delete, and then [`KILLS`] words killed one after another.
fn keys(editor: &mut Editor) {
    for _ in 0..KEYS {
        editor.type_char('a');
    }
    for _ in 0..KEYS / 2 {
        editor.backward_delete_char();
    }
    editor.line_start();
    for _ in 0..KEYS / 2 {
        editor.delete_char();
    }
    for _ in 0..KILLS {
        for c in ['a', 'b', ' '] {
            editor.type_char(c);
        }
        editor.unix_word_rubout();
    }
}

#[test]
fn readline_erase_keys_keep_their_blocks() {
    let _serial = serial();
    let mut editor = Editor::new();
    // A first round fills the undo log, its steps and the buffer the next step
    // writes into, which is what makes the keys after it cost no heap block.
    keys(&mut editor);
    let measured = blocks(|| keys(&mut editor));
    eprintln!(
        "blocks: {measured} for {KEYS} typed characters, {KEYS} erased ones and {KILLS} kills"
    );
    assert_eq!(
        measured, BLOCKS,
        "{measured} blocks for {KEYS} typed characters, {KEYS} erased ones and {KILLS} kills, \
         over {BLOCKS}"
    );
}

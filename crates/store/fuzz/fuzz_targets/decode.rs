//! Feeds arbitrary bytes to the field decoder and the frame reader.
//!
//! Run with `cargo +nightly fuzz run decode` from `crates/store/fuzz`.

#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use reagent_store::codec::Decoder;
use reagent_store::frame;

/// The part id and sequence number the frame reader checks against.
const PART: u64 = 0x0123_4567_89ab_cdef;
const SEQ: u64 = 1;

fuzz_target!(|data: &[u8]| {
    let bytes = Bytes::copy_from_slice(data);
    let mut decoder = Decoder::new(&bytes);
    while let Ok(Some(_)) = decoder.next_field() {}

    let mut at = 0;
    while let Ok(found) = frame::read(&bytes, at, PART, SEQ) {
        at += frame::OVERHEAD + found.payload.len();
    }
});

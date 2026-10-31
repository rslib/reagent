//! Allocation counts for the fan-out and for one client's writer.
//!
//! Fanning one frame out to three clients must clone the `Arc` and nothing
//! else, so 1,000 sends in steady state cost no block. The writer drains its
//! queue into a reused `Vec` and a reused `IoSlice` array, so one batch costs
//! no block of its own either.
//!
//! dhat allows one profiler at a time, so every measured case takes the shared
//! lock below as its first line: only the measured call runs inside the
//! profiler.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use dhat::{Alloc, HeapStats, Profiler};
use reagent_protocol::hub::{self, Frame, Hub, Kind};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Serializes the profiled sections so two profilers never overlap.
static MEASURE: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MEASURE.lock().unwrap_or_else(|error| error.into_inner())
}

/// The clients one measured fan-out serves.
const CLIENTS: u32 = 3;

/// The sends one measured fan-out makes.
const SENDS: usize = 1000;

/// The sends that warm the catch-up list, which keeps its room after a commit.
const WARM: usize = 2048;

/// The frames one measured writer run drains, under the queue's capacity so
/// no client is cut.
const FRAMES: usize = 768;

/// The most blocks one fan-out of a frame to every client may allocate: the
/// clone of the `Arc` costs none.
const BLOCKS_PER_FRAME: f64 = 0.0;

/// One encoded frame, shared by every client that gets it.
fn frame() -> Frame {
    Arc::from(
        &br#"{"jsonrpc":"2.0","method":"event","params":{"type":"text_delta","text":"x"}}"#[..],
    )
}

/// Takes every buffer in one call and keeps nothing.
#[derive(Debug, Default)]
struct Null;

impl AsyncWrite for Null {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

/// A hub with `CLIENTS` attached clients, and their queues.
fn attached() -> (Hub, Vec<mpsc::Receiver<hub::Entry>>) {
    let mut hub = Hub::new();
    let mut queues = Vec::new();
    for id in 1..=CLIENTS {
        let (queue, frames) = hub::channel();
        let (closing, _cut) = oneshot::channel();
        hub.join(id, Arc::from("client"), queue, closing);
        hub.attach(id, Some(u64::from(id)));
        queues.push(frames);
    }
    drain(&mut queues);
    (hub, queues)
}

/// Reads every frame a queue holds now.
fn drain(queues: &mut [mpsc::Receiver<hub::Entry>]) {
    for queue in queues {
        while queue.try_recv().is_ok() {}
    }
}

#[test]
fn hub_fan_out_to_three_clients_allocates_nothing() {
    let _guard = lock();
    let (mut hub, mut queues) = attached();
    let frame = frame();
    // The list of frames since the last commit keeps the room it grew, so the
    // measured run only pushes.
    for _ in 0..WARM {
        hub.send(Arc::clone(&frame), Kind::Delta);
        drain(&mut queues);
    }
    hub.send(Arc::clone(&frame), Kind::Committed);
    drain(&mut queues);

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    for _ in 0..SENDS {
        hub.send(Arc::clone(&frame), Kind::Delta);
        drain(&mut queues);
    }
    let stats = HeapStats::get();
    drop(profiler);

    let per_send = stats.total_blocks as f64 / SENDS as f64;
    eprintln!(
        "hub fan-out: {} blocks for {SENDS} frames to {CLIENTS} clients = {per_send:.3} per frame, {} bytes",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        per_send <= BLOCKS_PER_FRAME,
        "{} blocks for {SENDS} frames to {CLIENTS} clients = {per_send:.3} per frame, over {BLOCKS_PER_FRAME}: a frame was copied",
        stats.total_blocks
    );
}

/// A queue nobody writes to, and the signal of a client nobody cuts.
fn empty() -> (mpsc::Receiver<hub::Entry>, oneshot::Receiver<()>) {
    let (queue, frames) = hub::channel();
    drop(queue);
    (frames, oneshot::channel().1)
}

#[test]
fn hub_writer_allocates_nothing_per_batch() {
    let _guard = lock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a runtime");
    let mut hub = Hub::new();
    let (queue, frames) = hub::channel();
    let (closing, cut) = oneshot::channel();
    hub.join(1, Arc::from("client"), queue, closing);
    hub.attach(1, Some(1));
    let frame = frame();
    // The queue is filled and closed before the writer starts, outside the
    // measured part.
    let mut queue = frames;
    while queue.try_recv().is_ok() {}
    for _ in 0..FRAMES {
        hub.send(Arc::clone(&frame), Kind::Delta);
    }
    drop(hub);
    // The first block_on of a runtime sets up its own buffers, outside the
    // measured part.
    runtime.block_on(std::future::ready(()));

    // A writer with nothing to write costs what it keeps for its whole life.
    let (idle_queue, idle_cut) = empty();
    let profiler = Profiler::builder().testing().build();
    runtime.block_on(hub::write(Null, idle_queue, idle_cut));
    let idle = HeapStats::get();
    drop(profiler);

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    runtime.block_on(hub::write(Null, queue, cut));
    let stats = HeapStats::get();
    drop(profiler);

    eprintln!(
        "hub writer: {} blocks for {FRAMES} frames in {} batches, {} for none, {} bytes",
        stats.total_blocks,
        FRAMES.div_ceil(hub::IOV),
        idle.total_blocks,
        stats.total_bytes
    );
    assert!(
        stats.total_blocks <= idle.total_blocks,
        "{} blocks for {FRAMES} frames, over the {} of a writer with none: a batch allocated",
        stats.total_blocks,
        idle.total_blocks
    );
}

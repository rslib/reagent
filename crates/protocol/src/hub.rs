//! The frames of one session, encoded once for every client that gets them.
//!
//! A worker encodes each event once into one [`Frame`] and tells the hub its
//! [`Kind`]; the hub clones the `Arc` into every attached client's queue, so a
//! fan-out allocates nothing. A client's writer drains its queue into vectored
//! writes of [`IOV`] frames and, while deltas stream, waits at most
//! [`DELTA_WAIT`] for more of them, so a long answer costs one write per window
//! instead of one per delta. It keeps taking frames while its socket takes
//! nothing, so a slow socket does not fill the queue of a client that reads.
//!
//! A client whose queue is full is cut: the hub stops queueing to it, its
//! writer names the reason `behind` and closes its half of the connection, and
//! the clients that stay hear that it left.

use std::io::IoSlice;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::message;
use crate::message::AttachResult;
use crate::message::methods;

/// How many frames one client's queue holds before the hub cuts it.
pub const CAPACITY: usize = 4096;

/// How many frames one batch holds: the queue's own room, so a burst moves
/// from the queue into the batch instead of filling it.
pub const BATCH: usize = 1024;

/// How many frames one `write_vectored` call carries: below the `IOV_MAX` of
/// every platform, and under the 16 KiB of stack one frame of an async
/// function may take.
pub const IOV: usize = 256;

/// How long a writer waits for more deltas before it writes.
pub const DELTA_WAIT: Duration = Duration::from_millis(16);

/// How many pending bytes of deltas end that wait early.
pub const DELTA_BYTES: usize = 64 * 1024;

/// How long a cut client has to take the `closing` notice.
pub const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// The client id a worker hands out.
pub type ClientId = u32;

/// One frame: one encoded line, shared by every client that gets it.
pub type Frame = Arc<[u8]>;

/// What a frame carries, so the hub and the writer never parse its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A piece of a streamed answer.
    Delta,
    /// The journal committed a record, which ends what a client catches up on.
    Committed,
    /// The frame of the approval request that waits, by id.
    ApprovalRequest(u64),
    /// The frame of the answer that resolved the approval of this id.
    ApprovalResolved(u64),
    /// The frame of the question request that waits, by id.
    QuestionRequest(u64),
    /// The frame of the answer that resolved the question of this id, or the
    /// one of a question no client answered.
    QuestionResolved(u64),
    /// The end of a run. An approval it cancelled gets no answer, so the end
    /// takes every waiting request out of the catch-up.
    RunEnd,
    /// Anything else, written as soon as it is queued.
    Other,
}

/// A request that waits for a person. The two kinds are told apart, so that an
/// approval and a question of the same id do not resolve each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waits {
    Approval(u64),
    Question(u64),
}

/// One entry of a client's queue: the frame, and whether it is a delta.
#[derive(Debug)]
pub struct Entry {
    frame: Frame,
    delta: bool,
}

impl Entry {
    /// The bytes this entry writes.
    fn bytes(&self) -> usize {
        self.frame.len()
    }

    /// Whether this entry may wait for more deltas.
    fn is_delta(&self) -> bool {
        self.delta
    }
}

/// The queue of one client, and the writer's side of it.
pub fn channel() -> (mpsc::Sender<Entry>, mpsc::Receiver<Entry>) {
    mpsc::channel(CAPACITY)
}

/// The notice of a client that attached.
#[derive(Serialize)]
struct Joined<'a> {
    client: ClientId,
    name: &'a str,
}

/// The notice of a client that left.
#[derive(Serialize)]
struct Left {
    client: ClientId,
}

/// The frames of one session, and the clients that receive them.
///
/// One event is encoded once, into one frame, and every client gets a clone of
/// that frame.
#[derive(Debug, Default)]
pub struct Hub {
    clients: Vec<Client>,
    /// The frames since the last committed record, so that a client catches up
    /// from the record it read in the journal.
    uncommitted: Vec<Frame>,
    /// The frames of the requests that wait for a person, by request id.
    waiting: Vec<(Waits, Frame)>,
    /// The newest record the journal committed.
    committed: u64,
    /// Whether a run is open, which the answer to `attach` names.
    running: bool,
    /// The line of the notice being built, reused.
    line: Vec<u8>,
    /// The frame of the notice being built, reused.
    frame: Vec<u8>,
}

/// One client of the hub: its queue, its name and whether it attached.
#[derive(Debug)]
struct Client {
    id: ClientId,
    name: Arc<str>,
    attached: bool,
    queue: mpsc::Sender<Entry>,
    /// Fires once, when the hub cuts this client.
    closing: Option<oneshot::Sender<()>>,
    /// Whether a send found its queue full; the hub cuts it after the send.
    full: bool,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the client of `id` and its queue; it gets no event until it
    /// attaches. `closing` fires if the hub cuts it.
    pub fn join(
        &mut self,
        id: ClientId,
        name: Arc<str>,
        queue: mpsc::Sender<Entry>,
        closing: oneshot::Sender<()>,
    ) {
        self.clients.push(Client {
            id,
            name,
            attached: false,
            queue,
            closing: Some(closing),
            full: false,
        });
    }

    /// Sets whether a run is open: the answer to `attach` names it.
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    /// Sets the newest record the journal committed.
    pub fn set_committed(&mut self, record: u64) {
        self.committed = self.committed.max(record);
    }

    /// Queues one frame to every attached client, and remembers it where a
    /// client that attaches later catches up from it.
    pub fn send(&mut self, frame: Frame, kind: Kind) {
        match kind {
            Kind::ApprovalRequest(id) => {
                self.waiting.push((Waits::Approval(id), Arc::clone(&frame)))
            }
            Kind::QuestionRequest(id) => {
                self.waiting.push((Waits::Question(id), Arc::clone(&frame)))
            }
            Kind::ApprovalResolved(id) => self
                .waiting
                .retain(|(open, _)| *open != Waits::Approval(id)),
            Kind::QuestionResolved(id) => self
                .waiting
                .retain(|(open, _)| *open != Waits::Question(id)),
            Kind::Committed => {}
            Kind::RunEnd => {
                self.waiting.clear();
                self.uncommitted.push(Arc::clone(&frame));
            }
            Kind::Delta | Kind::Other => self.uncommitted.push(Arc::clone(&frame)),
        }
        let delta = matches!(kind, Kind::Delta);
        self.everyone(&frame, delta);
        if matches!(kind, Kind::Committed) {
            self.uncommitted.clear();
        }
    }

    /// Attaches the client of `id`: the frames it missed as one frame, the
    /// answer to `request`, then the notice of its own join.
    ///
    /// All of them are queued under the hub's lock, so no live frame slips
    /// between the catch-up and what follows it. The catch-up is one frame, so
    /// a long open reply does not fill the queue of the client that missed it.
    ///
    /// The answer names `running`, whether a run is open: a client that missed
    /// the `run_end` of a run the journal committed since its last record would
    /// otherwise not know the run is over.
    pub fn attach(&mut self, id: ClientId, request: Option<u64>) {
        let Some(at) = self.position(id) else {
            return;
        };
        // A second attach has nothing to catch up on: the client already gets
        // every frame. It gets its answer and no second join notice.
        let again = self.clients[at].attached;
        self.clients[at].attached = true;
        let missed = if again {
            0
        } else {
            self.uncommitted.len() + self.waiting.len()
        };
        if missed > 0 {
            let bytes = self
                .uncommitted
                .iter()
                .chain(self.waiting.iter().map(|(_, frame)| frame))
                .map(|frame| frame.len())
                .sum();
            let mut catch_up = Vec::with_capacity(bytes);
            for frame in self
                .uncommitted
                .iter()
                .chain(self.waiting.iter().map(|(_, frame)| frame))
            {
                catch_up.extend_from_slice(frame);
            }
            let frame: Frame = Arc::from(catch_up.as_slice());
            if !self.push_or_cut(at, &frame) {
                return;
            }
        }
        if let Some(request) = request {
            self.frame.clear();
            message::result(
                &mut self.frame,
                request,
                &AttachResult {
                    committed: self.committed,
                    running: self.running,
                },
            );
            let frame: Frame = Arc::from(self.frame.as_slice());
            if !self.push_or_cut(at, &frame) {
                return;
            }
        }
        if !again {
            self.notice_joined(id);
        }
    }

    /// Queues the answer line in `line` to the client of `id`.
    pub fn reply(&mut self, id: ClientId, line: &[u8]) {
        let Some(at) = self.position(id) else {
            return;
        };
        let frame: Frame = Arc::from(line);
        self.push_or_cut(at, &frame);
    }

    /// Takes the client of `id` out: the clients that stay hear that it left.
    pub fn leave(&mut self, id: ClientId) {
        let Some(at) = self.position(id) else {
            return;
        };
        let attached = self.clients[at].attached;
        let client = self.clients.remove(at);
        // The queue closes so its writer drains what is queued, then stops.
        drop(client);
        if attached {
            self.left_notice(id);
        }
    }

    /// Queues a frame to every attached client, then cuts the ones whose queue
    /// was full.
    ///
    /// The cuts wait until every client had the frame: a cut sends a notice to
    /// the others, and a cut inside the loop would move the clients under it.
    fn everyone(&mut self, frame: &Frame, delta: bool) {
        for client in &mut self.clients {
            if client.attached && !client.full {
                client.full = !Self::queue(client, frame, delta);
            }
        }
        while let Some(at) = self.clients.iter().position(|client| client.full) {
            self.behind(at);
        }
    }

    /// Queues a frame to the client at `at`, or cuts it when its queue is full.
    fn push_or_cut(&mut self, at: usize, frame: &Frame) -> bool {
        if Self::queue(&self.clients[at], frame, false) {
            return true;
        }
        self.behind(at);
        false
    }

    /// Queues a frame to one client: false when its queue is full.
    fn queue(client: &Client, frame: &Frame, delta: bool) -> bool {
        let entry = Entry {
            frame: Arc::clone(frame),
            delta,
        };
        client.queue.try_send(entry).is_ok()
    }

    /// Cuts a client that fell behind: nothing more goes to it, its writer
    /// names the reason `behind` and closes the connection, and the clients
    /// that stay hear that it left.
    fn behind(&mut self, at: usize) {
        let client = self.clients.remove(at);
        drop(client.queue);
        if let Some(closing) = client.closing {
            let _ = closing.send(());
        }
        if client.attached {
            self.left_notice(client.id);
        }
    }

    /// Tells every attached client that the client of `id` left.
    fn left_notice(&mut self, id: ClientId) {
        self.line.clear();
        serde_json::to_writer(&mut self.line, &Left { client: id })
            .expect("a notice to a vector cannot fail");
        self.frame.clear();
        message::notify(&mut self.frame, methods::CLIENT_LEFT, &self.line);
        let frame: Frame = Arc::from(self.frame.as_slice());
        self.everyone(&frame, false);
    }

    /// Tells every attached client that the client of `id` joined, with the
    /// name it gave.
    fn notice_joined(&mut self, id: ClientId) {
        let Some(name) = self
            .clients
            .iter()
            .find(|client| client.id == id)
            .map(|client| Arc::clone(&client.name))
        else {
            return;
        };
        self.line.clear();
        serde_json::to_writer(
            &mut self.line,
            &Joined {
                client: id,
                name: &name,
            },
        )
        .expect("a notice to a vector cannot fail");
        self.frame.clear();
        message::notify(&mut self.frame, methods::CLIENT_JOINED, &self.line);
        let frame: Frame = Arc::from(self.frame.as_slice());
        self.everyone(&frame, false);
    }

    /// Where the client of `id` sits.
    fn position(&self, id: ClientId) -> Option<usize> {
        self.clients.iter().position(|client| client.id == id)
    }
}

/// Writes every frame of one client's queue, then closes its half.
///
/// `closing` fires when the hub cut the client: the writer then names the
/// reason and closes the connection instead of waiting on a socket that takes
/// nothing.
pub async fn write<W: AsyncWrite + Unpin>(
    mut out: W,
    mut queue: mpsc::Receiver<Entry>,
    closing: oneshot::Receiver<()>,
) {
    let mut closing = Cut::new(closing);
    let mut batch: Vec<Entry> = Vec::with_capacity(BATCH);
    // The frames that arrive while the socket is busy, so that a slow socket
    // never fills the queue of a client that reads.
    let mut busy: Vec<Entry> = Vec::with_capacity(BATCH);
    loop {
        if batch.is_empty() {
            let taken = tokio::select! {
                biased;
                cut = closing.cut() => {
                    if cut {
                        return cut_off(&mut out).await;
                    }
                    queue.recv_many(&mut batch, BATCH).await
                }
                taken = queue.recv_many(&mut batch, BATCH) => taken,
            };
            if taken == 0 {
                break;
            }
        }
        // A batch that carries a delta waits for the rest of the window or for
        // 64 KiB more; a batch of other events goes out at once.
        if batch.iter().any(Entry::is_delta) {
            let deadline = Instant::now() + DELTA_WAIT;
            let mut bytes: usize = batch.iter().map(Entry::bytes).sum();
            while bytes < DELTA_BYTES && batch.len() < BATCH {
                let room = BATCH - batch.len();
                let more = tokio::select! {
                    biased;
                    cut = closing.cut() => {
                        if cut {
                            return cut_off(&mut out).await;
                        }
                        0
                    }
                    () = tokio::time::sleep_until(deadline) => 0,
                    more = queue.recv_many(&mut batch, room) => more,
                };
                if more == 0 {
                    break;
                }
                bytes += batch[batch.len() - more..]
                    .iter()
                    .map(Entry::bytes)
                    .sum::<usize>();
            }
        }
        match flush(&mut out, &batch, &mut busy, &mut queue, &mut closing).await {
            Flushed::All => {}
            Flushed::Cut => return cut_off(&mut out).await,
            // A notice after part of a frame would make one broken line, so the
            // connection just closes.
            Flushed::CutInFrame => {
                let _ = tokio::time::timeout(CLOSE_WAIT, out.shutdown()).await;
                return;
            }
        }
        batch.clear();
        std::mem::swap(&mut batch, &mut busy);
    }
    let _ = out.shutdown().await;
}

/// The params of the notice a cut client gets.
const BEHIND: &[u8] = br#"{"reason":"behind"}"#;

/// Names the reason to a cut client, then closes its half of the connection.
async fn cut_off<W: AsyncWrite + Unpin>(out: &mut W) {
    let mut line = Vec::with_capacity(64);
    message::notify(&mut line, methods::CLOSING, BEHIND);
    let _ = tokio::time::timeout(CLOSE_WAIT, out.write_all(&line)).await;
    let _ = tokio::time::timeout(CLOSE_WAIT, out.shutdown()).await;
}

/// How a flush ended.
enum Flushed {
    /// Every byte went out.
    All,
    /// The hub cut the client between two frames.
    Cut,
    /// The hub cut the client while part of a frame was out.
    CutInFrame,
}

/// Writes every byte of `batch`, unless the hub cuts the client first.
///
/// `busy` takes the frames that arrive while the socket takes nothing, so that
/// writing a long catch-up frame does not fill the queue the client reads
/// from. The next batch is those frames.
async fn flush<W: AsyncWrite + Unpin>(
    out: &mut W,
    batch: &[Entry],
    busy: &mut Vec<Entry>,
    queue: &mut mpsc::Receiver<Entry>,
    closing: &mut Cut,
) -> Flushed {
    let mut closed = false;
    for chunk in batch.chunks(IOV) {
        let mut slices: [IoSlice<'_>; IOV] = std::array::from_fn(|_| IoSlice::new(&[]));
        for (slice, entry) in slices.iter_mut().zip(chunk) {
            *slice = IoSlice::new(&entry.frame);
        }
        let mut left: &mut [IoSlice<'_>] = &mut slices[..chunk.len()];
        // Whether the first slice left is a frame of which some bytes are out.
        let mut partial = false;
        while !left.is_empty() {
            let room = BATCH - busy.len();
            tokio::select! {
                biased;
                cut = closing.cut() => {
                    if cut {
                        return if partial { Flushed::CutInFrame } else { Flushed::Cut };
                    }
                }
                written = out.write_vectored(left) => match written {
                    Ok(0) | Err(_) => return Flushed::CutInFrame,
                    Ok(written) => {
                        IoSlice::advance_slices(&mut left, written);
                        partial = !left.is_empty() && is_cut_in_frame(left, chunk);
                    }
                },
                // A closed queue stays ready, so it is polled only once.
                taken = queue.recv_many(busy, room), if room > 0 && !closed => {
                    closed = taken == 0;
                }
            }
        }
    }
    Flushed::All
}

/// Whether the first slice in `left` starts inside its frame of `chunk`.
fn is_cut_in_frame(left: &[IoSlice<'_>], chunk: &[Entry]) -> bool {
    let at = chunk.len() - left.len();
    left[0].len() < chunk[at].frame.len()
}

/// The signal that cuts one client.
#[derive(Debug)]
struct Cut(Option<oneshot::Receiver<()>>);

impl Cut {
    fn new(closing: oneshot::Receiver<()>) -> Self {
        Self(Some(closing))
    }

    /// Whether the hub cut the client. A hub that let go of it without cutting
    /// stays silent, so the writer drains what it queued.
    async fn cut(&mut self) -> bool {
        let Some(closing) = self.0.as_mut() else {
            std::future::pending::<()>().await;
            return false;
        };
        let cut = closing.await.is_ok();
        self.0 = None;
        cut
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;

    use serde_json::Value;

    use super::*;

    /// The frames a catch-up test queues before one client attaches.
    const CATCH_UP: usize = 3000;

    /// The writes 1,000 deltas streamed over 100 ms may take.
    const MAX_WRITES: usize = 100 / 16 + 2;

    /// One event frame carrying `params`.
    fn frame(params: &str) -> Frame {
        let mut buf = Vec::new();
        message::notify(&mut buf, methods::EVENT, params.as_bytes());
        Arc::from(buf.as_slice())
    }

    /// One approval request frame for `id`.
    fn asked(id: u64) -> Frame {
        let mut buf = Vec::new();
        message::notify(
            &mut buf,
            methods::APPROVAL_REQUEST,
            format!(r#"{{"id":{id}}}"#).as_bytes(),
        );
        Arc::from(buf.as_slice())
    }

    /// One approval resolved frame for `id`.
    fn resolved(id: u64) -> Frame {
        let mut buf = Vec::new();
        message::notify(
            &mut buf,
            methods::APPROVAL_RESOLVED,
            format!(r#"{{"id":{id}}}"#).as_bytes(),
        );
        Arc::from(buf.as_slice())
    }

    /// One question request frame for `id`.
    fn question(id: u64) -> Frame {
        let mut buf = Vec::new();
        message::notify(
            &mut buf,
            methods::QUESTION_REQUEST,
            format!(
                r#"{{"id":{id},"questions":[{{"q":"Which database?","options":[{{"label":"sqlite"}}],"multi":false}}]}}"#
            )
            .as_bytes(),
        );
        Arc::from(buf.as_slice())
    }

    /// One question resolved frame for `id`, by `client`.
    fn answer(id: u64) -> Frame {
        let mut buf = Vec::new();
        message::notify(
            &mut buf,
            methods::QUESTION_RESOLVED,
            format!(r#"{{"id":{id},"client":1}}"#).as_bytes(),
        );
        Arc::from(buf.as_slice())
    }

    /// A hub whose clients are the ids, their queues and their cut signals.
    fn hub_with(ids: &[ClientId]) -> (Hub, Vec<mpsc::Receiver<Entry>>, Vec<oneshot::Receiver<()>>) {
        let mut hub = Hub::new();
        let mut queues = Vec::new();
        let mut cuts = Vec::new();
        for id in ids {
            let (queue, frames) = channel();
            let (closing, cut) = oneshot::channel();
            hub.join(*id, Arc::from(format!("c{id}")), queue, closing);
            queues.push(frames);
            cuts.push(cut);
        }
        (hub, queues, cuts)
    }

    /// Every entry a queue holds now.
    fn drain(queue: &mut mpsc::Receiver<Entry>) -> Vec<Entry> {
        let mut entries = Vec::new();
        while let Ok(entry) = queue.try_recv() {
            entries.push(entry);
        }
        entries
    }

    /// The frames of `entries`.
    fn frames(entries: Vec<Entry>) -> Vec<Frame> {
        entries.into_iter().map(|entry| entry.frame).collect()
    }

    /// Every line the frames carry, in order.
    fn lines(frames: &[Frame]) -> Vec<Value> {
        frames
            .iter()
            .flat_map(|frame| frame.split(|byte| *byte == b'\n'))
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("a line is JSON"))
            .collect()
    }

    #[test]
    fn one_encode_reaches_three_clients() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1, 2, 3]);
        for id in [1, 2, 3] {
            hub.attach(id, Some(id.into()));
        }
        for queue in &mut queues {
            drain(queue);
        }
        hub.send(frame(r#"{"type":"text_delta","text":"x"}"#), Kind::Delta);
        let frames: Vec<Frame> = queues
            .iter_mut()
            .map(|queue| {
                let frames = frames(drain(queue));
                assert_eq!(frames.len(), 1, "one event, one frame");
                frames[0].clone()
            })
            .collect();
        assert!(
            Arc::ptr_eq(&frames[0], &frames[1]) && Arc::ptr_eq(&frames[1], &frames[2]),
            "every client gets the same frame, not a copy of it"
        );
        let frame = lines(&frames[..1]);
        assert_eq!(frame[0]["method"], "event");
        assert_eq!(frame[0]["params"]["type"], "text_delta");
        assert_eq!(frame[0]["params"]["text"], "x");
    }

    #[test]
    fn the_uncommitted_list_clears_on_committed() {
        let mut hub = Hub::new();
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        hub.send(frame(r#"{"type":"text_delta","text":"x"}"#), Kind::Delta);
        assert_eq!(hub.uncommitted.len(), 2);
        hub.send(frame(r#"{"type":"committed"}"#), Kind::Committed);
        assert!(
            hub.uncommitted.is_empty(),
            "a committed record ends what a client catches up on"
        );
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        assert_eq!(hub.uncommitted.len(), 1, "only the frames after it stay");
    }

    #[test]
    fn a_resolved_approval_leaves_the_waiting_list() {
        let mut hub = Hub::new();
        hub.send(asked(4), Kind::ApprovalRequest(4));
        assert_eq!(hub.waiting.len(), 1);
        assert!(
            hub.uncommitted.is_empty(),
            "an approval frame waits once, not twice"
        );
        hub.send(resolved(4), Kind::ApprovalResolved(4));
        assert!(hub.waiting.is_empty(), "an answered request does not wait");
    }

    #[test]
    fn the_end_of_a_run_takes_a_cancelled_approval_out_of_the_catch_up() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1]);
        hub.send(asked(4), Kind::ApprovalRequest(4));
        hub.send(
            frame(r#"{"type":"run_end","reason":"cancelled"}"#),
            Kind::RunEnd,
        );
        assert!(hub.waiting.is_empty(), "a cancelled request does not wait");
        hub.attach(1, None);
        let seen = lines(&frames(drain(&mut queues[0])));
        assert!(
            seen.iter().all(|line| line["method"] != "approval_request"),
            "a late client is not asked a question no call waits for: {seen:?}"
        );
    }

    #[test]
    fn ask_a_client_that_attaches_while_a_question_waits_gets_it() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1]);
        hub.send(question(4), Kind::QuestionRequest(4));
        hub.attach(1, None);
        let seen = lines(&frames(drain(&mut queues[0])));
        let asked: Vec<&Value> = seen
            .iter()
            .filter(|line| line["method"] == "question_request")
            .collect();
        assert_eq!(
            asked.len(),
            1,
            "the catch-up carries the question: {seen:?}"
        );
        assert_eq!(asked[0]["params"]["id"], 4);
        assert!(asked[0]["params"]["questions"].is_array());

        hub.send(answer(4), Kind::QuestionResolved(4));
        let (mut hub, mut queues, _cuts) = hub_with(&[2]);
        hub.send(question(5), Kind::QuestionRequest(5));
        hub.send(answer(5), Kind::QuestionResolved(5));
        assert!(hub.waiting.is_empty(), "an answered question waits no more");
        hub.attach(2, None);
        let seen = lines(&frames(drain(&mut queues[0])));
        assert!(
            seen.iter().all(|line| line["method"] != "question_request"),
            "a resolved question is not asked again: {seen:?}"
        );
    }

    #[test]
    fn two_cuts_in_one_send_skip_no_client() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1, 2, 3]);
        for id in [1, 2, 3] {
            hub.attach(id, None);
        }
        for queue in &mut queues {
            drain(queue);
        }
        let turn = frame(r#"{"type":"turn_start"}"#);
        for _ in 0..CAPACITY - 1 {
            hub.send(Arc::clone(&turn), Kind::Other);
            drain(&mut queues[2]);
        }
        // The first client has room for one frame, the second has none.
        let _ = queues[0].try_recv();
        hub.send(Arc::clone(&turn), Kind::Other);
        drain(&mut queues[2]);
        let last = frame(r#"{"type":"turn_end","reason":"end_turn"}"#);
        hub.send(Arc::clone(&last), Kind::Other);
        let got = frames(drain(&mut queues[2]));
        assert!(
            Arc::ptr_eq(&got[0], &last),
            "the client that reads gets the frame before the notices"
        );
        let left: Vec<u64> = lines(&got[1..])
            .iter()
            .map(|line| line["params"]["client"].as_u64().expect("a client id"))
            .collect();
        assert_eq!(left, vec![2, 1], "both cut clients are named, once each");
        assert_eq!(hub.clients.len(), 1);
    }

    #[test]
    fn a_second_attach_sends_no_second_catch_up() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1, 2]);
        hub.attach(2, None);
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        hub.attach(1, Some(1));
        drain(&mut queues[0]);
        drain(&mut queues[1]);
        hub.attach(1, Some(2));
        let again = lines(&frames(drain(&mut queues[0])));
        assert_eq!(again.len(), 1, "only the answer: {again:?}");
        assert_eq!(again[0]["id"], 2);
        assert!(
            drain(&mut queues[1]).is_empty(),
            "no second join notice for the other client"
        );
    }

    #[test]
    fn attach_gets_the_uncommitted_frames_then_the_waiting_ones() {
        let (mut hub, mut queues, _cuts) = hub_with(&[2]);
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        hub.send(asked(7), Kind::ApprovalRequest(7));
        hub.send(frame(r#"{"type":"text_delta","text":"x"}"#), Kind::Delta);
        let mut queue = queues.pop().expect("a queue");
        assert!(
            drain(&mut queue).is_empty(),
            "a client that has not attached gets no event"
        );

        hub.attach(2, Some(9));
        let frames = lines(&frames(drain(&mut queue)));
        assert_eq!(
            frames.len(),
            5,
            "the catch-up, the answer, the notice: {frames:?}"
        );
        assert_eq!(frames[0]["params"]["type"], "turn_start");
        assert_eq!(frames[1]["params"]["type"], "text_delta");
        assert_eq!(frames[2]["method"], "approval_request");
        assert_eq!(frames[2]["params"]["id"], 7);
        assert_eq!(frames[3]["id"], 9, "the answer follows the catch-up");
        assert_eq!(frames[3]["result"]["committed"], 0);
        assert_eq!(frames[4]["method"], "client_joined");
        assert_eq!(frames[4]["params"]["client"], 2);
    }

    #[test]
    fn the_catch_up_of_a_long_open_reply_is_one_frame() {
        let (mut hub, mut queues, mut cuts) = hub_with(&[1]);
        for at in 0..CATCH_UP {
            hub.send(
                frame(&format!(r#"{{"type":"text_delta","text":"d{at};"}}"#)),
                Kind::Delta,
            );
        }
        let mut queue = queues.pop().expect("a queue");
        assert!(drain(&mut queue).is_empty());
        hub.attach(1, Some(1));
        let entries = drain(&mut queue);
        assert!(cuts[0].try_recv().is_err(), "the client is not cut");
        assert_eq!(
            entries.len(),
            3,
            "one catch-up frame, one answer, one notice"
        );
        assert!(
            entries[0].frame.len() > CATCH_UP,
            "the catch-up is one frame, not {CATCH_UP}"
        );
        let lines = lines(&frames(entries)[..1]);
        assert_eq!(lines.len(), CATCH_UP, "every delta is in the catch-up");
        for (at, line) in lines.iter().enumerate() {
            assert_eq!(
                line["params"]["text"],
                format!("d{at};"),
                "the catch-up keeps the order"
            );
        }
    }

    #[test]
    fn the_catch_up_names_the_committed_record() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1]);
        hub.set_committed(5);
        hub.send(frame(r#"{"type":"committed"}"#), Kind::Committed);
        hub.attach(1, Some(2));
        let frames = lines(&frames(drain(&mut queues[0])));
        let answer = frames
            .iter()
            .find(|frame| frame["id"] == 2)
            .expect("the attach answer");
        assert_eq!(answer["result"]["committed"], 5);
    }

    #[test]
    fn the_attach_answer_says_whether_a_run_is_open() {
        // The run ended before the record the journal committed, so its
        // `run_end` is in no frame the client catches up on: the answer says
        // the run is over.
        let (mut hub, mut queues, _cuts) = hub_with(&[1]);
        hub.send(
            frame(r#"{"type":"run_end","reason":"end_turn"}"#),
            Kind::RunEnd,
        );
        hub.set_committed(5);
        hub.send(frame(r#"{"type":"committed"}"#), Kind::Committed);
        hub.attach(1, Some(2));
        let caught = lines(&frames(drain(&mut queues[0])));
        let answer = caught
            .iter()
            .find(|frame| frame["id"] == 2)
            .expect("the attach answer");
        assert_eq!(answer["result"]["running"], false, "{answer}");
        assert!(
            !caught
                .iter()
                .any(|frame| frame["params"]["type"] == "run_end"),
            "the end of the run is past the catch-up: {caught:?}"
        );

        let (mut hub, mut queues, _cuts) = hub_with(&[1]);
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        hub.set_running(true);
        hub.attach(1, Some(1));
        let again = lines(&frames(drain(&mut queues[0])));
        let answer = again
            .iter()
            .find(|frame| frame["id"] == 1)
            .expect("the attach answer");
        assert_eq!(answer["result"]["running"], true, "{answer}");
    }

    #[test]
    fn a_full_queue_drops_only_that_client() {
        let (mut hub, mut queues, mut cuts) = hub_with(&[1, 2]);
        hub.attach(1, Some(1));
        hub.attach(2, Some(2));
        let mut behind = queues.swap_remove(0);
        let mut reading = queues.swap_remove(0);
        let mut cut = cuts.swap_remove(0);
        drain(&mut behind);
        drain(&mut reading);
        // Only the second client reads, so only the first one falls behind.
        for _ in 0..CAPACITY {
            hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
            drain(&mut reading);
        }
        assert!(cut.try_recv().is_err(), "the full queue is not a cut yet");
        hub.send(frame(r#"{"type":"turn_start"}"#), Kind::Other);
        assert_eq!(hub.clients.len(), 1, "only the client behind is gone");
        assert_eq!(hub.clients[0].id, 2);
        assert_eq!(drain(&mut behind).len(), CAPACITY, "nothing more for it");
        assert!(behind.try_recv().is_err(), "its queue closed");
        let after = lines(&frames(drain(&mut reading)));
        assert_eq!(
            after.len(),
            2,
            "the frame and the notice of the client that left: {after:?}"
        );
        let (notice, event) = after
            .iter()
            .partition::<Vec<_>, _>(|line| line["method"] == "client_left");
        assert_eq!(notice.len(), 1, "{after:?}");
        assert_eq!(notice[0]["params"]["client"], 1);
        assert_eq!(event[0]["params"]["type"], "turn_start");
        assert_eq!(cut.try_recv().ok(), Some(()), "its writer is cut");
    }

    #[test]
    fn a_client_that_leaves_is_named_to_the_others() {
        let (mut hub, mut queues, _cuts) = hub_with(&[1, 2]);
        hub.attach(1, Some(1));
        hub.attach(2, Some(2));
        drain(&mut queues[0]);
        drain(&mut queues[1]);
        hub.leave(2);
        let frames = lines(&frames(drain(&mut queues[0])));
        let left = frames.last().expect("a notice");
        assert_eq!(left["method"], "client_left");
        assert_eq!(left["params"]["client"], 2);
        assert!(
            drain(&mut queues[1]).is_empty(),
            "the client that left gets nothing more"
        );
    }

    /// A writer that takes every buffer in one call and counts its calls.
    #[derive(Debug)]
    struct Counted {
        writes: Arc<AtomicUsize>,
        /// Whether the bytes of the first write may still be on their way.
        seen: Arc<AtomicUsize>,
    }

    impl AsyncWrite for Counted {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.seen.fetch_add(buf.len(), Ordering::Relaxed);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let bytes: usize = bufs.iter().map(|buf| buf.len()).sum();
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.seen.fetch_add(bytes, Ordering::Relaxed);
            Poll::Ready(Ok(bytes))
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

    /// A writer whose writes are counted, and what it was given.
    fn counted() -> (Counted, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let writes = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let out = Counted {
            writes: Arc::clone(&writes),
            seen: Arc::clone(&seen),
        };
        (out, writes, seen)
    }

    #[tokio::test(start_paused = true)]
    async fn deltas_go_out_in_one_write_per_window() {
        let (out, writes, _seen) = counted();
        let (queue, entries) = channel();
        let (_closing, cut) = oneshot::channel();
        let writer = tokio::spawn(write(out, entries, cut));
        for at in 0..100 {
            for _ in 0..10 {
                queue
                    .send(Entry {
                        frame: frame(&format!(r#"{{"type":"text_delta","text":"d{at};"}}"#)),
                        delta: true,
                    })
                    .await
                    .expect("queued");
            }
            // 100 rounds of 10 deltas: 100 ms of streaming.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(queue);
        writer.await.expect("the writer ran");
        let taken = writes.load(Ordering::Relaxed);
        assert!(
            taken <= MAX_WRITES,
            "{taken} writes for 1,000 deltas over 100 ms, over {MAX_WRITES}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_that_is_not_a_delta_goes_out_at_once() {
        let (out, writes, seen) = counted();
        let (queue, entries) = channel();
        let (_closing, cut) = oneshot::channel();
        let writer = tokio::spawn(write(out, entries, cut));
        let start = Instant::now();
        queue
            .send(Entry {
                frame: frame(r#"{"type":"turn_start"}"#),
                delta: false,
            })
            .await
            .expect("queued");
        while seen.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        let waited = start.elapsed();
        drop(queue);
        writer.await.expect("the writer ran");
        assert_eq!(writes.load(Ordering::Relaxed), 1, "one frame, one write");
        assert_eq!(
            waited,
            Duration::ZERO,
            "a frame that is not a delta waits for nothing"
        );
    }
}

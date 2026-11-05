//! The status line: the run's state on the left, the key hints on the right.
//!
//! It is one row with no band of its own, two cells in from the left edge, on
//! the last row the frame draws. The left part is the state: the moving bar,
//! the word for it, the elapsed time and the key that ends it, or the pause of a
//! box that waits, or `idle`; the hints follow from the right end, whole, and
//! drop off the right first when the row is short. Every style is a theme role
//! ([`App::style`]), so the line reads in terminal mode, where the glyphs and
//! the weight are the only cues.
//!
//! The bar has a clock of its own: [`Motion`] holds it and [`App::next_tick`]
//! names the instant of its next step, which `paint_status` arms on `deadline`,
//! the timer the run loop already waits on (the way `arm_tick` arms a panel's).
//! A run that ends arms nothing, so an idle client keeps its zero wakeups, and
//! the clock is read only while a run is on screen. A reply that streams draws
//! its own frames, and the step stands under them: a stream that stalls still
//! moves the bar and the elapsed time behind it.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::theme::{Emphasis, Role};

use super::super::App;

/// The bar's frames, the filled run of three cells at each offset it can take,
/// so a step draws one string and composes nothing.
const BAR: [&str; 6] = [
    "▰▰▰▱▱▱▱▱",
    "▱▰▰▰▱▱▱▱",
    "▱▱▰▰▰▱▱▱",
    "▱▱▱▰▰▰▱▱",
    "▱▱▱▱▰▰▰▱",
    "▱▱▱▱▱▰▰▰",
];

/// The frame the bar holds still on under `NO_MOTION`: the one the mockup draws.
const BAR_STILL: &str = BAR[1];

/// The cells of the bar, and the cells its filled run covers.
const BAR_CELLS: u64 = 8;
const BAR_RUN: u64 = 3;

/// The cells the run travels, so one there-and-back takes ten steps.
const BAR_TRAVEL: u64 = BAR_CELLS - BAR_RUN;

/// One step of the bar, and how often the elapsed time moves under `NO_MOTION`,
/// which drops the step.
const STEP: Duration = Duration::from_millis(100);
const SECOND: Duration = Duration::from_secs(1);

/// The columns the line starts at: the mockup's two cells of left padding.
const PAD: u16 = 2;

/// The words of the line.
const IDLE: &str = "idle";
const WORKING: &str = "working";
const INTERRUPT: &str = "esc interrupt";
const WAITS: &str = "⏸ waits for you";
const ANSWER_KEY: &str = "←→ enter";
const ANSWER_WORD: &str = "answer";
const RESYNCED: &str = "resynced";
const ARMED: &str = "Ctrl+C again to quit";

/// The key hints, with the columns each takes, in the order they drop: off the
/// right end first, whole, so the left part always shows. They come from the
/// key table, so the line and the help sheet name the same keys.
const HINTS: &[crate::keys::Hint] = crate::keys::STATUS;

/// The columns between two hints, and the space the last one ends on.
const HINT_GAP: u16 = 2;
const HINT_TAIL: u16 = 1;

impl App {
    /// Takes what the line carries between frames: the note it shows once, and
    /// the run its clock counts.
    ///
    /// The line has no built copy to replace — it is drawn span by span — so
    /// this keeps the note in the app's own buffer and hands the run state to
    /// the clock the bar and the elapsed time are read from.
    pub(crate) fn refresh_status(&mut self) {
        if !self.status_dirty {
            return;
        }
        self.status_dirty = false;
        // A note names itself once, until the next change the line shows.
        self.status.clear();
        if std::mem::take(&mut self.resynced) {
            self.status.push_str(RESYNCED);
        }
        let (running, paused) = (self.running, self.approval.is_some());
        let now = Instant::now();
        MOTION.with_borrow_mut(|motion| motion.take(running, paused, now));
    }

    /// When the bar's next step is due, for a loop that waits on it; `None` when
    /// nothing on the line moves, which is every idle frame and every pause.
    ///
    /// The run loop waits on `deadline` already (`arm_tick` arms a panel's tick
    /// there too), so this names the instant and [`App::paint_status`] arms it:
    /// the bar needs no branch of the loop and no timer of its own. A streamed
    /// reply draws its own frames while the deltas come, but a step stands on
    /// this clock as well: a stream that stalls leaves the bar and the elapsed
    /// time behind it moving.
    pub(crate) fn next_tick(&self) -> Option<Instant> {
        if !self.running || self.approval.is_some() {
            return None;
        }
        let now = Instant::now();
        MOTION.with_borrow_mut(|motion| motion.next(now))
    }

    /// Draws the status line at row `y`, cut at `width` columns, and arms the
    /// frame that draws its next step.
    ///
    /// The note the line carries is taken out of the app's own buffer for the
    /// draw and put back, so a frame copies no text into a second `String`. The
    /// armed instant goes on `deadline`, which the loop already waits on, and
    /// the frame it draws is asked for here: a step is pending until it lands.
    pub(crate) fn paint_status(&mut self, y: u16, rows: u16, width: u16, _dim: u32) {
        let y = y.min(rows.saturating_sub(1));
        let width = width.max(1);
        let note = std::mem::take(&mut self.status);
        MOTION.with_borrow_mut(|motion| self.draw_status(motion, &note, y, width));
        self.status = note;
        let Some(due) = self.next_tick() else {
            return;
        };
        let due = tokio::time::Instant::from_std(due);
        if self.deadline.is_none_or(|deadline| due < deadline) {
            self.deadline = Some(due);
            self.dirty = true;
        }
    }

    /// Draws one row of the line: the state on the left, the note, the hint of
    /// an armed quit, and the key hints that fit from the right end.
    ///
    /// The clock is read here, in the two states that show a time, so an idle
    /// frame — the one a key draws — reads no clock at all.
    fn draw_status(&mut self, motion: &mut Motion, note: &str, y: u16, width: u16) {
        let accent = self.style(Role::Accent, Emphasis::Plain);
        let bold = self.style(Role::Text, Emphasis::Bold);
        let plain = self.style(Role::Text, Emphasis::Plain);
        let dim = self.style(Role::Dim, Emphasis::Plain);
        let mut x = PAD;
        if self.approval.is_some() {
            // The box waits, so the run does: the glyph, the wait and the answer.
            let now = Instant::now();
            let warn = self.style(Role::Warn, Emphasis::Bold);
            x = self.screen.put_str_until(x, y, WAITS, warn, width);
            x = x.saturating_add(1);
            x = self
                .screen
                .put_str_until(x, y, motion.time(now), dim, width);
            x = x.saturating_add(2);
            x = self.screen.put_str_until(x, y, ANSWER_KEY, plain, width);
            x = x.saturating_add(1);
            x = self.screen.put_str_until(x, y, ANSWER_WORD, dim, width);
        } else if self.running {
            // A run: the bar steps, then what it is doing, for how long, and the
            // key that ends it.
            let now = Instant::now();
            let bar = motion.bar(now);
            x = self.screen.put_str_until(x, y, bar, accent, width);
            x = x.saturating_add(1);
            x = self.screen.put_str_until(x, y, WORKING, bold, width);
            x = x.saturating_add(1);
            x = self
                .screen
                .put_str_until(x, y, motion.time(now), dim, width);
            x = x.saturating_add(2);
            x = self.screen.put_str_until(x, y, INTERRUPT, dim, width);
        } else {
            x = self.screen.put_str_until(x, y, IDLE, dim, width);
        }
        if !note.is_empty() {
            x = x.saturating_add(2);
            x = self.screen.put_str_until(x, y, note, dim, width);
        }
        if self.armed {
            x = x.saturating_add(2);
            x = self.screen.put_str_until(x, y, ARMED, dim, width);
        }
        self.draw_hints(y, width, x, plain, dim);
    }

    /// Draws the hints that fit, whole, from the right end: the left part always
    /// shows, and the hints drop off the right end first.
    fn draw_hints(&mut self, y: u16, width: u16, left: u16, key: u32, word: u32) {
        let mut shown = HINTS.len();
        while shown > 0 && left.saturating_add(1).saturating_add(hints_width(shown)) > width {
            shown -= 1;
        }
        if shown == 0 {
            return;
        }
        // The hints end one space short of the edge, so the run of blanks in
        // front of them takes whatever the left part leaves.
        let mut x = width - hints_width(shown);
        for (index, hint) in HINTS[..shown].iter().enumerate() {
            if index > 0 {
                x = x.saturating_add(HINT_GAP);
            }
            x = self.screen.put_str_until(x, y, hint.keys, key, width);
            x = self.screen.put_str_until(x, y, " ", key, width);
            x = self.screen.put_str_until(x, y, hint.words, word, width);
        }
    }
}

/// The columns one hint takes: its key, one space and its words.
fn hint_columns(hint: &crate::keys::Hint) -> u16 {
    (hint.keys.len() + 1 + hint.words.len()) as u16
}

/// The columns the first `shown` hints take: their own, the gap between them and
/// the space the last one ends on.
fn hints_width(shown: usize) -> u16 {
    let mut width = HINT_TAIL;
    for (index, hint) in HINTS[..shown].iter().enumerate() {
        width += hint_columns(hint);
        if index > 0 {
            width += HINT_GAP;
        }
    }
    width
}

/// The offset of the bar's filled run at `step`: it travels one cell per step to
/// the last cell it fits and comes back, so the bar reads right and then left.
const fn offset_of(step: u64) -> usize {
    let at = step % (BAR_TRAVEL * 2);
    let offset = if at <= BAR_TRAVEL {
        at
    } else {
        BAR_TRAVEL * 2 - at
    };
    offset as usize
}

/// Writes the elapsed time the line shows — seconds under a minute, minutes and
/// two digits of seconds past it — into `out`, without building a `String`.
fn write_time(out: &mut String, gone: Duration) {
    let seconds = gone.as_secs();
    out.clear();
    let _ = if seconds < 60 {
        write!(out, "{seconds}s")
    } else {
        write!(out, "{}m {:02}s", seconds / 60, seconds % 60)
    };
}

/// The clock the status line counts with: the run it shows, the pause an answer
/// puts it on, and the buffer the elapsed time is written into.
#[derive(Debug, Default)]
pub(crate) struct Motion {
    /// When the run started, and so where the bar and the time come from: `None`
    /// when nothing runs, so nothing moves and no frame is armed.
    started: Option<Instant>,
    /// How far the run had gone when a box started to wait, frozen while it
    /// waits: the line says what the run paused at, not how long it waited.
    paused: Option<Duration>,
    /// Whether the bar moves, read from `NO_MOTION` the first time it draws.
    moving: Option<bool>,
    /// The elapsed time as drawn, formatted into this buffer and reused, so a
    /// step allocates no block.
    time: String,
}

impl Motion {
    /// A clock that has not been wound: nothing runs yet.
    fn new() -> Motion {
        Motion::default()
    }

    /// A bar that moves, whatever the environment says: a test's bar.
    #[cfg(test)]
    fn stepping() -> Motion {
        Motion {
            moving: Some(true),
            ..Motion::new()
        }
    }

    /// Takes the run the line shows: a run that starts winds the clock, a run
    /// that ends stops it, and a box that waits freezes the time it waits at.
    fn take(&mut self, running: bool, paused: bool, now: Instant) {
        if !running && !paused {
            self.started = None;
            self.paused = None;
            return;
        }
        let started = *self.started.get_or_insert(now);
        if paused {
            self.paused
                .get_or_insert_with(|| now.saturating_duration_since(started));
        } else {
            self.paused = None;
        }
    }

    /// Whether the bar moves: `NO_MOTION` draws it still, and the elapsed time
    /// then moves once a second.
    fn moves(&mut self) -> bool {
        *self
            .moving
            .get_or_insert_with(|| crate::theme::Flags::from_env().motion)
    }

    /// How far the run has gone, frozen at the pause while an answer waits.
    fn elapsed(&self, now: Instant) -> Duration {
        if let Some(paused) = self.paused {
            return paused;
        }
        match self.started {
            Some(started) => now.saturating_duration_since(started),
            None => Duration::ZERO,
        }
    }

    /// The step the bar is on: one per [`STEP`] of the run.
    fn step(&self, now: Instant) -> u64 {
        u64::try_from(self.elapsed(now).as_millis() / STEP.as_millis()).unwrap_or(u64::MAX)
    }

    /// The bar's frame: the run's step while it moves, the mockup's still frame
    /// under `NO_MOTION`.
    fn bar(&mut self, now: Instant) -> &'static str {
        if !self.moves() {
            return BAR_STILL;
        }
        BAR[offset_of(self.step(now))]
    }

    /// The elapsed time as the line draws it, into the buffer this holds.
    fn time(&mut self, now: Instant) -> &str {
        let gone = self.elapsed(now);
        write_time(&mut self.time, gone);
        if self.paused.is_some() {
            self.time.push_str(", paused");
        }
        &self.time
    }

    /// When the line must be drawn again: the next step of the bar, or the next
    /// second under `NO_MOTION`. `None` when nothing runs or a box waits, so a
    /// still line arms no timer.
    fn next(&mut self, now: Instant) -> Option<Instant> {
        let started = self.started?;
        if self.paused.is_some() {
            return None;
        }
        let every = if self.moves() { STEP } else { SECOND };
        let gone = now.saturating_duration_since(started);
        let done = u32::try_from(gone.as_millis() / every.as_millis()).unwrap_or(u32::MAX);
        Some(started + every * done.saturating_add(1))
    }
}

thread_local! {
    /// The clock of the status line: one per thread, which is the thread the
    /// app's loop draws on, so a test holds its own.
    static MOTION: RefCell<Motion> = RefCell::new(Motion::new());
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use crate::app::{ApprovalBox, Item};
    use crate::client::{Approval, Initialized};

    use super::*;

    /// The terminal these cases draw in: wide enough for every hint, tall
    /// enough for an approval card above the status line.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// An app in full screen, its status line on the last row, nothing running.
    fn app(columns: u16) -> App {
        let init = Initialized {
            session: "sess-1".to_owned(),
            ..Initialized::default()
        };
        App::new(&init, (columns, ROWS), true)
    }

    /// One frame drawn the way the run loop draws it: the deadline of the frame
    /// before it cleared, then the regions painted.
    fn frame(app: &mut App) {
        app.deadline = None;
        app.dirty = false;
        app.paint();
    }

    /// The text of the status row, trailing blanks trimmed.
    fn row(app: &App) -> String {
        let y = app.screen.rows() - 1;
        let mut text = String::new();
        for x in 0..app.screen.columns() {
            match app.screen.back().cell(x, y).char() {
                Some(drawn) => text.push(drawn),
                None => text.push(' '),
            }
        }
        let keep = text.trim_end_matches(' ').len();
        text.truncate(keep);
        text
    }

    /// Moves the line's clock to a run that started `gone` ago, leaving the
    /// buffers the line draws with as they are.
    fn rewind(gone: Duration) {
        MOTION.with_borrow_mut(|motion| {
            motion.moving = Some(true);
            motion.started = Instant::now().checked_sub(gone);
        });
    }

    /// The capacity of the buffer the elapsed time is written into.
    fn time_capacity() -> usize {
        MOTION.with_borrow(|motion| motion.time.capacity())
    }

    /// The bar travels one cell per step to the last cell it fits and comes
    /// back, so 14 steps of the clock read right and then left.
    #[test]
    fn moving_bar_steps_right_then_left_over_fourteen_steps() {
        let offsets: Vec<u64> = (0..14).map(|step| offset_of(step) as u64).collect();
        assert_eq!(
            offsets,
            [0, 1, 2, 3, 4, 5, 4, 3, 2, 1, 0, 1, 2, 3],
            "the run goes right to the last cell and back"
        );
        // Each offset is one frame of the bar: eight cells, three of them filled,
        // and the filled run starts at the offset.
        assert_eq!(BAR.len() as u64, BAR_TRAVEL + 1, "a frame per offset");
        for (step, offset) in offsets.iter().copied().enumerate() {
            let frame = BAR[offset as usize];
            assert_eq!(frame.chars().count() as u64, BAR_CELLS, "step {step}");
            let filled = frame.chars().filter(|cell| *cell == '▰').count() as u64;
            assert_eq!(filled, BAR_RUN, "step {step}: {frame}");
            let before = frame
                .chars()
                .take(offset as usize)
                .filter(|cell| *cell == '▰')
                .count();
            assert_eq!(
                before, 0,
                "step {step}: the run starts at {offset} in {frame}"
            );
        }
        // The clock names the same step: 100 ms of the run per step.
        let now = Instant::now();
        let mut motion = Motion::stepping();
        motion.started = now.checked_sub(STEP * 13);
        assert_eq!(offset_of(motion.step(now)) as u64, 3, "13 steps in");
    }

    /// A run at its start draws the bar at the first offset, and the clock names
    /// one step of it; nothing runs here, so nothing is armed.
    #[test]
    fn a_status_line_arms_no_tick_when_idle() {
        let mut app = app(COLUMNS);
        frame(&mut app);
        assert_eq!(app.next_tick(), None, "an idle line moves nothing");
        assert_eq!(app.deadline, None, "an idle app arms no timer");
        assert!(!app.dirty, "an idle frame asks for no other frame");
        let text = row(&app);
        assert!(text.starts_with("  idle"), "{text:?}");
        assert!(
            text.ends_with("ctrl+x / commands  ctrl+x more  ? keys"),
            "every hint fits at 80 columns: {text:?}"
        );
    }

    /// Under `NO_MOTION` the bar holds still and the clock moves the elapsed time
    /// once a second, so no step of 100 ms is armed; with motion the same run
    /// arms the step.
    #[test]
    fn moving_bar_under_no_motion_arms_no_hundred_millisecond_tick() {
        let mut app = app(COLUMNS);
        app.running = true;
        MOTION.with_borrow_mut(|motion| motion.moving = Some(false));
        frame(&mut app);
        let due = app.deadline.expect("the elapsed time still moves");
        let wait = due.into_std().saturating_duration_since(Instant::now());
        assert!(!wait.is_zero(), "the tick is in the future");
        assert!(
            wait <= SECOND,
            "under NO_MOTION the line moves once a second, not once a step: {wait:?}"
        );
        let text = row(&app);
        assert!(text.contains(BAR_STILL), "the bar holds still: {text:?}");

        // The same run with the bar moving arms the step instead.
        MOTION.with_borrow_mut(|motion| motion.moving = Some(true));
        frame(&mut app);
        let due = app.deadline.expect("a moving bar arms its step");
        let wait = due.into_std().saturating_duration_since(Instant::now());
        assert!(!wait.is_zero(), "the step is in the future");
        assert!(wait <= STEP, "one step, not one second: {wait:?}");
        assert!(
            row(&app).contains(BAR[0]),
            "a run at its start: {:?}",
            row(&app)
        );
    }

    /// A live reply draws its own frames, but not while the stream stalls: the
    /// bar's step stands on the app's clock too, so the time it names and the
    /// elapsed time behind it keep moving when no delta arrives.
    #[test]
    fn moving_bar_steps_while_a_live_reply_streams() {
        let mut app = app(COLUMNS);
        app.running = true;
        MOTION.with_borrow_mut(|motion| motion.moving = Some(true));
        app.live.push(Item::Thinking("reading the file".to_owned()));
        frame(&mut app);
        let text = row(&app);
        assert!(text.contains("working"), "{text:?}");
        assert!(text.contains(BAR[0]), "the bar still draws: {text:?}");
        let due = app.next_tick().expect("a streamed step is still armed");
        let wait = due.saturating_duration_since(Instant::now());
        assert!(!wait.is_zero(), "the step is in the future");
        assert!(wait <= STEP, "one step, not one second: {wait:?}");
        let armed = app.deadline.expect("a stalled stream still steps");
        assert_eq!(
            armed.into_std(),
            due,
            "the step the line names is the one armed"
        );
    }

    /// A box that waits pauses the line: it names the wait, the time it paused
    /// at and the keys that answer it, and it arms nothing while it waits.
    #[test]
    fn a_status_line_in_an_approval_waits_for_you() {
        let mut app = app(COLUMNS);
        app.running = true;
        frame(&mut app);
        app.approval = Some(ApprovalBox::new(
            Approval {
                id: 1,
                tool: "bash".into(),
                call: "rm -rf target".into(),
                rule: "outside the project".into(),
                save: true,
            },
            COLUMNS,
        ));
        app.status_dirty = true;
        // The card builds its rows on the frame it first draws them, and full
        // screen reserves them before it draws, so the frame asks first.
        app.approval_rows();
        frame(&mut app);
        let text = row(&app);
        assert!(text.contains("⏸ waits for you"), "{text:?}");
        assert!(text.contains(", paused"), "{text:?}");
        assert!(text.contains("←→ enter answer"), "{text:?}");
        assert!(!text.contains("working"), "{text:?}");
        assert_eq!(app.next_tick(), None, "a paused line moves nothing");
        assert_eq!(app.deadline, None, "and arms no timer");
    }

    /// On a short row the left part stays and the hints drop off the right end
    /// whole: never half a hint, and never a hint cut over the left part.
    #[test]
    fn a_status_line_at_sixty_columns_keeps_the_left_part_and_drops_hints_whole() {
        let mut app = app(60);
        app.running = true;
        MOTION.with_borrow_mut(|motion| motion.moving = Some(true));
        frame(&mut app);
        let text = row(&app);
        assert!(
            text.starts_with(&format!("  {} working 0s  {INTERRUPT}", BAR[0])),
            "the left part stays whole: {text:?}"
        );
        assert!(
            text.ends_with("ctrl+x / commands"),
            "one whole hint is left: {text:?}"
        );
        for dropped in ["ctrl+x more", "? keys"] {
            assert!(!text.contains(dropped), "{dropped:?} dropped: {text:?}");
        }
        // A hint is counted at the columns it draws in, so none arrives cut.
        for hint in HINTS {
            let drawn = UnicodeWidthStr::width(hint.keys) + 1 + UnicodeWidthStr::width(hint.words);
            assert_eq!(
                usize::from(hint_columns(hint)),
                drawn,
                "the width of {} {}",
                hint.keys,
                hint.words
            );
        }
    }

    /// A step of the bar draws a few bytes and allocates no block: the time is
    /// formatted into a buffer that keeps its room, and the note's buffer does
    /// too, so a frame of a run costs the cells it changed.
    #[test]
    fn moving_bar_step_allocates_nothing() {
        let mut app = app(COLUMNS);
        app.running = true;
        // The line's buffers warm on the frame that first draws them, and the
        // frame that writes them in full is outside the measurement.
        rewind(Duration::from_secs(59 * 60 + 59));
        frame(&mut app);
        let mut bytes = Vec::new();
        app.screen.diff(&mut bytes);
        let (note, time) = (app.status.capacity(), time_capacity());
        assert!(time > 0, "the elapsed time is written into a buffer");
        let mut drawn = Vec::new();
        for second in 1..=14u64 {
            rewind(Duration::from_secs(59 * 60 + 59 + second));
            frame(&mut app);
            bytes.clear();
            app.screen.diff(&mut bytes);
            drawn.push(bytes.len());
        }
        assert_eq!(
            time_capacity(),
            time,
            "a step allocates no block: the time buffer kept its room"
        );
        assert_eq!(
            app.status.capacity(),
            note,
            "the note's buffer kept its room"
        );
        let most = drawn.iter().copied().max().unwrap_or(0);
        eprintln!("moving bar: at most {most} bytes for a step, over {drawn:?}");
        assert!(most > 0, "the bar moved: {drawn:?}");
        assert!(
            most <= 64,
            "a step writes a few bytes, over {most}: {drawn:?}"
        );
    }

    /// The elapsed time reads in seconds under a minute and in minutes and two
    /// digits of seconds past it, into the buffer it is given.
    #[test]
    fn a_status_line_names_the_elapsed_time_in_seconds_and_minutes() {
        let mut time = String::from("room for the text it held before");
        for (gone, want) in [
            (0, "0s"),
            (12, "12s"),
            (59, "59s"),
            (60, "1m 00s"),
            (64, "1m 04s"),
            (3599, "59m 59s"),
        ] {
            write_time(&mut time, Duration::from_secs(gone));
            assert_eq!(time, want, "{gone} s");
        }
        // A pause keeps the time it froze on and says so.
        let mut motion = Motion::stepping();
        motion.paused = Some(Duration::from_secs(12));
        assert_eq!(motion.time(Instant::now()), "12s, paused");
    }
}

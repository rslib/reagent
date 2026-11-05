//! A panel written outside the crate, on the public slot API only.
//!
//! The panel below uses nothing but what `reagent_tui::slot` exports: it says
//! what its lines mean, and the sidebar places them, folds it by its priority
//! and draws it. The last test counts the heap blocks of a steady draw.
//!
//! dhat allows one profiler per process, so every test locks [`SERIAL`] for its
//! whole body: a test that ran beside a profiler would be counted in its
//! numbers.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dhat::{Alloc, HeapStats, Profiler};
use reagent_tui::slot::{Component, Lines, Panel, Priority, Sidebar, Size, Text, Update};

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Held by every test for its whole body, so one profiler runs at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many times a measurement runs; the smallest count is the work's own.
const SAMPLES: usize = 3;

/// The sidebar a test draws: narrow enough that its content is cut.
const WIDTH: u16 = 40;
const HEIGHT: u16 = 14;

/// A panel the crate has never seen: an id, a title, a priority and one text
/// line per component.
struct Example {
    id: &'static str,
    title: Text,
    priority: Priority,
    lines: Vec<Arc<str>>,
}

impl Example {
    /// A panel of `count` text lines, each naming the panel.
    fn new(id: &'static str, title: &'static str, priority: Priority, count: usize) -> Example {
        let lines = (0..count)
            .map(|index| Arc::from(format!("{id} line {index}")))
            .collect();
        Example {
            id,
            title: Text::Static(title),
            priority,
            lines,
        }
    }
}

impl Panel for Example {
    fn id(&self) -> &str {
        self.id
    }

    fn title(&self) -> &Text {
        &self.title
    }

    fn priority(&self) -> Priority {
        self.priority
    }

    fn min_width(&self) -> u16 {
        20
    }

    fn update(&mut self, _update: &Update<'_>) -> bool {
        false
    }

    fn draw(&self, size: Size, out: &mut Vec<Component>) {
        assert!(size.width >= self.min_width(), "the host honours min_width");
        for line in &self.lines {
            out.push(Component::text(Text::shared(line)));
        }
        out.push(Component::row("tokens", "50k / 200k"));
        out.push(Component::progress("context", 50, 200));
        out.push(Component::badge("live", reagent_tui::slot::Role::Good));
    }
}

/// The text of every line the sidebar drew, runs joined.
fn texts(lines: &Lines) -> Vec<String> {
    lines
        .iter()
        .map(|spans| {
            let mut text = String::new();
            for span in spans {
                text.push_str(span.text());
            }
            text
        })
        .collect()
}

#[test]
fn an_outside_panel_draws_in_the_sidebar() {
    let _serial = serial();
    let mut sidebar = Sidebar::new();
    sidebar.add(Example::new("example", "Example", 5, 2));
    assert_eq!(sidebar.len(), 1);
    assert_eq!(sidebar.focused().expect("a panel").id(), "example");
    let drawn = texts(sidebar.draw(WIDTH, HEIGHT));
    assert!(drawn[0].contains("Example"), "{drawn:?}");
    assert!(drawn.iter().any(|line| line.contains("example line 0")));
    assert!(drawn.iter().any(|line| line.contains("example line 1")));
    assert!(drawn.iter().any(|line| line.contains("50k / 200k")));
    assert!(drawn.iter().any(|line| line.contains("[live]")));
    for line in &drawn {
        assert!(
            line.chars().count() <= WIDTH as usize,
            "{line:?} is too wide"
        );
    }
}

#[test]
fn an_outside_panel_folds_by_priority_when_the_height_is_short() {
    let _serial = serial();
    let mut sidebar = Sidebar::new();
    sidebar.add(Example::new("first", "First", 9, 3));
    sidebar.add(Example::new("second", "Second", 1, 3));
    // Both expanded want four rows of content each, with a title above.
    let drawn = texts(sidebar.draw(WIDTH, 8));
    assert_eq!(drawn.len(), 8, "{drawn:?}");
    assert!(
        drawn.iter().any(|line| line.contains("first line 2")),
        "the higher priority panel kept its content: {drawn:?}"
    );
    assert!(
        !drawn.iter().any(|line| line.contains("second line 0")),
        "the lower priority panel folded to its title: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|line| line.contains("Second")),
        "a folded panel still shows its title: {drawn:?}"
    );
}

#[test]
fn a_steady_draw_allocates_nothing() {
    let _serial = serial();
    let mut sidebar = Sidebar::new();
    sidebar.add(Example::new("sessions", "Sessions", 9, 6));
    sidebar.add(Example::new("context", "Context", 3, 2));
    // Two draws give every buffer the capacity it needs.
    sidebar.draw(WIDTH, HEIGHT);
    sidebar.draw(WIDTH, HEIGHT);
    let mut least = u64::MAX;
    for _ in 0..SAMPLES {
        let profiler = Profiler::builder().testing().build();
        let drawn = sidebar.draw(WIDTH, HEIGHT).len();
        let stats = HeapStats::get();
        drop(profiler);
        assert!(drawn > 0, "a draw that stopped drawing fails here");
        least = least.min(stats.total_blocks);
    }
    eprintln!("blocks: {least} per steady sidebar draw of {WIDTH} by {HEIGHT}");
    assert_eq!(least, 0, "a steady draw allocates");
}

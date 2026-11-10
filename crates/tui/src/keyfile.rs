//! The keys file: `keys.toml` in the config folder moves any key of the TUI.
//!
//! The map, the raw key readers and the tables of hints are written for the
//! keys the table below names, so the file is read as a translation and not as
//! a second map: a key the file moves is turned into the first default key of
//! its action before any reader sees it, a default key some action still holds
//! is left as it is, and a default key every action holding it moved away from
//! is swallowed. The map itself never changes, so an app with no file does no
//! work per key and both draw and answer the same keys.
//!
//! The file is one `[section]` table per set of keys, with `name = "key"` or
//! `name = ["key", "key"]` under it:
//!
//! ```toml
//! [keys]
//! send = "ctrl+s"
//! up = ["up", "ctrl+k"]
//! [card]
//! once = "o"
//! ```
//!
//! A key name is `ctrl+`, `alt+` and `shift+` in any order, then one character
//! or one of `enter tab esc up down left right home end pgup pgdn backspace
//! delete space backtab f1`..`f12`, in any case. Alt+0-9 are the argument digits
//! and stay where they are.
//!
//! A line the file gets wrong is one note that names its line, and the other
//! lines still apply. Two moved actions of one section on one key, or a moved
//! action on a key an action that did not move still holds, are one note that
//! names the key and the section, and neither line applies: the action keeps
//! its own keys, which can free a key another line had moved onto, so the
//! clashes are found again until none is left.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::keys::{Choice, HINTS, STATUS};

/// One section of the file, and of the chain one key press is read in.
///
/// A section's rows are contiguous in its table, in the order the hints and the
/// translation read them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    /// The text box and the transcript, `[keys]`.
    Keys,
    /// `[keys]` without the text box's own keys: the quit and the leader, which
    /// are the keys a sheet leaves it.
    KeysQuitLeader,
    /// The key after the leader, `[leader]`.
    Leader,
    /// A list above the text box, `[list]`.
    List,
    /// A sheet in place of the text box, `[sheet]`.
    Sheet,
    /// A focused sidebar, `[sidebar]`.
    Sidebar,
    /// The approval card's row of answers, `[card]`.
    Card,
    /// The question card's rows, `[ask]`.
    Ask,
}

/// The sections the file has a table for, in the order the notes name them.
const WRITTEN: [Section; 7] = [
    Section::Keys,
    Section::Leader,
    Section::List,
    Section::Sheet,
    Section::Sidebar,
    Section::Card,
    Section::Ask,
];

impl Section {
    /// The rows of this section, in the order the hints show them.
    fn rows(self) -> &'static [Row] {
        match self {
            Section::Keys => KEYS,
            // The quit, the leader and the cancel are the first rows of the text
            // box's own table.
            Section::KeysQuitLeader => &KEYS[..3],
            Section::Leader => LEADER,
            Section::List => LIST,
            Section::Sheet => SHEET,
            Section::Sidebar => SIDEBAR,
            Section::Card => CARD,
            Section::Ask => ASK,
        }
    }

    /// The name the file's table header gives this section.
    fn name(self) -> &'static str {
        match self {
            Section::Keys | Section::KeysQuitLeader => "keys",
            Section::Leader => "leader",
            Section::List => "list",
            Section::Sheet => "sheet",
            Section::Sidebar => "sidebar",
            Section::Card => "card",
            Section::Ask => "ask",
        }
    }

    /// The section one table header names, or `None` for a name no section has.
    fn from_name(name: &str) -> Option<Section> {
        WRITTEN
            .into_iter()
            .find(|section| section.name().eq_ignore_ascii_case(name))
    }

    /// Whether a plain letter of this section reads in either case, which is
    /// what the leader's keys and the card's answers do.
    fn either_case(self) -> bool {
        matches!(self, Section::Leader | Section::Card)
    }
}

/// One action of the key map: its name in the file, and its own keys, comma
/// separated, in the order the file's first one means and the hints print.
struct Row {
    name: &'static str,
    keys: &'static str,
}

impl Row {
    /// The key an app key that moved is read as: the first of the action's own.
    fn first(&self) -> &'static str {
        self.keys.split(',').next().unwrap_or("")
    }
}

/// The text box and the transcript. The quit, the leader and the cancel come
/// first, which is the range a sheet leaves to them.
static KEYS: &[Row] = &[
    Row {
        name: "quit",
        keys: "ctrl+c",
    },
    Row {
        name: "leader",
        keys: "ctrl+x",
    },
    Row {
        name: "abort-search",
        keys: "ctrl+g",
    },
    Row {
        name: "send",
        keys: "enter,ctrl+m",
    },
    Row {
        name: "new-line",
        keys: "shift+enter,ctrl+enter,ctrl+j",
    },
    Row {
        name: "queue",
        keys: "alt+enter,tab",
    },
    Row {
        name: "cancel",
        keys: "esc",
    },
    Row {
        name: "redraw",
        keys: "ctrl+l",
    },
    Row {
        name: "left",
        keys: "left,ctrl+b",
    },
    Row {
        name: "right",
        keys: "right,ctrl+f",
    },
    Row {
        name: "up",
        keys: "up,ctrl+p",
    },
    Row {
        name: "down",
        keys: "down,ctrl+n",
    },
    Row {
        name: "home",
        keys: "home",
    },
    Row {
        name: "end",
        keys: "end",
    },
    Row {
        name: "backspace",
        keys: "backspace,ctrl+h",
    },
    Row {
        name: "delete",
        keys: "delete",
    },
    Row {
        name: "line-start",
        keys: "ctrl+a",
    },
    Row {
        name: "line-end",
        keys: "ctrl+e",
    },
    Row {
        name: "kill-end",
        keys: "ctrl+k",
    },
    Row {
        name: "kill-start",
        keys: "ctrl+u",
    },
    Row {
        name: "kill-word",
        keys: "ctrl+w",
    },
    Row {
        name: "forward-word",
        keys: "alt+f,ctrl+right,alt+right",
    },
    Row {
        name: "backward-word",
        keys: "alt+b,ctrl+left,alt+left",
    },
    Row {
        name: "delete-char",
        keys: "ctrl+d",
    },
    Row {
        name: "kill-word-forward",
        keys: "alt+d",
    },
    Row {
        name: "kill-word-backward",
        keys: "alt+backspace,ctrl+alt+h",
    },
    Row {
        name: "delete-space",
        keys: "alt+\\",
    },
    Row {
        name: "transpose",
        keys: "ctrl+t",
    },
    Row {
        name: "transpose-words",
        keys: "alt+t",
    },
    Row {
        name: "upcase",
        keys: "alt+u",
    },
    Row {
        name: "downcase",
        keys: "alt+l",
    },
    Row {
        name: "capitalize",
        keys: "alt+c",
    },
    Row {
        name: "yank",
        keys: "ctrl+y",
    },
    Row {
        name: "yank-pop",
        keys: "alt+y",
    },
    Row {
        name: "undo",
        keys: "ctrl+_,ctrl+/,ctrl+7",
    },
    Row {
        name: "mark",
        keys: "ctrl+space,ctrl+@",
    },
    Row {
        name: "quote",
        keys: "ctrl+q",
    },
    Row {
        name: "paste-image",
        keys: "ctrl+v",
    },
    Row {
        name: "char-search",
        keys: "ctrl+],ctrl+5",
    },
    Row {
        name: "char-search-backward",
        keys: "ctrl+alt+],ctrl+alt+5",
    },
    Row {
        name: "negative-argument",
        keys: "alt+-",
    },
    Row {
        name: "history-oldest",
        keys: "alt+<",
    },
    Row {
        name: "history-newest",
        keys: "alt+>",
    },
    Row {
        name: "yank-last-word",
        keys: "alt+.",
    },
    Row {
        name: "search-backward",
        keys: "ctrl+r",
    },
    Row {
        name: "search-forward",
        keys: "ctrl+s",
    },
    Row {
        name: "fold",
        keys: "tab",
    },
    Row {
        name: "open-all",
        keys: "ctrl+o",
    },
    Row {
        name: "page-up",
        keys: "pgup",
    },
    Row {
        name: "page-down",
        keys: "pgdn",
    },
    Row {
        name: "scroll-up",
        keys: "shift+up",
    },
    Row {
        name: "scroll-down",
        keys: "shift+down",
    },
    Row {
        name: "jump-down",
        keys: "end",
    },
];

/// The key after the leader.
static LEADER: &[Row] = &[
    Row {
        name: "sidebar",
        keys: "b",
    },
    Row {
        name: "sessions",
        keys: "l",
    },
    Row {
        name: "model",
        keys: "m",
    },
    Row {
        name: "level",
        keys: "t",
    },
    Row {
        name: "sidebar-focus",
        keys: "tab",
    },
    Row {
        name: "commands",
        keys: "/",
    },
    Row {
        name: "undo",
        keys: "ctrl+u",
    },
    Row {
        name: "exchange",
        keys: "ctrl+x",
    },
    Row {
        name: "close",
        keys: "esc",
    },
];

/// A list above the text box.
static LIST: &[Row] = &[
    Row {
        name: "up",
        keys: "up,ctrl+p",
    },
    Row {
        name: "down",
        keys: "down,ctrl+n",
    },
    Row {
        name: "complete",
        keys: "tab",
    },
    Row {
        name: "pick",
        keys: "enter",
    },
    Row {
        name: "close",
        keys: "esc",
    },
];

/// A sheet in place of the text box.
static SHEET: &[Row] = &[
    Row {
        name: "up",
        keys: "up,ctrl+p",
    },
    Row {
        name: "down",
        keys: "down,ctrl+n",
    },
    Row {
        name: "page-up",
        keys: "pgup",
    },
    Row {
        name: "page-down",
        keys: "pgdn",
    },
    Row {
        name: "complete",
        keys: "tab",
    },
    Row {
        name: "pick",
        keys: "enter",
    },
    Row {
        name: "close",
        keys: "esc",
    },
    Row {
        name: "delete",
        keys: "ctrl+d",
    },
    Row {
        name: "backspace",
        keys: "backspace",
    },
    Row {
        name: "kill-word-backward",
        keys: "alt+backspace",
    },
];

/// A focused sidebar, with the page keys of the sessions panel.
static SIDEBAR: &[Row] = &[
    Row {
        name: "next",
        keys: "tab",
    },
    Row {
        name: "previous",
        keys: "shift+tab",
    },
    Row {
        name: "up",
        keys: "up",
    },
    Row {
        name: "down",
        keys: "down",
    },
    Row {
        name: "open",
        keys: "enter",
    },
    Row {
        name: "leave",
        keys: "esc",
    },
    Row {
        name: "page-back",
        keys: "left",
    },
    Row {
        name: "page-next",
        keys: "right",
    },
];

/// The approval card's row of answers.
static CARD: &[Row] = &[
    Row {
        name: "once",
        keys: "y",
    },
    Row {
        name: "always",
        keys: "a",
    },
    Row {
        name: "no",
        keys: "n",
    },
    Row {
        name: "tell",
        keys: "t",
    },
    Row {
        name: "left",
        keys: "left",
    },
    Row {
        name: "right",
        keys: "right",
    },
    Row {
        name: "answer",
        keys: "enter",
    },
    Row {
        name: "refuse",
        keys: "esc",
    },
];

/// The question card's keys: the pick, the answer, the next question and the
/// skip.
static ASK: &[Row] = &[
    Row {
        name: "up",
        keys: "up,ctrl+p",
    },
    Row {
        name: "down",
        keys: "down,ctrl+n",
    },
    Row {
        name: "answer",
        keys: "enter",
    },
    Row {
        name: "next",
        keys: "tab",
    },
    Row {
        name: "toggle",
        keys: "space",
    },
    Row {
        name: "skip",
        keys: "esc",
    },
    Row {
        name: "new-line",
        keys: "ctrl+j,shift+enter",
    },
];

/// The chain of a key press with nothing else on screen.
pub const KEYS_CHAIN: &[Section] = &[Section::Keys];
/// The chain of the key that follows the leader.
pub const LEADER_CHAIN: &[Section] = &[Section::Leader, Section::Keys];
/// The chain of a key press while an approval card takes its answers.
pub const CARD_CHAIN: &[Section] = &[Section::Card, Section::Keys];
/// The chain of a key press while a question card takes its answers.
pub const ASK_CHAIN: &[Section] = &[Section::Ask, Section::Keys];
/// The chain of a key press while a sheet is open: the sheet's keys, then only
/// the quit and the leader of the text box's.
pub const SHEET_CHAIN: &[Section] = &[Section::Sheet, Section::KeysQuitLeader];
/// The chain of a key press while a list is open.
pub const LIST_CHAIN: &[Section] = &[Section::List, Section::Keys];
/// The chain of a key press while the sidebar has the keys.
pub const SIDEBAR_CHAIN: &[Section] = &[Section::Sidebar, Section::Keys];

/// One key as the file or a table writes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    modifiers: KeyModifiers,
    code: KeyCode,
}

impl Key {
    /// The key one name gives: `ctrl+`, `alt+` and `shift+` in any order, then
    /// one character or one named key. Allocates nothing.
    fn parse(name: &str) -> Option<Key> {
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = name;
        loop {
            match (
                prefix(rest, "ctrl+"),
                prefix(rest, "alt+"),
                prefix(rest, "shift+"),
            ) {
                (Some(after), _, _) => {
                    modifiers.insert(KeyModifiers::CONTROL);
                    rest = after;
                }
                (_, Some(after), _) => {
                    modifiers.insert(KeyModifiers::ALT);
                    rest = after;
                }
                (_, _, Some(after)) => {
                    modifiers.insert(KeyModifiers::SHIFT);
                    rest = after;
                }
                (None, None, None) => break,
            }
        }
        let mut code = named(rest)?;
        // A terminal reports Shift on a letter as the capital itself.
        if modifiers == KeyModifiers::SHIFT
            && let KeyCode::Char(c) = code
            && c.is_ascii_alphabetic()
        {
            code = KeyCode::Char(c.to_ascii_uppercase());
            modifiers = KeyModifiers::NONE;
        }
        Some(Key { modifiers, code })
    }

    /// The event a terminal sends for this key.
    fn event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    /// Whether two keys are one key of `section`, which reads the case of a
    /// letter the way the translation does.
    fn same_key(self, other: Key, section: Section) -> bool {
        self.matches(&other.event(), section) && other.matches(&self.event(), section)
    }

    /// Whether `event` presses this key in `section`.
    ///
    /// A letter with Ctrl or Alt, and a plain letter of the leader's keys or the
    /// card's answers, matches in either case, and Shift+Tab and the terminal's
    /// BackTab are one key.
    fn matches(self, event: &KeyEvent, section: Section) -> bool {
        let shift_tab = (self.code == KeyCode::Tab && self.modifiers.contains(KeyModifiers::SHIFT))
            || self.code == KeyCode::BackTab;
        if shift_tab {
            let backtab = event.code == KeyCode::BackTab
                || (event.code == KeyCode::Tab && event.modifiers.contains(KeyModifiers::SHIFT));
            return backtab && same(self.modifiers, event.modifiers, true);
        }
        let letter = match self.code {
            KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                let modified = self
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if modified || (self.modifiers.is_empty() && section.either_case()) {
                    Some(c)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(want) = letter {
            let KeyCode::Char(got) = event.code else {
                return false;
            };
            // A letter's own case carries Shift, which neither the map nor the
            // card's answers read.
            return got.eq_ignore_ascii_case(&want) && same(self.modifiers, event.modifiers, true);
        }
        // A character carries its own Shift, which a terminal may report as well.
        let ignore_shift = matches!(self.code, KeyCode::Char(_));
        same(self.modifiers, event.modifiers, ignore_shift) && self.code == event.code
    }
}

/// `text` without `prefix`, in either case, and `None` when it does not start
/// with it. A byte cut is a prefix of no text.
fn prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

/// Whether two sets of modifiers read the same, Shift left out for the keys
/// whose own case carries it.
fn same(a: KeyModifiers, b: KeyModifiers, ignore_shift: bool) -> bool {
    a.contains(KeyModifiers::CONTROL) == b.contains(KeyModifiers::CONTROL)
        && a.contains(KeyModifiers::ALT) == b.contains(KeyModifiers::ALT)
        && (ignore_shift || a.contains(KeyModifiers::SHIFT) == b.contains(KeyModifiers::SHIFT))
}

/// The code one key name gives after its modifiers, or `None` for a name no key
/// has.
fn named(name: &str) -> Option<KeyCode> {
    let words = [
        ("enter", KeyCode::Enter),
        ("tab", KeyCode::Tab),
        ("backtab", KeyCode::BackTab),
        ("esc", KeyCode::Esc),
        ("up", KeyCode::Up),
        ("down", KeyCode::Down),
        ("left", KeyCode::Left),
        ("right", KeyCode::Right),
        ("home", KeyCode::Home),
        ("end", KeyCode::End),
        ("pgup", KeyCode::PageUp),
        ("pgdn", KeyCode::PageDown),
        ("backspace", KeyCode::Backspace),
        ("delete", KeyCode::Delete),
        ("space", KeyCode::Char(' ')),
    ];
    if let Some((_, code)) = words
        .into_iter()
        .find(|(word, _)| name.eq_ignore_ascii_case(word))
    {
        return Some(code);
    }
    // The function keys: `f1` to `f12`.
    if let Some(digits) = prefix(name, "f")
        && let Ok(number) = digits.parse::<u8>()
        && (1..=12).contains(&number)
        && digits.len() == number.to_string().len()
    {
        return Some(KeyCode::F(number));
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(KeyCode::Char(c)),
        // A backslash is written twice in a TOML string, and once in a literal.
        (Some('\\'), Some('\\')) => Some(KeyCode::Char('\\')),
        _ => None,
    }
}

/// A row the file moved: the section, the row of its table, and the keys.
#[derive(Debug)]
struct Moved {
    section: Section,
    row: u8,
    keys: Box<[Key]>,
}

/// The table a keys file answers, and the translation one key press takes.
#[derive(Debug, Default)]
pub struct KeyFile {
    /// The rows the file moved, in the order the file gives them.
    moved: Vec<Moved>,
    /// The keys every hint, status hint and card choice shows, printed once,
    /// so the frame that draws them allocates nothing.
    labels: Labels,
    /// The notes the file left, one per problem.
    notes: Vec<String>,
}

/// The keys a frame shows, printed once when the file is read: the app draws
/// them in place of `Hint::keys` where the file moved one of them.
#[derive(Debug, Default)]
pub struct Labels {
    /// One per hint of [`crate::keys::HINTS`], in its order.
    hints: Box<[String]>,
    /// One per hint of [`crate::keys::STATUS`], in its order.
    status: Box<[String]>,
    /// The leader's own key.
    leader: String,
    /// One per choice of [`crate::keys::Choice`], in its order.
    choices: Vec<String>,
    /// The first key that sends, for the box's footer.
    send: String,
    /// The first key that queues, for the box's footer.
    queue: String,
    /// The placeholder of an empty box while a turn runs.
    busy: String,
    /// The first key of the sheet's delete, for the question it asks.
    delete: String,
    /// The keys that answer the approval card, for the status line.
    answer: String,
    /// The keys that pick and answer a question, for its card's hint row.
    ask: String,
    /// The keys that edit and send a typed answer, for the card's hint row.
    ask_text: String,
    /// One per foot of [`Foot`], in its order.
    feet: Box<[String]>,
    /// One per row of the command table, in its order.
    commands: Box<[String]>,
}

impl Labels {
    /// The keys one hint of [`crate::keys::HINTS`] shows, by its place there.
    pub fn hint(&self, index: usize) -> &str {
        self.hints.get(index).map_or("", |label| label)
    }

    /// The keys one hint of [`crate::keys::STATUS`] shows, by its place there.
    pub fn status(&self, index: usize) -> &str {
        self.status.get(index).map_or("", |label| label)
    }

    /// The key the leader waits for.
    pub fn leader(&self) -> &str {
        &self.leader
    }

    /// The first key that sends.
    pub fn send(&self) -> &str {
        &self.send
    }

    /// The first key that queues.
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// The placeholder of an empty box while a turn runs.
    pub fn busy(&self) -> &str {
        &self.busy
    }

    /// The key of one answer of the approval card.
    pub fn choice(&self, choice: Choice) -> &str {
        self.choices.get(choice.index()).map_or("", |label| label)
    }

    /// The words of one foot, with the keys the file moved in it.
    pub fn foot(&self, foot: Foot) -> &str {
        self.feet.get(foot.index()).map_or("", |text| text)
    }

    /// The first key of the sheet's delete.
    pub fn delete(&self) -> &str {
        &self.delete
    }

    /// The keys that move through the card's answers and pick one.
    pub fn answer(&self) -> &str {
        &self.answer
    }

    /// The keys that pick a row of a question card and answer the question.
    pub fn ask(&self) -> &str {
        &self.ask
    }

    /// The keys that edit a typed answer and send it.
    pub fn ask_text(&self) -> &str {
        &self.ask_text
    }

    /// The key one row of the command table shows, by its place there.
    pub fn command(&self, index: usize) -> &str {
        self.commands.get(index).map_or("", |key| key)
    }
}

/// One row of words a frame draws with a key in it: the footer of a list, the
/// hint or the footer of a sheet, or the interrupt of the status line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Foot {
    /// The footer of the `/` list.
    ListCommands,
    /// The footer of the palette.
    ListPalette,
    /// The footer of the file list.
    ListFiles,
    /// The footer of the which-key list.
    ListLeader,
    /// The footer of the model sheet.
    Model,
    /// The footer of the level sheet.
    Level,
    /// The footer of the sessions sheet.
    Sessions,
    /// The hint of the sessions sheet's title row.
    SessionsHint,
    /// The footer of the key help sheet.
    Help,
    /// The footer of the login sheet, while no login runs.
    Login,
    /// The footer of the login sheet, while a login runs.
    LoginWaiting,
    /// The hint of the login sheet's title row.
    LoginHint,
    /// The interrupt of the status line.
    Interrupt,
    /// The hint of the sidebar's folded header row.
    SidebarHint,
}

impl Foot {
    /// Every foot, in the order [`Labels`] holds them.
    pub const ALL: [Foot; 14] = [
        Foot::ListCommands,
        Foot::ListPalette,
        Foot::ListFiles,
        Foot::ListLeader,
        Foot::Model,
        Foot::Level,
        Foot::Sessions,
        Foot::SessionsHint,
        Foot::Help,
        Foot::Login,
        Foot::LoginWaiting,
        Foot::LoginHint,
        Foot::Interrupt,
        Foot::SidebarHint,
    ];

    /// The foot's place in [`Labels`].
    fn index(self) -> usize {
        self as usize
    }

    /// The words of the foot, with each key it names in braces.
    fn text(self) -> &'static str {
        match self {
            Foot::ListCommands => {
                "\u{2191}\u{2193} choose \u{b7} {list:complete} completes \u{b7} {list:pick} runs \u{b7} {list:close} closes"
            }
            Foot::ListPalette => {
                "{keys:leader} {leader:commands} closes \u{b7} your draft stays in the box"
            }
            Foot::ListFiles => {
                " files \u{b7} {list:complete} inserts the path \u{b7} {list:close} closes"
            }
            Foot::ListLeader => "{leader:close} cancels \u{b7} ? shows every key",
            Foot::Model => {
                "\u{2191}\u{2193} choose \u{b7} {sheet:pick} switches the next turn \u{b7} {sheet:close} back to your draft"
            }
            Foot::Level => {
                "\u{2191}\u{2193} choose \u{b7} {sheet:pick} applies from the next turn \u{b7} {sheet:close} back to your draft"
            }
            Foot::Sessions => {
                "\u{2191}\u{2193} choose \u{b7} {sheet:pick} opens \u{b7} {sheet:delete} deletes \u{b7} {sheet:close} back to your draft"
            }
            Foot::SessionsHint => "{sheet:pick} opens",
            Foot::LoginHint => "{sheet:pick} starts a login",
            Foot::Help => {
                "{sheet:close} closes \u{b7} {keys:leader} {leader:commands} runs any command by name"
            }
            Foot::Login => {
                "\u{2191}\u{2193} choose \u{b7} {sheet:pick} logs in \u{b7} {sheet:close} back to your draft"
            }
            Foot::LoginWaiting => {
                "{sheet:close} stops waiting \u{b7} the login goes on in the browser"
            }
            Foot::Interrupt => "{keys:cancel} interrupt",
            Foot::SidebarHint => "{keys:leader} {leader:sidebar} panels",
        }
    }
}

/// One line of the file, before the clashes are settled.
struct Line {
    section: Section,
    row: usize,
    keys: Vec<Key>,
    /// The line's number, from 1.
    number: usize,
}

impl KeyFile {
    /// Reads a keys file's text, and answers the table with the notes of the
    /// lines it could not use.
    pub fn parse(text: &str) -> (KeyFile, Vec<String>) {
        let mut file = KeyFile::default();
        let mut section = None;
        let mut lines: Vec<Line> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = Section::from_name(name.trim());
                if section.is_none() {
                    file.note(format!("line {number}: no [{}] section", name.trim()));
                }
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                file.note(format!("line {number}: {line:?} is not name = value"));
                continue;
            };
            let name = name.trim();
            let Some(section) = section else {
                file.note(format!("line {number}: no [section] before {name}"));
                continue;
            };
            let rows = section.rows();
            let Some(row) = rows
                .iter()
                .position(|row| row.name.eq_ignore_ascii_case(name))
            else {
                file.note(format!("line {number}: [{}] has no {name}", section.name()));
                continue;
            };
            if lines
                .iter()
                .any(|line| line.section == section && line.row == row)
            {
                file.note(format!("line {number}: {} is set twice", rows[row].name));
                continue;
            }
            let Some(keys) = keys(value, number, rows[row].name, &mut file.notes) else {
                continue;
            };
            lines.push(Line {
                section,
                row,
                keys,
                number,
            });
        }
        file.settle(&mut lines);
        file.moved = lines
            .iter()
            .map(|line| Moved {
                section: line.section,
                row: line.row as u8,
                keys: line.keys.iter().copied().collect(),
            })
            .collect();
        let queue = label("keys:queue", "", &file.moved);
        // The arrows stay one glyph pair until a file moves one of them.
        let arrows_moved = file.moved.iter().any(|moved| {
            moved.section == Section::Card
                && matches!(
                    Section::Card.rows()[usize::from(moved.row)].name,
                    "left" | "right"
                )
        });
        let answer = if arrows_moved {
            "{card:left} {card:right} {card:answer}"
        } else {
            "\u{2190}\u{2192} {card:answer}"
        };
        file.labels = Labels {
            delete: label("sheet:delete", "", &file.moved),
            answer: labelled(answer, &file.moved),
            ask: labelled(
                "1-9 or {ask:up} {ask:down} pick \u{b7} type to answer \u{b7} {ask:answer} answers \u{b7} {ask:next} next question \u{b7} {ask:skip} skips",
                &file.moved,
            ),
            ask_text: labelled(
                "{ask:answer} sends your answer \u{b7} {ask:new-line} new line \u{b7} {ask:skip} clears",
                &file.moved,
            ),
            feet: Foot::ALL
                .iter()
                .map(|foot| labelled(foot.text(), &file.moved))
                .collect(),
            commands: COMMANDS
                .iter()
                .map(|text| labelled(text, &file.moved))
                .collect(),
            send: label("keys:send", "", &file.moved),
            busy: format!("Steer the run \u{b7} {queue} queues for the next turn"),
            queue,
            hints: HINTS
                .iter()
                .map(|hint| label(hint.acts, hint.keys, &file.moved))
                .collect(),
            status: STATUS
                .iter()
                .map(|hint| label(hint.acts, hint.keys, &file.moved))
                .collect(),
            leader: label("keys:leader", "", &file.moved),
            choices: CHOICES
                .iter()
                .map(|action| {
                    let mut out = String::new();
                    push_key(Section::Card, action, 0, &file.moved, &mut out);
                    out
                })
                .collect(),
        };
        let notes = std::mem::take(&mut file.notes);
        (file, notes)
    }

    /// The keys the hints and the card show.
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Adds one note.
    fn note(&mut self, note: String) {
        self.notes.push(note);
    }

    /// Drops the lines whose keys clash until none is left, and notes each
    /// clash it found: a dropped line gives its action its own keys back, which
    /// can put one of them under another line.
    fn settle(&mut self, lines: &mut Vec<Line>) {
        while let Some(found) = clash(lines) {
            match found {
                Clash::Lines(first, second, key) => {
                    let (a, b) = (&lines[first], &lines[second]);
                    self.note(format!(
                        "line {} and line {}: {} is one key of {} and {} in [{}]",
                        a.number,
                        b.number,
                        key_name(key),
                        a.section.rows()[a.row].name,
                        b.section.rows()[b.row].name,
                        a.section.name()
                    ));
                    lines.remove(second);
                    lines.remove(first);
                }
                Clash::Held(first, held, key) => {
                    let line = &lines[first];
                    self.note(format!(
                        "line {}: {} is one key of {} in [{}], which did not move",
                        line.number,
                        key_name(key),
                        held,
                        line.section.name()
                    ));
                    lines.remove(first);
                }
            }
        }
    }

    /// The keys one row of one section holds now, or `None` for the table's own.
    fn moved(&self, section: Section, row: usize) -> Option<&[Key]> {
        let row = u8::try_from(row).ok()?;
        // The file writes the quit and the leader under `[keys]`.
        let section = match section {
            Section::KeysQuitLeader => Section::Keys,
            section => section,
        };
        self.moved
            .iter()
            .find(|moved| moved.section == section && moved.row == row)
            .map(|moved| &*moved.keys)
    }

    /// Translates one key press for what is on screen, `chain` first:
    /// `Some(key)` is the key the map and the raw readers take, and `None` is a
    /// key the file swallowed.
    ///
    /// For each section, in order: a key of an action that moved reads as the
    /// first key that action has; a key an action that did not move still holds
    /// reads as it is; a key every action that held it moved away from is
    /// swallowed; any other key goes to the next section, and the end of the
    /// chain reads it as it is. Allocates nothing.
    pub fn translate(&self, chain: &[Section], event: KeyEvent) -> Option<KeyEvent> {
        let mut out = event;
        for &section in chain {
            match self.step(section, &out) {
                Step::Bound(key) => {
                    out.code = key.code;
                    out.modifiers = key.modifiers;
                    return Some(out);
                }
                Step::Keep => return Some(out),
                Step::Freed => return None,
                Step::Next => {}
            }
        }
        Some(out)
    }

    /// One section of the chain, for one key press.
    fn step(&self, section: Section, event: &KeyEvent) -> Step {
        let mut keep = false;
        let mut freed = false;
        for (index, row) in section.rows().iter().enumerate() {
            let moved = self.moved(section, index);
            if let Some(keys) = moved
                && keys.iter().any(|key| key.matches(event, section))
                && let Some(key) = Key::parse(row.first())
            {
                return Step::Bound(key);
            }
            let holds = row
                .keys
                .split(',')
                .filter_map(Key::parse)
                .any(|key| key.matches(event, section));
            match (moved.is_some(), holds) {
                (false, true) => keep = true,
                (true, true) => freed = true,
                _ => {}
            }
        }
        if keep {
            Step::Keep
        } else if freed {
            Step::Freed
        } else {
            Step::Next
        }
    }
}

/// What one section of the chain says about a key press.
enum Step {
    /// A moved action's key: read as this one.
    Bound(Key),
    /// A key an action that did not move holds: read as it is.
    Keep,
    /// A default key every action that held it moved away from.
    Freed,
    /// No row of the section knows the key.
    Next,
}

/// The keys of one `name = value` line, or `None` after a note that names the
/// line and what was wrong with it.
fn keys(value: &str, number: usize, action: &str, notes: &mut Vec<String>) -> Option<Vec<Key>> {
    let value = value.trim();
    let mut names = Vec::new();
    match value.strip_prefix('[') {
        Some(rest) => {
            let Some(inner) = rest.strip_suffix(']') else {
                notes.push(format!("line {number}: {action} takes one key or a list"));
                return None;
            };
            for item in inner.split(',') {
                let item = unquote(item.trim());
                if item.is_empty() {
                    notes.push(format!("line {number}: {action} has no keys"));
                    return None;
                }
                names.push(item);
            }
        }
        None => names.push(unquote(value)),
    }
    let mut keys = Vec::with_capacity(names.len());
    for name in names {
        let Some(key) = Key::parse(name) else {
            notes.push(format!("line {number}: {name:?} is no key name"));
            return None;
        };
        keys.push(key);
    }
    Some(keys)
}

/// The first clash of the lines, in table order.
enum Clash {
    /// Two lines of one section with one key, at their places in the lines.
    Lines(usize, usize, Key),
    /// A line on a key an action that did not move still holds, named.
    Held(usize, &'static str, Key),
}

/// The first clash of the lines.
fn clash(lines: &[Line]) -> Option<Clash> {
    for (first, line) in lines.iter().enumerate() {
        for (second, other) in lines.iter().enumerate().skip(first + 1) {
            if line.section != other.section {
                continue;
            }
            if let Some(key) = line.keys.iter().find(|key| {
                other
                    .keys
                    .iter()
                    .any(|other| key.same_key(*other, line.section))
            }) {
                return Some(Clash::Lines(first, second, *key));
            }
        }
    }
    // A line on a key an action that did not move still holds.
    for (index, line) in lines.iter().enumerate() {
        for (row, table) in line.section.rows().iter().enumerate() {
            let moved = lines
                .iter()
                .any(|other| other.section == line.section && other.row == row);
            if row == line.row || moved {
                continue;
            }
            if let Some(key) = table.keys.split(',').filter_map(Key::parse).find(|key| {
                line.keys
                    .iter()
                    .any(|other| key.same_key(*other, line.section))
            }) {
                return Some(Clash::Held(index, table.name, key));
            }
        }
    }
    None
}

/// The action names of the card's answers, in the order of
/// [`crate::keys::Choice::ALL`].
const CHOICES: [&str; 4] = ["once", "always", "no", "tell"];

/// The keys the rows of the command table show, in the order the table lists
/// them, with each key it names in braces: a row with no key is empty.
const COMMANDS: [&str; 11] = [
    "{keys:leader} {leader:model}",
    "{keys:leader} {leader:level}",
    "",
    "{keys:leader} {leader:sessions}",
    "",
    "",
    "",
    "",
    "",
    "?",
    "{keys:quit} {keys:quit}",
];

/// The keys one hint shows: the whole list it prints when it names no action,
/// and otherwise the first key in force of each action it names, with a token
/// that has no section printed as it is. Built on the heap once, when the file
/// is read.
fn label(acts: &str, keys: &str, moved: &[Moved]) -> String {
    if acts.is_empty() {
        return keys.to_owned();
    }
    let mut out = String::new();
    push_acts(acts, moved, &mut out);
    out
}

/// The words of a foot: each `{section:action}` in `text` written as the key in
/// force, and every other char as it is. Built on the heap once, when the file
/// is read.
fn labelled(text: &str, moved: &[Moved]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((head, tail)) = rest.split_once('{') {
        out.push_str(head);
        match tail.split_once('}') {
            Some((acts, tail)) => {
                push_acts(acts, moved, &mut out);
                rest = tail;
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Writes each act of `acts`, one space apart: the first key in force of an
/// action, with a token that has no section printed as it is.
fn push_acts(acts: &str, moved: &[Moved], out: &mut String) {
    let mut first = true;
    for token in acts.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        match token
            .split_once(':')
            .and_then(|(name, action)| Some((Section::from_name(name)?, action)))
        {
            Some((section, action)) => {
                // `section:action.1` is the second key of the action, and the
                // last one when the file left it fewer.
                let (action, at) = match action.split_once('.') {
                    Some((action, at)) => (action, at.parse().unwrap_or(0)),
                    None => (action, 0),
                };
                push_key(section, action, at, moved, out);
            }
            None => out.push_str(token),
        }
    }
}

/// Writes the key at `at` of the keys one action holds now, or its last one:
/// the file's, or the action's own, printed as the table prints it.
fn push_key(section: Section, action: &str, at: usize, moved: &[Moved], out: &mut String) {
    let rows = section.rows();
    let Some(row) = rows.iter().position(|row| row.name == action) else {
        return;
    };
    match moved
        .iter()
        .find(|entry| entry.section == section && entry.row as usize == row)
    {
        Some(entry) => {
            if let Some(key) = entry.keys.get(at).or(entry.keys.last()) {
                out.push_str(&key_name(*key));
            }
        }
        None => {
            let keys = rows[row].keys;
            if let Some(name) = keys
                .split(',')
                .nth(at)
                .or_else(|| keys.split(',').next_back())
            {
                out.push_str(name);
            }
        }
    }
}

/// One key as the file names it, built on the heap: for a note or a label.
fn key_name(key: Key) -> String {
    let mut out = String::new();
    for (flag, name) in [
        (KeyModifiers::CONTROL, "ctrl+"),
        (KeyModifiers::ALT, "alt+"),
        (KeyModifiers::SHIFT, "shift+"),
    ] {
        if key.modifiers.contains(flag) {
            out.push_str(name);
        }
    }
    let named = [
        (KeyCode::Enter, "enter"),
        (KeyCode::Tab, "tab"),
        (KeyCode::BackTab, "backtab"),
        (KeyCode::Esc, "esc"),
        (KeyCode::Up, "up"),
        (KeyCode::Down, "down"),
        (KeyCode::Left, "left"),
        (KeyCode::Right, "right"),
        (KeyCode::Home, "home"),
        (KeyCode::End, "end"),
        (KeyCode::PageUp, "pgup"),
        (KeyCode::PageDown, "pgdn"),
        (KeyCode::Backspace, "backspace"),
        (KeyCode::Delete, "delete"),
    ];
    match key.code {
        KeyCode::Char(' ') => out.push_str("space"),
        KeyCode::Char(c) => out.push(c),
        KeyCode::F(number) => out.push_str(&format!("f{number}")),
        code => match named.into_iter().find(|(named, _)| *named == code) {
            Some((_, name)) => out.push_str(name),
            None => out.push('?'),
        },
    }
    out
}

/// `line` without its comment. A `#` inside a quoted value starts no comment, so
/// the key `"#"` stays whole.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match character {
            '"' | '\'' => {
                if quote == Some(character) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(character);
                }
            }
            '#' if quote.is_none() => return &line[..index],
            _ => {}
        }
    }
    line
}

/// A value without its quotes; a value with no quotes is taken as it is, so a
/// missing pair still reaches the key check that names it.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of `text` and its notes.
    fn table(text: &str) -> (KeyFile, Vec<String>) {
        KeyFile::parse(text)
    }

    /// The card's answer keys read as the arrows until a file moves one.
    #[test]
    fn keys_in_force_answer_keeps_the_arrows_until_one_moves() {
        let (file, _) = table("");
        assert_eq!(file.labels().answer(), "\u{2190}\u{2192} enter");
        let (file, _) = table("[card]\nanswer = \"f3\"\n");
        assert_eq!(file.labels().answer(), "\u{2190}\u{2192} f3");
        let (file, _) = table("[card]\nleft = \"h\"\n");
        assert_eq!(file.labels().answer(), "h right enter");
    }

    /// Every foot keeps its place, and no two feet print the same words, so a
    /// frame that asks for one cannot get another's.
    #[test]
    fn every_foot_keeps_its_place() {
        let (file, notes) = table("");
        assert!(notes.is_empty(), "{notes:?}");
        let mut seen = Vec::new();
        for (index, foot) in Foot::ALL.iter().enumerate() {
            assert_eq!(foot.index(), index, "{foot:?}");
            let words = file.labels().foot(*foot);
            assert!(!words.contains('{'), "{foot:?} keeps a token: {words:?}");
            assert!(!seen.contains(&words), "{foot:?} repeats {words:?}");
            seen.push(words);
        }
    }

    /// The event of one key name, as a terminal reports it.
    fn event(name: &str) -> KeyEvent {
        let key = Key::parse(name).expect("a key name");
        KeyEvent::new(key.code, key.modifiers)
    }

    /// The code and the modifiers of the key `chain` reads `name` as.
    fn read(file: &KeyFile, chain: &[Section], name: &str) -> (KeyCode, KeyModifiers) {
        let event = file
            .translate(chain, event(name))
            .unwrap_or_else(|| panic!("{name} is swallowed"));
        (event.code, event.modifiers)
    }

    #[test]
    fn a_sheet_reads_the_moved_quit_and_leader() {
        let (file, notes) =
            table("[keys]\nquit = \"ctrl+g\"\nleader = \"ctrl+z\"\nabort-search = \"alt+g\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            read(&file, SHEET_CHAIN, "ctrl+g"),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            read(&file, SHEET_CHAIN, "ctrl+z"),
            (KeyCode::Char('x'), KeyModifiers::CONTROL)
        );
        assert!(file.translate(SHEET_CHAIN, event("ctrl+c")).is_none());
    }

    #[test]
    fn a_capital_matches_as_a_terminal_reports_it() {
        let (file, notes) = table("[keys]\nkill-end = \"K\"\nyank = \"shift+y\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        let shifted = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT);
        let kill = file.translate(KEYS_CHAIN, shifted('K')).expect("bound");
        assert_eq!(kill.code, KeyCode::Char('k'));
        let yank = file.translate(KEYS_CHAIN, shifted('Y')).expect("bound");
        assert_eq!(yank.code, KeyCode::Char('y'));
        let plain = file.translate(KEYS_CHAIN, event("k")).expect("text");
        assert_eq!(plain.code, KeyCode::Char('k'));
        assert_eq!(plain.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn ctrl_m_moves_with_send() {
        let (file, notes) = table("[keys]\nsend = \"alt+s\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        assert!(file.translate(KEYS_CHAIN, event("ctrl+m")).is_none());
        assert!(file.translate(KEYS_CHAIN, event("enter")).is_none());
    }

    #[test]
    fn a_table_with_no_moved_row_rebuilds_the_keys_of_every_hint() {
        // The recipes of the hints answer the table they are written for, in the
        // order and the words of every one of them.
        let (file, notes) = table("");
        assert!(notes.is_empty(), "{notes:?}");
        let labels = file.labels();
        for (index, hint) in HINTS.iter().enumerate() {
            assert_eq!(labels.hint(index), hint.keys, "{}", hint.words);
        }
        for (index, hint) in STATUS.iter().enumerate() {
            assert_eq!(labels.status(index), hint.keys, "{}", hint.words);
        }
        assert_eq!(labels.leader(), "ctrl+x");
        for (choice, key) in Choice::ALL.into_iter().zip(["y", "a", "n", "t"]) {
            assert_eq!(labels.choice(choice), key);
        }
    }

    #[test]
    fn a_moved_action_shows_the_key_it_moved_to() {
        let (file, notes) = table(concat!(
            "[keys]\n",
            "send = \"ctrl+s\"\n",
            "search-forward = \"alt+s\"\n",
            "leader = \"alt+x\"\n",
            "[leader]\n",
            "commands = \"c\"\n",
            "[card]\n",
            "once = \"o\"\n",
        ));
        assert!(notes.is_empty(), "{notes:?}");
        let labels = file.labels();
        assert_eq!(labels.hint(0), "ctrl+s", "the send row");
        assert_eq!(labels.hint(3), "alt+x c", "the leader and its command key");
        assert_eq!(labels.hint(17), "alt+0-9 ctrl+space", "the keys it prints");
        assert_eq!(labels.hint(31), "c", "the leader's own command key");
        assert_eq!(labels.hint(26), "b", "a key the file left where it was");
        assert_eq!(labels.status(0), "alt+x c");
        assert_eq!(labels.status(1), "alt+x");
        assert_eq!(labels.leader(), "alt+x");
        assert_eq!(labels.choice(Choice::Once), "o");
        assert_eq!(labels.choice(Choice::Tell), "t");
    }

    #[test]
    fn a_key_name_is_modifiers_then_one_key() {
        assert_eq!(
            Key::parse("ctrl+s"),
            Some(Key {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('s')
            })
        );
        assert_eq!(
            Key::parse("Ctrl+Alt+]").map(|key| key.code),
            Some(KeyCode::Char(']'))
        );
        assert_eq!(Key::parse("f12").map(|key| key.code), Some(KeyCode::F(12)));
        assert_eq!(Key::parse("F5").map(|key| key.code), Some(KeyCode::F(5)));
        assert_eq!(
            Key::parse("alt+\\").map(|key| key.code),
            Some(KeyCode::Char('\\'))
        );
        assert_eq!(
            Key::parse("space").map(|key| key.code),
            Some(KeyCode::Char(' '))
        );
        assert_eq!(
            Key::parse("backtab").map(|key| key.code),
            Some(KeyCode::BackTab)
        );
        for name in ["", "control+s", "ctrl+", "f13", "f0", "enterx", "ctrl+ss"] {
            assert_eq!(Key::parse(name), None, "{name:?} is no key");
        }
    }

    #[test]
    fn every_default_of_the_tables_parses() {
        for section in WRITTEN {
            for row in section.rows() {
                assert!(!row.name.is_empty(), "an action needs a name");
                assert!(
                    Key::parse(row.first()).is_some(),
                    "{}: {:?} is no key",
                    row.name,
                    row.first()
                );
                for name in row.keys.split(',') {
                    assert!(
                        Key::parse(name).is_some(),
                        "{}: {name:?} is no key",
                        row.name
                    );
                }
            }
        }
        // A sheet leaves the text box's quit, leader and cancel, which the first
        // three rows of its table are.
        let rows = Section::KeysQuitLeader.rows();
        let names: Vec<&str> = rows.iter().map(|row| row.name).collect();
        assert_eq!(names, ["quit", "leader", "abort-search"]);
        for section in [Section::Keys, Section::Leader, Section::Card] {
            let mut names: Vec<&str> = section.rows().iter().map(|row| row.name).collect();
            names.sort_unstable();
            let count = names.len();
            names.dedup();
            assert_eq!(
                names.len(),
                count,
                "{:?} names an action twice",
                section.name()
            );
        }
    }

    #[test]
    fn a_moved_key_reads_as_its_action_and_frees_its_own() {
        // Ctrl+S is the search's own key, which the file moves away first.
        let (file, notes) = table("[keys]\nsend = \"ctrl+s\"\nsearch-forward = \"alt+s\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+s"),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            file.translate(KEYS_CHAIN, event("enter")),
            None,
            "Enter is a default every action that held it moved away from"
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "tab"),
            (KeyCode::Tab, KeyModifiers::NONE)
        );
    }

    #[test]
    fn a_value_is_one_key_or_a_list_and_quotes_keep_a_hash() {
        let (file, notes) =
            table("[keys]\nup = [\"up\", \"alt+k\"]\nquote = \"ctrl+q\" # a comment\n");
        assert!(notes.is_empty(), "{notes:?}");
        for name in ["up", "alt+k"] {
            assert_eq!(
                read(&file, KEYS_CHAIN, name),
                (KeyCode::Up, KeyModifiers::NONE)
            );
        }
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+q"),
            (KeyCode::Char('q'), KeyModifiers::CONTROL)
        );
        // A `#` inside the quotes is the key itself, not a comment.
        let (file, notes) = table("[leader]\nsidebar = \"#\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            read(&file, LEADER_CHAIN, "#"),
            (KeyCode::Char('b'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn a_bad_line_is_one_note_that_names_it_and_the_others_apply() {
        let text = concat!(
            "[nokeys]\n",
            "send = \"alt+s\"\n",
            "[keys]\n",
            "snd = \"alt+s\"\n",
            "send = \"control+s\"\n",
            "up = []\n",
            "down alt+n\n",
            "redraw = \"ctrl+alt+r\"\n",
        );
        let (file, notes) = table(text);
        assert_eq!(notes.len(), 6, "{notes:?}");
        for (note, line) in notes.iter().zip([1, 2, 4, 5, 6, 7]) {
            assert!(note.contains(&format!("line {line}")), "{note}");
        }
        assert!(notes[3].contains("control+s"), "{}", notes[3]);
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+alt+r"),
            (KeyCode::Char('l'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            file.translate(KEYS_CHAIN, event("ctrl+l")),
            None,
            "the good line after the bad ones still applies"
        );
    }

    #[test]
    fn a_clash_names_the_key_and_the_section_and_both_lines_stay_out() {
        let (file, notes) = table("[keys]\nsend = \"ctrl+s\"\nqueue = \"ctrl+s\"\n");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("ctrl+s"), "{}", notes[0]);
        assert!(notes[0].contains("[keys]"), "{}", notes[0]);
        assert_eq!(
            read(&file, KEYS_CHAIN, "enter"),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+s"),
            (KeyCode::Char('s'), KeyModifiers::CONTROL),
            "neither line moved"
        );
    }

    #[test]
    fn a_key_an_action_that_did_not_move_holds_drops_the_line() {
        let (file, notes) = table("[keys]\nsend = \"up\"\n");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("up"), "{}", notes[0]);
        assert!(notes[0].contains("[keys]"), "{}", notes[0]);
        assert_eq!(
            read(&file, KEYS_CHAIN, "up"),
            (KeyCode::Up, KeyModifiers::NONE),
            "the line stays out and Up stays Up"
        );
    }

    #[test]
    fn a_dropped_line_frees_a_key_that_makes_another_clash() {
        let (file, notes) = table("[keys]\nsend = \"tab\"\nfold = \"enter\"\n");
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(
            read(&file, KEYS_CHAIN, "enter"),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "tab"),
            (KeyCode::Tab, KeyModifiers::NONE)
        );
    }

    #[test]
    fn a_freed_default_is_swallowed() {
        let (file, _) = table("[keys]\nup = \"ctrl+p\"\n");
        assert_eq!(file.translate(KEYS_CHAIN, event("up")), None);
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+p"),
            (KeyCode::Up, KeyModifiers::NONE)
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "down"),
            (KeyCode::Down, KeyModifiers::NONE),
            "another action still holds Down"
        );
    }

    #[test]
    fn a_key_two_actions_hold_stays_until_both_move() {
        // Tab is the queue's and the fold's.
        let (file, _) = table("[keys]\nqueue = \"alt+enter\"\n");
        assert_eq!(
            read(&file, KEYS_CHAIN, "tab"),
            (KeyCode::Tab, KeyModifiers::NONE)
        );
        let (file, _) = table("[keys]\nqueue = \"alt+enter\"\nfold = \"alt+z\"\n");
        assert_eq!(
            file.translate(KEYS_CHAIN, event("tab")),
            None,
            "both moved, so Tab is free"
        );
    }

    #[test]
    fn the_chain_reads_the_leader_section_before_the_text_box() {
        let (file, _) = table("[keys]\nleader = \"alt+x\"\n[leader]\nsessions = \"s\"\n");
        assert_eq!(
            read(&file, LEADER_CHAIN, "alt+x"),
            (KeyCode::Char('x'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            file.translate(KEYS_CHAIN, event("ctrl+x")),
            None,
            "the old leader key is free"
        );
        assert_eq!(
            read(&file, LEADER_CHAIN, "s"),
            (KeyCode::Char('l'), KeyModifiers::NONE)
        );
        assert_eq!(
            file.translate(LEADER_CHAIN, event("l")),
            None,
            "the key the sessions moved away from is swallowed after the leader"
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "s"),
            (KeyCode::Char('s'), KeyModifiers::NONE),
            "and is the box's own key when the leader does not wait"
        );
    }

    #[test]
    fn a_sheet_reads_its_own_keys_and_only_the_quit_and_leader_of_the_box() {
        let (file, _) = table(
            "[keys]\nsend = \"ctrl+s\"\nsearch-forward = \"alt+s\"\n[sheet]\npick = \"ctrl+o\"\n",
        );
        assert_eq!(
            read(&file, SHEET_CHAIN, "ctrl+s"),
            (KeyCode::Char('s'), KeyModifiers::CONTROL),
            "a moved send must not pick a row"
        );
        assert_eq!(
            read(&file, KEYS_CHAIN, "ctrl+s"),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            read(&file, SHEET_CHAIN, "ctrl+o"),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        for name in ["ctrl+c", "ctrl+x"] {
            assert_eq!(
                read(&file, SHEET_CHAIN, name),
                read(&file, KEYS_CHAIN, name),
                "{name} is still the quit or the leader"
            );
        }
    }

    #[test]
    fn a_letter_with_ctrl_or_alt_reads_in_either_case() {
        let (file, _) = table("[keys]\nabort-search = \"Ctrl+G\"\n");
        for name in ["ctrl+g", "ctrl+G", "CTRL+g"] {
            assert_eq!(
                read(&file, KEYS_CHAIN, name),
                (KeyCode::Char('g'), KeyModifiers::CONTROL),
                "{name}"
            );
        }
    }

    #[test]
    fn a_plain_letter_of_the_leader_and_the_card_reads_in_either_case() {
        let (file, _) = table("[leader]\nsessions = \"s\"\n[card]\nonce = \"o\"\n");
        for name in ["s", "S"] {
            assert_eq!(
                read(&file, LEADER_CHAIN, name),
                (KeyCode::Char('l'), KeyModifiers::NONE),
                "{name}"
            );
        }
        for name in ["o", "O"] {
            assert_eq!(
                read(&file, CARD_CHAIN, name),
                (KeyCode::Char('y'), KeyModifiers::NONE),
                "{name}"
            );
        }
        assert_eq!(
            file.translate(CARD_CHAIN, event("y")),
            None,
            "the answer it moved away from is swallowed"
        );
    }

    #[test]
    fn shift_tab_and_backtab_are_one_key() {
        let (file, notes) = table("[sidebar]\nprevious = \"alt+p\"\nup = \"shift+tab\"\n");
        assert!(notes.is_empty(), "{notes:?}");
        for name in ["shift+tab", "backtab"] {
            assert_eq!(
                read(&file, SIDEBAR_CHAIN, name),
                (KeyCode::Up, KeyModifiers::NONE),
                "{name}"
            );
        }
        assert_eq!(
            file.translate(SIDEBAR_CHAIN, event("up")),
            None,
            "the up key the line moved away from is free"
        );
    }
}

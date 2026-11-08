//! Terminal input events mapped to editor and app actions.
//!
//! The map is the change-1 list: Enter sends or steers, Alt+Enter queues, Tab
//! queues while no popup is open, Shift+Enter or Ctrl+J starts a new line, Esc
//! cancels, the second Ctrl+C within [`QUIT_WINDOW`] quits, the arrows, Home,
//! End, Backspace, Delete, Ctrl+A, B, E, K, U and W edit, and Up and Down go to
//! the history when the editor cannot move, as Ctrl+P and Ctrl+N do. Every key a terminal reports in
//! more than one form maps the same way: the legacy forms are in the tests.
//! Mapping one key allocates nothing.
//!
//! Change 2 adds one key of its own, the leader Ctrl+X: Ctrl+X then b or Tab is
//! a [`Leader`] the app reads with [`Keys::take_leader`], and any other key
//! after Ctrl+X goes back to the editor as if there were no leader, so that
//! readline's own Ctrl+X sequences can claim that place later. Ctrl+C is the
//! one key that keeps its meaning after the leader, because the first of two
//! presses only arms the quit. The leader is a flag and no timer: it waits for
//! its key however long that takes.
//!
//! Change 3 gives the approval card a row of answers and the keys that move it:
//! Left and Right step the pick and stop at the ends of the row, Enter answers
//! with the pick, and the four letters of the row answer at once. The card's
//! state, which the card's frame draws from, is [`Card`] here: the pick, whether
//! a save answer is offered, and the answer a key gave, which the frame that
//! follows it prints as the card's one line of the transcript. The card's
//! request stays with the box the app keeps; [`Card`] copies its tool and call
//! once, when a card comes on screen, so the line survives the box.
//!
//! Change 4 adds the keys of the command palette and the lists and sheets it
//! opens. Ctrl+X / is [`Action::Palette`]. A list is open above the text box and a
//! sheet in its place, and each one takes its own keys in its own function
//! ([`list_key`] and [`sheet_key`]); the text box keeps the rest of the map, so
//! typing in a list still edits the query. The leader gains Ctrl+X m for the
//! model sheet, Ctrl+X l for the sessions sheet and Ctrl+X t for the level
//! sheet, all plain keys, and Esc right after Ctrl+X closes the leader instead
//! of cancelling the run. The one
//! table ([`HINTS`], with [`STATUS`] for the status line) is what the hints, the
//! which-key list and the help sheet read, and [`Keys::leader_pending`] tells the
//! app when to draw the which-key list.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How long a second Ctrl+C still quits.
pub const QUIT_WINDOW: Duration = Duration::from_millis(1000);

/// What one key press asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Enter: send, or steer the run that is streaming.
    Send,
    /// Alt+Enter, or Tab with no popup open: queue after the run.
    Queue,
    /// Shift+Enter or Ctrl+J: a line break in the editor.
    NewLine,
    /// Esc: cancel the run.
    Cancel,
    /// The second Ctrl+C within [`QUIT_WINDOW`].
    Quit,
    /// The first Ctrl+C: the app shows the quit hint.
    QuitArmed,
    /// An answer to the approval card on screen, under the key the app's own
    /// answer reads: `y` allows the call once, `a` allows it here and saves the
    /// rule, and `n` opens the note that tells the model what to do instead.
    /// The card's No answer is [`Action::Cancel`], which the app answers as a
    /// refusal with no note, and so is Esc in the card.
    Approval(char),
    /// Ctrl+X /: open the command palette.
    Palette,
    /// A list or a sheet moves its pick this many rows, and stops at its ends.
    Move(i8),
    /// Tab in a list or a sheet: complete the pick into the text box.
    Complete,
    /// Enter in a list or a sheet: take the pick.
    Pick,
    /// Esc in a list or a sheet: close it.
    Close,
    /// Ctrl+D in the sessions sheet: arm the row on screen, and delete it on
    /// the second press.
    Delete,
    /// A key the editor handles.
    Edit(EditKey),
    /// Ctrl+L: draw the whole screen again.
    Redraw,
    /// A key this map leaves alone.
    Ignore,
}

/// A key the editor handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    /// Insert this character at the cursor.
    Insert(char),
    /// One grapheme left.
    Left,
    /// One grapheme right.
    Right,
    /// Up or Ctrl+P: one visual line up, else the history.
    Up,
    /// Down or Ctrl+N: one visual line down, else the history.
    Down,
    /// The start of the visual row.
    Home,
    /// The end of the visual row.
    End,
    /// Remove the grapheme before the cursor.
    Backspace,
    /// Remove the grapheme after the cursor.
    Delete,
    /// Ctrl+A: the start of the line.
    LineStart,
    /// Ctrl+E: the end of the line.
    LineEnd,
    /// Ctrl+K: kill to the end of the line.
    KillEnd,
    /// Ctrl+U: kill to the start of the line.
    KillStart,
    /// Ctrl+W: kill the word before the cursor.
    KillWord,
    /// Alt+F, Ctrl+Right, Alt+Right: the end of the word after the cursor.
    ForwardWord,
    /// Alt+B, Ctrl+Left, Alt+Left: the start of the word before the cursor.
    BackwardWord,
    /// Ctrl+D: remove the character after the cursor.
    DeleteChar,
    /// Alt+D: kill the word after the cursor.
    KillWordForward,
    /// Alt+Backspace and Alt+Ctrl+H: kill the word before the cursor.
    KillWordBackward,
    /// Alt+\: delete the spaces and tabs around the cursor.
    DeleteSpace,
    /// Ctrl+T: transpose the two characters around the cursor.
    Transpose,
    /// Alt+T: transpose the two words around the cursor.
    TransposeWords,
    /// Alt+U: upcase the word after the cursor.
    Upcase,
    /// Alt+L: downcase the word after the cursor.
    Downcase,
    /// Alt+C: capitalize the word after the cursor.
    Capitalize,
    /// Ctrl+Y: put the newest kill back.
    Yank,
    /// Alt+Y: put the kill before the yanked one in its place.
    YankPop,
    /// Ctrl+_ and Ctrl+X Ctrl+U: take the last edit back.
    Undo,
    /// Ctrl+Space: set the mark.
    Mark,
    /// Ctrl+X Ctrl+X: exchange the cursor and the mark.
    Exchange,
    /// Ctrl+Q: insert the next key as it is.
    Quote,
    /// Ctrl+V: put the image of the clipboard in the box as a chip.
    PasteImage,
    /// Ctrl+]: move to the next character the next key names.
    CharSearch,
    /// Alt+Ctrl+]: move to the last one the next key names.
    CharSearchBackward,
    /// Alt+0 to Alt+9: a digit of the next command's argument.
    Digit(u8),
    /// Alt+-: a negative argument.
    NegativeArgument,
    /// The key a quoted insert or a character search waited for.
    NextKey(Option<char>),
    /// Alt+<: the oldest message of the session.
    HistoryOldest,
    /// Alt+>: the draft again.
    HistoryNewest,
    /// Alt+.: the last word of the previous message, and of the one before it
    /// on each press after.
    YankLastArg,
    /// Ctrl+R: search the history back, or the next older match.
    SearchBackward,
    /// Ctrl+S: search the history forward, or the next newer match.
    SearchForward,
    /// Ctrl+G: end the history search and give the draft back.
    AbortSearch,
}

/// What is open over the editor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overlay {
    /// Nothing: the editor has every key the map owns.
    #[default]
    None,
    /// A popup: the note the approval card's Tell answer opens, which takes Tab
    /// and starts a line of the note on Shift+Enter.
    Popup,
    /// An approval card: it takes the row of answers, its arrows, Enter and Esc.
    Approval,
    /// A list above the text box, such as the `/` commands and the `@` files:
    /// the arrows, Tab, Enter and Esc are its own, and every other key maps as
    /// it does with no overlay, because the text box text is its query.
    List,
    /// A sheet in place of the text box, such as the model and the session
    /// sheets: the arrows, PageUp and PageDown, Tab, Enter and Esc are its own,
    /// a printable key is its filter, and it leaves every other key alone.
    Sheet,
}

/// What Ctrl+X and then one more key asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    /// Ctrl+X b: show the sidebar, or hide it.
    ToggleSidebar,
    /// Ctrl+X Tab: move the focus into the sidebar, and back out.
    FocusSidebar,
    /// Ctrl+X m: the model sheet.
    Model,
    /// Ctrl+X t: the level sheet.
    Level,
    /// Ctrl+X l: the sessions sheet.
    Sessions,
    /// Ctrl+X /: the command palette.
    Commands,
}

/// One answer of the approval card's row of choices.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Choice {
    /// `y Allow once`: run the call this one time.
    #[default]
    Once,
    /// `a Always here`: run it and save the rule for this folder.
    Always,
    /// `n No`: refuse the call.
    No,
    /// `t Tell it instead`: refuse it with a note on what to do instead.
    Tell,
}

impl Choice {
    /// Every choice, in the order the card's row shows them.
    pub const ALL: [Choice; 4] = [Choice::Once, Choice::Always, Choice::No, Choice::Tell];

    /// The index of the choice in [`Choice::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The key and the label the card draws for this choice.
    pub const fn parts(self) -> (&'static str, &'static str) {
        match self {
            Choice::Once => ("y", "Allow once"),
            Choice::Always => ("a", "Always here"),
            Choice::No => ("n", "No"),
            Choice::Tell => ("t", "Tell it instead"),
        }
    }
}

/// What a card's key answered, for the one line the answer leaves behind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    /// Allowed once.
    Once,
    /// Allowed here, with the rule saved.
    Always,
    /// Refused, with a note or without one.
    Deny,
}

impl Answer {
    /// The words the line of the transcript opens with, and the glyph that
    /// carries the answer where no color is drawn.
    const fn words(self) -> &'static str {
        match self {
            Answer::Once => "\u{2713} allowed once",
            Answer::Always => "\u{2713} allowed here",
            Answer::Deny => "\u{2717} denied",
        }
    }
}

/// The approval card's state as its keys and its frame see it.
///
/// The card's request stays with the app's box (`ApprovalBox` in `app`), which
/// the app drops when it answers it. What the card needs while it is on screen
/// lives here, with the key map that reads its keys: the pick of its row of
/// answers, whether it offers a save answer, and the answer a key gave, so that
/// the frame after it prints the card's one line even though the box is gone.
#[derive(Debug)]
pub struct Card {
    /// The id of the card on screen, and `None` while no card is.
    id: Option<u64>,
    /// The tool and the call of the card on screen, for its line. The buffer is
    /// filled when a card comes and not again, so a frame copies nothing.
    text: String,
    /// The choice the row of answers has picked.
    pick: Choice,
    /// Whether the card offers the save answer: a card with no rule to save
    /// steps over the Always choice and takes no Always key.
    save: bool,
    /// The answer a key gave, until the frame after it takes the line.
    answer: Option<Answer>,
}

impl Default for Card {
    /// A card the keys have not seen: no card on screen, the first choice
    /// picked, and a save answer offered until a card says otherwise — a card
    /// with no rule to save has its pick settled when its first frame draws it.
    fn default() -> Self {
        Card {
            id: None,
            text: String::new(),
            pick: Choice::Once,
            save: true,
            answer: None,
        }
    }
}

impl Card {
    /// Puts a card on screen. A card the keys have not seen starts with its
    /// first choice picked; the same card keeps its pick, so a note left open
    /// and closed again does not move the row.
    pub fn show(&mut self, id: u64, tool: &str, call: &str, save: bool) {
        self.save = save;
        if self.id == Some(id) {
            return;
        }
        self.id = Some(id);
        self.pick = Choice::Once;
        self.text.clear();
        self.text.push_str(tool);
        self.text.push(' ');
        self.text.push_str(call);
    }

    /// Takes the card off the screen: no key answers it from here on.
    pub fn hide(&mut self) {
        self.id = None;
    }

    /// The choice the row of answers has picked.
    pub fn pick(&self) -> Choice {
        self.pick
    }

    /// Picks `choice`, as its letter in the row does.
    pub fn pick_to(&mut self, choice: Choice) {
        self.pick = choice;
    }

    /// Moves the pick `step` places along the row, stopping at the ends of it. A
    /// card with no rule to save steps over Always.
    pub fn step(&mut self, step: i8) {
        let last = Choice::ALL.len() as i8 - 1;
        let mut index = (self.pick.index() as i8 + step).clamp(0, last);
        if !self.save && Choice::ALL[index as usize] == Choice::Always {
            index = (index + step.signum()).clamp(0, last);
        }
        self.pick = Choice::ALL[index as usize];
    }

    /// Records the answer a key gave, for the line the next frame prints.
    fn answered(&mut self, answer: Answer) {
        if self.id.is_some() {
            self.answer = Some(answer);
        }
    }

    /// Takes the line the answered card leaves in the transcript, once: `✓
    /// allowed once · bash rm -rf target`. `open` is the id of the card on
    /// screen now, if one is: while the card a key answered is still up, the
    /// answer was not taken and the line waits.
    pub fn took(&mut self, open: Option<u64>) -> Option<String> {
        let answer = self.answer?;
        if open == self.id && self.id.is_some() {
            return None;
        }
        self.answer = None;
        let words = answer.words();
        let mut line = String::with_capacity(words.len() + 3 + self.text.len());
        line.push_str(words);
        line.push_str(" \u{b7} ");
        line.push_str(&self.text);
        Some(line)
    }
}

/// Where a key works, which is the group the help sheet lists it under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// The text box, where typing goes.
    TextBox,
    /// The transcript.
    Transcript,
    /// The key after Ctrl+X.
    Leader,
}

/// One key and the words that say what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    /// The keys to press, as the terminal prints them.
    pub keys: &'static str,
    /// The words that say what they do.
    pub words: &'static str,
    /// Where the key works.
    pub group: Group,
    /// The `section:action` of each key of the hint, in order, which says which
    /// keys a file moved; a token with no section is printed as it is. Empty
    /// for a hint whose keys are already all there.
    pub acts: &'static str,
}

/// Every key the help sheet lists, in its order.
pub const HINTS: &[Hint] = &[
    Hint {
        keys: "enter",
        words: "send or steer",
        group: Group::TextBox,
        acts: "keys:send",
    },
    Hint {
        keys: "alt+enter",
        words: "queue",
        group: Group::TextBox,
        acts: "keys:queue",
    },
    Hint {
        keys: "shift+enter",
        words: "new line",
        group: Group::TextBox,
        acts: "keys:new-line",
    },
    Hint {
        keys: "ctrl+x /",
        words: "commands",
        group: Group::TextBox,
        acts: "keys:leader leader:commands",
    },
    Hint {
        keys: "/",
        words: "commands",
        group: Group::TextBox,
        acts: "",
    },
    Hint {
        keys: "@",
        words: "files",
        group: Group::TextBox,
        acts: "",
    },
    Hint {
        keys: "?",
        words: "keys",
        group: Group::TextBox,
        acts: "",
    },
    Hint {
        keys: "esc ctrl+g",
        words: "interrupt",
        group: Group::TextBox,
        acts: "keys:cancel keys:abort-search",
    },
    Hint {
        keys: "ctrl+c ctrl+c",
        words: "quit",
        group: Group::TextBox,
        acts: "keys:quit keys:quit",
    },
    Hint {
        keys: "ctrl+a ctrl+e",
        words: "line ends",
        group: Group::TextBox,
        acts: "keys:line-start keys:line-end",
    },
    Hint {
        keys: "alt+b alt+f",
        words: "by word",
        group: Group::TextBox,
        acts: "keys:backward-word keys:forward-word",
    },
    Hint {
        keys: "ctrl+k ctrl+u ctrl+w",
        words: "kill",
        group: Group::TextBox,
        acts: "keys:kill-end keys:kill-start keys:kill-word",
    },
    Hint {
        keys: "ctrl+y alt+y",
        words: "yank",
        group: Group::TextBox,
        acts: "keys:yank keys:yank-pop",
    },
    Hint {
        keys: "ctrl+t alt+t",
        words: "transpose",
        group: Group::TextBox,
        acts: "keys:transpose keys:transpose-words",
    },
    Hint {
        keys: "ctrl+_",
        words: "undo",
        group: Group::TextBox,
        acts: "keys:undo",
    },
    Hint {
        keys: "ctrl+q",
        words: "quote",
        group: Group::TextBox,
        acts: "keys:quote",
    },
    Hint {
        keys: "ctrl+v",
        words: "paste an image",
        group: Group::TextBox,
        acts: "keys:paste-image",
    },
    Hint {
        keys: "alt+0-9 ctrl+space",
        words: "arg, mark",
        group: Group::TextBox,
        acts: "alt+0-9 keys:mark",
    },
    Hint {
        keys: "ctrl+r ctrl+s",
        words: "search history",
        group: Group::TextBox,
        acts: "keys:search-backward keys:search-forward",
    },
    Hint {
        keys: "ctrl+p ctrl+n",
        words: "up, down",
        group: Group::TextBox,
        acts: "keys:up.1 keys:down.1",
    },
    Hint {
        keys: "alt+< alt+>",
        words: "its ends",
        group: Group::TextBox,
        acts: "keys:history-oldest keys:history-newest",
    },
    Hint {
        keys: "alt+.",
        words: "last word",
        group: Group::TextBox,
        acts: "keys:yank-last-word",
    },
    Hint {
        keys: "tab",
        words: "fold the block",
        group: Group::Transcript,
        acts: "keys:fold",
    },
    Hint {
        keys: "ctrl+o",
        words: "open all",
        group: Group::Transcript,
        acts: "keys:open-all",
    },
    Hint {
        keys: "pgup pgdn",
        words: "scroll",
        group: Group::Transcript,
        acts: "keys:page-up keys:page-down",
    },
    Hint {
        keys: "end",
        words: "jump down",
        group: Group::Transcript,
        acts: "keys:jump-down",
    },
    Hint {
        keys: "b",
        words: "sidebar",
        group: Group::Leader,
        acts: "leader:sidebar",
    },
    Hint {
        keys: "l",
        words: "sessions",
        group: Group::Leader,
        acts: "leader:sessions",
    },
    Hint {
        keys: "m",
        words: "model",
        group: Group::Leader,
        acts: "leader:model",
    },
    Hint {
        keys: "t",
        words: "level",
        group: Group::Leader,
        acts: "leader:level",
    },
    Hint {
        keys: "tab",
        words: "sidebar focus",
        group: Group::Leader,
        acts: "leader:sidebar-focus",
    },
    Hint {
        keys: "/",
        words: "commands",
        group: Group::Leader,
        acts: "leader:commands",
    },
];

/// The keys the status line shows at its right, in the order they drop.
pub const STATUS: &[Hint] = &[
    Hint {
        keys: "ctrl+x /",
        words: "commands",
        group: Group::TextBox,
        acts: "keys:leader leader:commands",
    },
    Hint {
        keys: "ctrl+x",
        words: "more",
        group: Group::Leader,
        acts: "keys:leader",
    },
    Hint {
        keys: "?",
        words: "keys",
        group: Group::TextBox,
        acts: "",
    },
];

/// The keys of one group, in table order. Allocates nothing.
pub fn hints(group: Group) -> impl Iterator<Item = &'static Hint> {
    HINTS.iter().filter(move |hint| hint.group == group)
}

/// The key map. Its state is when Ctrl+C armed the quit, whether Ctrl+X waits
/// for its second key, and the approval card on screen.
#[derive(Debug, Default)]
pub struct Keys {
    armed: Option<Instant>,
    /// Ctrl+X came and its second key has not.
    leader: bool,
    /// The sidebar key the leader resolved, until the app takes it.
    command: Option<Leader>,
    /// The approval card's pick and its line of the transcript.
    card: Card,
}

impl Keys {
    /// A map with the quit unarmed and no leader pending.
    pub fn new() -> Self {
        Keys::default()
    }

    /// The sidebar key the last press resolved, if it was one. A leader command
    /// is spent as it is taken.
    pub fn take_leader(&mut self) -> Option<Leader> {
        self.command.take()
    }

    /// The approval card's state, which its frame draws from.
    pub fn card(&self) -> &Card {
        &self.card
    }

    /// The approval card's state, for the frame that puts a card on screen and
    /// takes the line its answer leaves.
    pub fn card_mut(&mut self) -> &mut Card {
        &mut self.card
    }

    /// Map one key press.
    ///
    /// `now` is the clock the Ctrl+C window is compared against; there is no
    /// timer and no wakeup.
    pub fn action(&mut self, key: KeyEvent, overlay: Overlay, now: Instant) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::Ignore;
        }
        if self.leader {
            // The leader has its key. Ctrl+X b and Ctrl+X Tab are the two app
            // keys of change 2; every other key goes back to the editor as if
            // the leader had not come. Ctrl+C is the quit key here too: the
            // leader it dropped never spends it (CHG-j9kb5g review).
            self.leader = false;
            if let Some(command) = leader_command(&key) {
                self.command = Some(command);
                return Action::Ignore;
            }
            if is_ctrl_c(&key) {
                return self.ctrl_c(now);
            }
            // Esc closes the leader and goes no further: the run stays on, so
            // that aiming Esc at the leader is not the same as cancelling.
            if key.code == KeyCode::Esc {
                return Action::Ignore;
            }
            return map(&key, overlay, &mut self.card, true);
        }
        if is_leader(&key) {
            self.leader = true;
            return Action::Ignore;
        }
        if is_ctrl_c(&key) {
            return self.ctrl_c(now);
        }
        let action = map(&key, overlay, &mut self.card, false);
        // Ctrl+D on an empty box is the quit key, as Ctrl+C is, and the app
        // asks for it with `Keys::quit`: the arming has to survive this key for
        // a second press to spend it.
        if action != Action::Edit(EditKey::DeleteChar) {
            self.armed = None;
        }
        action
    }

    /// Whether Ctrl+X waits for its second key, which the which-key list draws
    /// from.
    pub fn leader_pending(&self) -> bool {
        self.leader
    }

    /// The quit key: arm the quit, or quit when the last arming is inside
    /// [`QUIT_WINDOW`]. Ctrl+C and Ctrl+D on an empty box both come here.
    pub fn quit(&mut self, now: Instant) -> Action {
        self.ctrl_c(now)
    }

    /// Drop a pending Ctrl+C arming: the app calls this for a key that could
    /// have been the quit key and was not.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// Ctrl+C: arm the quit, or quit when the last arming is inside
    /// [`QUIT_WINDOW`].
    fn ctrl_c(&mut self, now: Instant) -> Action {
        let armed = self
            .armed
            .is_some_and(|at| now.saturating_duration_since(at) <= QUIT_WINDOW);
        self.armed = if armed { None } else { Some(now) };
        if armed {
            Action::Quit
        } else {
            Action::QuitArmed
        }
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Ctrl+G, which cancels as Esc does.
pub(crate) fn is_ctrl_g(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('g' | 'G'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Ctrl+X, the leader: 0x18 reaches crossterm as `Char('x')` with CONTROL.
fn is_leader(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('x')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// The app keys that follow the leader. A plain key only: Ctrl+X Ctrl+B is a
/// readline key to give back, not a sidebar one.
fn leader_command(key: &KeyEvent) -> Option<Leader> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char('/') => Some(Leader::Commands),
        KeyCode::Char('b') | KeyCode::Char('B') => Some(Leader::ToggleSidebar),
        KeyCode::Char('l') | KeyCode::Char('L') => Some(Leader::Sessions),
        KeyCode::Char('m') | KeyCode::Char('M') => Some(Leader::Model),
        KeyCode::Char('t') | KeyCode::Char('T') => Some(Leader::Level),
        KeyCode::Tab => Some(Leader::FocusSidebar),
        _ => None,
    }
}

/// The character a key types when a quoted insert takes it: the character of a
/// character key, the control character a Ctrl key sends, a tab for Tab and a
/// line break for Enter. `None` for a key that types no character.
pub fn quoted_char(key: &KeyEvent) -> Option<char> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) if ctrl => Some(control_char(c)),
        KeyCode::Char(c) => Some(c),
        KeyCode::Tab => Some('\t'),
        KeyCode::Enter => Some('\n'),
        _ => None,
    }
}

/// The control character a Ctrl key on `c` sends: Ctrl+J is a line break.
fn control_char(c: char) -> char {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        char::from((lower as u8) & 0x1f)
    } else {
        c
    }
}

fn map(key: &KeyEvent, overlay: Overlay, card: &mut Card, after_leader: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if overlay == Overlay::Approval
        && let Some(action) = card_key(key, card)
    {
        return action;
    }
    if overlay == Overlay::Popup
        && let Some(action) = note_key(key, card)
    {
        return action;
    }
    if overlay == Overlay::Sheet {
        return sheet_key(key);
    }
    if overlay == Overlay::List
        && let Some(action) = list_key(key)
    {
        return action;
    }
    match key.code {
        KeyCode::Enter if alt => Action::Queue,
        // Ctrl+J is 0x0A, which a cooked terminal reports as Enter with CONTROL
        // and `Char('m')` with CONTROL is 0x0D, an ordinary Enter.
        KeyCode::Enter if shift || ctrl => Action::NewLine,
        KeyCode::Enter => Action::Send,
        KeyCode::Tab if overlay == Overlay::None => Action::Queue,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Left if ctrl || alt => Action::Edit(EditKey::BackwardWord),
        KeyCode::Right if ctrl || alt => Action::Edit(EditKey::ForwardWord),
        KeyCode::Left => Action::Edit(EditKey::Left),
        KeyCode::Right => Action::Edit(EditKey::Right),
        KeyCode::Up if !alt => Action::Edit(EditKey::Up),
        KeyCode::Down if !alt => Action::Edit(EditKey::Down),
        KeyCode::Home if !alt => Action::Edit(EditKey::Home),
        KeyCode::End if !alt => Action::Edit(EditKey::End),
        KeyCode::Backspace if alt => Action::Edit(EditKey::KillWordBackward),
        KeyCode::Backspace => Action::Edit(EditKey::Backspace),
        KeyCode::Delete => Action::Edit(EditKey::Delete),
        KeyCode::Char(c) => {
            if alt {
                match (c.to_ascii_lowercase(), ctrl) {
                    ('f', false) => Action::Edit(EditKey::ForwardWord),
                    ('b', false) => Action::Edit(EditKey::BackwardWord),
                    ('d', false) => Action::Edit(EditKey::KillWordForward),
                    ('t', false) => Action::Edit(EditKey::TransposeWords),
                    ('u', false) => Action::Edit(EditKey::Upcase),
                    ('l', false) => Action::Edit(EditKey::Downcase),
                    ('c', false) => Action::Edit(EditKey::Capitalize),
                    ('y', false) => Action::Edit(EditKey::YankPop),
                    ('-', false) => Action::Edit(EditKey::NegativeArgument),
                    ('<', _) => Action::Edit(EditKey::HistoryOldest),
                    ('>', _) => Action::Edit(EditKey::HistoryNewest),
                    ('.', false) => Action::Edit(EditKey::YankLastArg),
                    ('\\', false) => Action::Edit(EditKey::DeleteSpace),
                    // 0x08 with Alt held: Alt+Ctrl+H.
                    ('h', true) => Action::Edit(EditKey::KillWordBackward),
                    // 0x1D with Alt held: Alt+Ctrl+].
                    (']', true) | ('5', true) => Action::Edit(EditKey::CharSearchBackward),
                    ('0'..='9', false) => match c.to_digit(10) {
                        Some(digit) => Action::Edit(EditKey::Digit(digit as u8)),
                        None => Action::Ignore,
                    },
                    _ => Action::Ignore,
                }
            } else if ctrl {
                match c.to_ascii_lowercase() {
                    // The Ctrl+X keys of readline: the leader drops its Ctrl key,
                    // so these two come back here from the branch above.
                    'u' if after_leader => Action::Edit(EditKey::Undo),
                    'x' if after_leader => Action::Edit(EditKey::Exchange),
                    'a' => Action::Edit(EditKey::LineStart),
                    // Readline's back one char. The sidebar takes Ctrl+X b, so
                    // the editor keeps Ctrl+B (TUI change 2 grill Q6).
                    'b' => Action::Edit(EditKey::Left),
                    'd' => Action::Edit(EditKey::DeleteChar),
                    'e' => Action::Edit(EditKey::LineEnd),
                    'f' => Action::Edit(EditKey::Right),
                    'k' => Action::Edit(EditKey::KillEnd),
                    'l' => Action::Redraw,
                    'u' => Action::Edit(EditKey::KillStart),
                    'w' => Action::Edit(EditKey::KillWord),
                    'y' => Action::Edit(EditKey::Yank),
                    't' => Action::Edit(EditKey::Transpose),
                    'q' => Action::Edit(EditKey::Quote),
                    'v' => Action::Edit(EditKey::PasteImage),
                    // 0x00: Ctrl+Space, which crossterm also reports as Ctrl+@.
                    ' ' | '@' => Action::Edit(EditKey::Mark),
                    // 0x1F: Ctrl+_, which crossterm reports as Ctrl+7, and the
                    // kitty protocol as Ctrl+_ or Ctrl+/.
                    '7' | '_' | '/' => Action::Edit(EditKey::Undo),
                    // 0x1D: Ctrl+], which crossterm reports as Ctrl+5.
                    ']' | '5' => Action::Edit(EditKey::CharSearch),
                    // 0x08: Ctrl+H, which is also what Ctrl+Backspace sends.
                    'h' => Action::Edit(EditKey::Backspace),
                    'j' => Action::NewLine,
                    'm' => Action::Send,
                    'p' => Action::Edit(EditKey::Up),
                    'n' => Action::Edit(EditKey::Down),
                    'r' => Action::Edit(EditKey::SearchBackward),
                    's' => Action::Edit(EditKey::SearchForward),
                    'g' => Action::Edit(EditKey::AbortSearch),
                    _ => Action::Ignore,
                }
            } else if c.is_control() {
                Action::Ignore
            } else {
                Action::Edit(EditKey::Insert(c))
            }
        }
        _ => Action::Ignore,
    }
}

/// The step of a pick that Ctrl+P or Ctrl+N asks for, as Up and Down do.
fn ctrl_step(key: &KeyEvent) -> Option<i8> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char('p' | 'P') => Some(-1),
        KeyCode::Char('n' | 'N') => Some(1),
        _ => None,
    }
}

/// One key of a list above the text box: the arrows move its pick, Tab completes
/// it, Enter takes it and Esc closes it. `None` for every other key, which maps
/// as it does with no overlay, since the text box text is the list's query.
fn list_key(key: &KeyEvent) -> Option<Action> {
    if let Some(step) = ctrl_step(key) {
        return Some(Action::Move(step));
    }
    match key.code {
        KeyCode::Up => Some(Action::Move(-1)),
        KeyCode::Down => Some(Action::Move(1)),
        KeyCode::Tab => Some(Action::Complete),
        KeyCode::Enter => Some(Action::Pick),
        KeyCode::Esc => Some(Action::Close),
        _ => None,
    }
}

/// One key of a sheet in place of the text box: the arrows and PageUp and
/// PageDown move its pick, Tab completes it, Enter takes it, Esc closes it, and
/// a printable key is its filter. Every other key is ignored, and Ctrl+C is
/// still the quit, which the map takes before a key reaches here.
fn sheet_key(key: &KeyEvent) -> Action {
    if let Some(step) = ctrl_step(key) {
        return Action::Move(step);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Up => Action::Move(-1),
        KeyCode::Down => Action::Move(1),
        KeyCode::PageUp => Action::Move(-10),
        KeyCode::PageDown => Action::Move(10),
        KeyCode::Tab => Action::Complete,
        KeyCode::Enter => Action::Pick,
        KeyCode::Esc => Action::Close,
        KeyCode::Backspace if alt => Action::Edit(EditKey::KillWordBackward),
        KeyCode::Backspace => Action::Edit(EditKey::Backspace),
        KeyCode::Char('d') if ctrl => Action::Delete,
        KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => Action::Edit(EditKey::Insert(c)),
        _ => Action::Ignore,
    }
}

/// One key of the approval card's row of answers: Left and Right step the pick
/// and stop at the ends of the row, a letter answers at once and moves the pick
/// to itself, Enter answers with the pick, and Esc is the refusal with no note.
/// `None` for a key the card leaves to the map, such as Ctrl+C.
fn card_key(key: &KeyEvent, card: &mut Card) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let choice = match key.code {
        KeyCode::Left => {
            card.step(-1);
            return Some(Action::Ignore);
        }
        KeyCode::Right => {
            card.step(1);
            return Some(Action::Ignore);
        }
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => card.pick(),
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'y' => Choice::Once,
            'a' => Choice::Always,
            'n' => Choice::No,
            't' => Choice::Tell,
            _ => return None,
        },
        KeyCode::Esc => {
            card.answered(Answer::Deny);
            return Some(Action::Cancel);
        }
        _ => return None,
    };
    // A card with no rule to save offers no Always answer, and takes no key for
    // it either: a key the card answers with nothing does not reach the editor.
    if choice == Choice::Always && !card.save {
        return Some(Action::Ignore);
    }
    card.pick_to(choice);
    Some(answer(choice, card))
}

/// The action one choice answers the app with, and the answer the transcript
/// prints for it. No is the app's [`Action::Cancel`], the refusal with no note,
/// and Tell it instead opens the note, which answers the card when it goes.
fn answer(choice: Choice, card: &mut Card) -> Action {
    match choice {
        Choice::Once => {
            card.answered(Answer::Once);
            Action::Approval('y')
        }
        Choice::Always => {
            card.answered(Answer::Always);
            Action::Approval('a')
        }
        Choice::No => {
            card.answered(Answer::Deny);
            Action::Cancel
        }
        Choice::Tell => Action::Approval('n'),
    }
}

/// One key of the note the card's Tell answer opens, which is the map's only
/// popup: Shift+Enter starts another line of it, and Enter sends it as the
/// refusal. `None` for those two, which the map already sends and starts a line
/// with, and for every other key, which reaches the note as an edit.
///
/// Only the Enter that sends the note answers the card. Esc goes back to the
/// choices and Shift+Enter starts a line, and neither is an answer: a card
/// another client resolves while the note is open must not be printed as a
/// refusal the person never gave.
fn note_key(key: &KeyEvent, card: &mut Card) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(Action::Edit(EditKey::Insert('\n')))
        }
        KeyCode::Enter => {
            card.answered(Answer::Deny);
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use KeyCode::Char;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{
        Action, Card, Choice, EditKey, Group, HINTS, Hint, Keys, Leader, Overlay, STATUS, hints,
    };

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn cases() -> Vec<(KeyEvent, Action)> {
        use EditKey::{Backspace, Delete, Down, End, Home, Insert, KillEnd, KillStart, KillWord};
        use EditKey::{
            BackwardWord, Capitalize, DeleteSpace, Downcase, ForwardWord, KillWordBackward,
            KillWordForward, NegativeArgument, TransposeWords, Upcase, YankPop,
        };
        use EditKey::{Left, LineEnd, LineStart, Right, Up};
        use KeyCode::Char;

        vec![
            (key(KeyCode::Enter, NONE), Action::Send),
            (key(KeyCode::Enter, ALT), Action::Queue),
            (key(KeyCode::Enter, SHIFT), Action::NewLine),
            (key(KeyCode::Enter, CTRL), Action::NewLine),
            (key(Char('j'), CTRL), Action::NewLine),
            (key(KeyCode::Tab, NONE), Action::Queue),
            (key(KeyCode::Esc, NONE), Action::Cancel),
            (key(KeyCode::Left, NONE), Action::Edit(Left)),
            (key(KeyCode::Right, NONE), Action::Edit(Right)),
            (key(KeyCode::Up, NONE), Action::Edit(Up)),
            (key(KeyCode::Down, NONE), Action::Edit(Down)),
            (key(KeyCode::Home, NONE), Action::Edit(Home)),
            (key(KeyCode::End, NONE), Action::Edit(End)),
            (key(KeyCode::Backspace, NONE), Action::Edit(Backspace)),
            (key(KeyCode::Delete, NONE), Action::Edit(Delete)),
            (key(Char('a'), CTRL), Action::Edit(LineStart)),
            (key(Char('e'), CTRL), Action::Edit(LineEnd)),
            (key(Char('k'), CTRL), Action::Edit(KillEnd)),
            (key(Char('u'), CTRL), Action::Edit(KillStart)),
            (key(Char('w'), CTRL), Action::Edit(KillWord)),
            (key(Char('x'), NONE), Action::Edit(Insert('x'))),
            (key(Char('X'), SHIFT), Action::Edit(Insert('X'))),
            (key(Char(' '), NONE), Action::Edit(Insert(' '))),
            (key(Char('世'), NONE), Action::Edit(Insert('世'))),
            (key(Char('\u{301}'), NONE), Action::Edit(Insert('\u{301}'))),
            (key(KeyCode::F(1), NONE), Action::Ignore),
            (key(KeyCode::BackTab, NONE), Action::Ignore),
            (key(Char('b'), ALT), Action::Edit(BackwardWord)),
            (key(Char('f'), ALT), Action::Edit(ForwardWord)),
            (key(Char('d'), ALT), Action::Edit(KillWordForward)),
            (key(KeyCode::Backspace, ALT), Action::Edit(KillWordBackward)),
            (key(Char('h'), CTRL | ALT), Action::Edit(KillWordBackward)),
            (key(Char('t'), ALT), Action::Edit(TransposeWords)),
            (key(Char('u'), ALT), Action::Edit(Upcase)),
            (key(Char('l'), ALT), Action::Edit(Downcase)),
            (key(Char('c'), ALT), Action::Edit(Capitalize)),
            (key(Char('y'), ALT), Action::Edit(YankPop)),
            (key(Char('\\'), ALT), Action::Edit(DeleteSpace)),
            (key(Char('-'), ALT), Action::Edit(NegativeArgument)),
            (key(Char('3'), ALT), Action::Edit(EditKey::Digit(3))),
            (key(Char('d'), CTRL), Action::Edit(EditKey::DeleteChar)),
            (key(Char('f'), CTRL), Action::Edit(Right)),
            (key(Char('t'), CTRL), Action::Edit(EditKey::Transpose)),
            (key(Char('y'), CTRL), Action::Edit(EditKey::Yank)),
            (key(Char('q'), CTRL), Action::Edit(EditKey::Quote)),
            // Ctrl+V is the image paste now, and Ctrl+Q quotes on its own.
            (key(Char('v'), CTRL), Action::Edit(EditKey::PasteImage)),
            (key(Char(' '), CTRL), Action::Edit(EditKey::Mark)),
            (key(Char('@'), CTRL), Action::Edit(EditKey::Mark)),
            (key(Char('7'), CTRL), Action::Edit(EditKey::Undo)),
            (key(Char('_'), CTRL), Action::Edit(EditKey::Undo)),
            (key(Char('/'), CTRL), Action::Edit(EditKey::Undo)),
            (key(Char('5'), CTRL), Action::Edit(EditKey::CharSearch)),
            (key(Char(']'), CTRL), Action::Edit(EditKey::CharSearch)),
            (
                key(Char('5'), CTRL | ALT),
                Action::Edit(EditKey::CharSearchBackward),
            ),
            (
                key(Char(']'), CTRL | ALT),
                Action::Edit(EditKey::CharSearchBackward),
            ),
            (key(KeyCode::Left, CTRL), Action::Edit(BackwardWord)),
            (key(KeyCode::Right, CTRL), Action::Edit(ForwardWord)),
            (key(KeyCode::Left, ALT), Action::Edit(BackwardWord)),
            (key(KeyCode::Right, ALT), Action::Edit(ForwardWord)),
        ]
    }

    #[test]
    fn the_key_map() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (event, want) in cases() {
            assert_eq!(
                keys.action(event, Overlay::None, now),
                want,
                "{event:?} maps to {want:?}"
            );
        }
    }

    /// The forms a terminal reports for the same key: 0x0A is an Enter with
    /// CONTROL when the terminal is not raw and `Char('j')` with CONTROL when it
    /// is, 0x0D is an Enter, 0x08 is `Char('h')` with CONTROL, and an Alt key
    /// arrives as an ESC prefix, which crossterm folds into the ALT modifier.
    #[test]
    fn the_legacy_forms_map_like_the_modern_ones() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (legacy, modern) in [
            (key(KeyCode::Enter, CTRL), key(KeyCode::Char('j'), CTRL)),
            (key(KeyCode::Enter, NONE), key(KeyCode::Char('m'), CTRL)),
            (key(KeyCode::Enter, ALT), key(KeyCode::Enter, ALT)),
            (key(KeyCode::Char('h'), CTRL), key(KeyCode::Backspace, NONE)),
        ] {
            assert_eq!(
                keys.action(legacy, Overlay::None, now),
                keys.action(modern, Overlay::None, now),
                "{legacy:?} and {modern:?} are the same key"
            );
        }
        assert_eq!(
            keys.action(key(KeyCode::Char('h'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Backspace)
        );
        assert_eq!(
            keys.action(key(KeyCode::Char('m'), CTRL), Overlay::None, now),
            Action::Send,
            "Ctrl+M is Enter"
        );
    }

    #[test]
    fn ctrl_c_arms_and_then_quits_within_a_second() {
        let mut keys = Keys::new();
        let start = Instant::now();
        let ctrl_c = key(KeyCode::Char('c'), CTRL);
        assert_eq!(keys.action(ctrl_c, Overlay::None, start), Action::QuitArmed);
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(999)),
            Action::Quit
        );
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_secs(5)),
            Action::QuitArmed,
            "a quit spends the arming"
        );
    }

    #[test]
    fn ctrl_c_two_seconds_apart_only_rearms() {
        let mut keys = Keys::new();
        let start = Instant::now();
        let ctrl_c = key(KeyCode::Char('c'), CTRL);
        assert_eq!(keys.action(ctrl_c, Overlay::None, start), Action::QuitArmed);
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_secs(2)),
            Action::QuitArmed
        );
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(2900)),
            Action::Quit,
            "the second press re-armed the window"
        );
    }

    #[test]
    fn another_key_before_the_second_ctrl_c_disarms() {
        let mut keys = Keys::new();
        let start = Instant::now();
        let ctrl_c = key(KeyCode::Char('c'), CTRL);
        assert_eq!(keys.action(ctrl_c, Overlay::None, start), Action::QuitArmed);
        assert_eq!(
            keys.action(key(KeyCode::Char('a'), NONE), Overlay::None, start),
            Action::Edit(EditKey::Insert('a'))
        );
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(100)),
            Action::QuitArmed
        );
    }

    /// The card's row of answers: a letter answers at once and moves the pick
    /// to itself, and the pick is where the row shows it.
    #[test]
    fn approval_card_keys_answer_at_once() {
        let mut keys = Keys::new();
        let now = Instant::now();
        keys.card_mut().show(1, "bash", "rm -rf target", true);
        for (pressed, want, pick) in [
            ('y', Action::Approval('y'), Choice::Once),
            ('a', Action::Approval('a'), Choice::Always),
            ('n', Action::Cancel, Choice::No),
            ('t', Action::Approval('n'), Choice::Tell),
        ] {
            assert_eq!(
                keys.action(key(Char(pressed), NONE), Overlay::Approval, now),
                want,
                "{pressed:?} answers the card"
            );
            assert_eq!(keys.card().pick(), pick, "the pick follows the letter");
        }
        assert_eq!(
            keys.action(key(Char('Y'), SHIFT), Overlay::Approval, now),
            Action::Approval('y'),
            "a shifted answer is the same answer"
        );
        assert_eq!(
            keys.action(key(Char('p'), CTRL | ALT), Overlay::Approval, now),
            Action::Ignore,
            "a Ctrl key that answers nothing is not an answer"
        );
        assert_eq!(
            keys.action(key(Char('y'), NONE), Overlay::None, now),
            Action::Edit(EditKey::Insert('y')),
            "outside the card y is text"
        );
    }

    /// Enter answers with the pick, the arrows move it and stop at the ends of
    /// the row, and Esc is the refusal.
    #[test]
    fn approval_card_enter_answers_with_the_pick() {
        let mut keys = Keys::new();
        let now = Instant::now();
        keys.card_mut().show(7, "bash", "rm -rf target", true);
        let right = key(KeyCode::Right, NONE);
        let left = key(KeyCode::Left, NONE);
        for want in [Choice::Always, Choice::No, Choice::Tell, Choice::Tell] {
            assert_eq!(keys.action(right, Overlay::Approval, now), Action::Ignore);
            assert_eq!(keys.card().pick(), want, "Right stops at the end");
        }
        for want in [Choice::No, Choice::Always, Choice::Once, Choice::Once] {
            assert_eq!(keys.action(left, Overlay::Approval, now), Action::Ignore);
            assert_eq!(keys.card().pick(), want, "Left stops at the start");
        }
        let enter = key(KeyCode::Enter, NONE);
        for want in [
            Action::Approval('y'),
            Action::Approval('a'),
            Action::Cancel,
            Action::Approval('n'),
        ] {
            assert_eq!(keys.action(enter, Overlay::Approval, now), want);
            assert_eq!(keys.action(right, Overlay::Approval, now), Action::Ignore);
        }
        assert_eq!(
            keys.action(key(KeyCode::Esc, NONE), Overlay::Approval, now),
            Action::Cancel,
            "Esc in the card refuses it with no note"
        );
        assert_eq!(
            keys.action(key(KeyCode::Esc, NONE), Overlay::None, now),
            Action::Cancel,
            "Esc outside the card still cancels"
        );
    }

    /// A card with no rule to save offers no Always answer: the pick steps over
    /// it and its key does nothing.
    #[test]
    fn approval_card_without_a_save_rule_steps_over_always() {
        let mut keys = Keys::new();
        let now = Instant::now();
        keys.card_mut().show(9, "bash", "make", false);
        assert_eq!(
            keys.action(key(KeyCode::Right, NONE), Overlay::Approval, now),
            Action::Ignore
        );
        assert_eq!(keys.card().pick(), Choice::No);
        assert_eq!(
            keys.action(key(Char('a'), NONE), Overlay::Approval, now),
            Action::Ignore,
            "a save with no rule to save would be dropped"
        );
        assert_eq!(keys.card().pick(), Choice::No, "the pick does not move");
    }

    /// The note's keys: Shift+Enter starts another line of it, Enter sends it as
    /// the refusal, and Esc goes back to the choices. Neither of those two is an
    /// answer, so a card another client resolves in the meantime leaves no
    /// refusal behind. Tab goes to nobody, as before.
    #[test]
    fn approval_card_note_keys() {
        let mut keys = Keys::new();
        let now = Instant::now();
        keys.card_mut().show(3, "bash", "make", true);
        assert_eq!(
            keys.action(key(KeyCode::Enter, SHIFT), Overlay::Popup, now),
            Action::Edit(EditKey::Insert('\n')),
            "Shift+Enter starts a line of the note"
        );
        assert_eq!(
            keys.card_mut().took(None),
            None,
            "a new line answers nothing"
        );
        assert_eq!(
            keys.action(key(KeyCode::Esc, NONE), Overlay::Popup, now),
            Action::Cancel
        );
        assert_eq!(
            keys.card_mut().took(None),
            None,
            "Esc goes back to the choices and answers nothing"
        );
        assert_eq!(
            keys.action(key(KeyCode::Enter, NONE), Overlay::Popup, now),
            Action::Send,
            "Enter sends the note as the refusal"
        );
        assert_eq!(
            keys.card_mut().took(None),
            Some("\u{2717} denied \u{b7} bash make".to_owned()),
            "the Enter that sends the note is the one answer it gives"
        );
        assert_eq!(
            keys.action(key(Char('x'), NONE), Overlay::Popup, now),
            Action::Edit(EditKey::Insert('x')),
            "a letter types into the note"
        );
        assert_eq!(
            keys.action(key(KeyCode::Tab, NONE), Overlay::Popup, now),
            Action::Ignore
        );
    }

    /// The line an answer leaves names the answer and the call, and it is the
    /// card that answered: while that card is still on screen the line waits.
    #[test]
    fn approval_card_leaves_its_answer_line_once() {
        let mut card = Card::default();
        assert_eq!(card.took(None), None, "no answer, no line");
        card.show(4, "bash", "rm -rf target", true);
        assert_eq!(card.took(Some(4)), None, "the card took no answer yet");
        assert_eq!(card.took(None), None, "and none waits behind it");

        let mut keys = Keys::new();
        let now = Instant::now();
        keys.card_mut().show(4, "bash", "rm -rf target", true);
        keys.action(key(Char('y'), NONE), Overlay::Approval, now);
        assert_eq!(
            keys.card_mut().took(Some(4)),
            None,
            "the card is still up, so nothing is taken"
        );
        let line = keys.card_mut().took(None).expect("the answer's line");
        assert_eq!(line, "\u{2713} allowed once \u{b7} bash rm -rf target");
        assert_eq!(keys.card_mut().took(None), None, "the line is taken once");
    }

    #[test]
    fn tab_queues_only_with_no_popup() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let tab = key(KeyCode::Tab, NONE);
        assert_eq!(keys.action(tab, Overlay::None, now), Action::Queue);
        assert_eq!(keys.action(tab, Overlay::Popup, now), Action::Ignore);
        assert_eq!(keys.action(tab, Overlay::Approval, now), Action::Ignore);
    }

    #[test]
    fn the_leader_maps_the_sidebar_keys() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let ctrl_x = key(Char('x'), CTRL);
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert_eq!(
            keys.take_leader(),
            None,
            "Ctrl+X on its own is not a command"
        );
        assert_eq!(
            keys.action(key(Char('b'), NONE), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(keys.take_leader(), Some(Leader::ToggleSidebar));
        assert_eq!(keys.take_leader(), None, "a leader command is spent once");

        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert_eq!(
            keys.action(key(KeyCode::Tab, NONE), Overlay::None, now),
            Action::Ignore,
            "the leader takes Tab before the queue does"
        );
        assert_eq!(keys.take_leader(), Some(Leader::FocusSidebar));

        // The leader wins over a popup too: Ctrl+X Tab is a command, not a Tab.
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert_eq!(
            keys.action(key(KeyCode::Tab, NONE), Overlay::Popup, now),
            Action::Ignore
        );
        assert_eq!(keys.take_leader(), Some(Leader::FocusSidebar));

        // A shifted key is the same command.
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert_eq!(
            keys.action(key(Char('B'), SHIFT), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(keys.take_leader(), Some(Leader::ToggleSidebar));

        // A Ctrl+X pair is given back: Ctrl+X Ctrl+B is not the sidebar key.
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert_eq!(
            keys.action(key(Char('b'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Left)
        );
        assert_eq!(keys.take_leader(), None);
    }

    /// A Ctrl+C that follows the leader is a Ctrl+C, not a key the leader drops:
    /// it arms the quit and then quits (CHG-j9kb5g review).
    #[test]
    fn ctrl_c_after_the_leader_arms_the_quit() {
        let mut keys = Keys::new();
        let start = Instant::now();
        let ctrl_x = key(Char('x'), CTRL);
        let ctrl_c = key(Char('c'), CTRL);
        assert_eq!(keys.action(ctrl_x, Overlay::None, start), Action::Ignore);
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start),
            Action::QuitArmed,
            "the leader is dropped and Ctrl+C arms the quit"
        );
        assert_eq!(keys.take_leader(), None, "Ctrl+C is no sidebar command");
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(100)),
            Action::Quit
        );
        // A leader between the two presses spends neither the arming nor the
        // key after it.
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(500)),
            Action::QuitArmed
        );
        assert_eq!(
            keys.action(ctrl_x, Overlay::None, start + Duration::from_millis(600)),
            Action::Ignore
        );
        assert_eq!(
            keys.action(ctrl_c, Overlay::None, start + Duration::from_millis(700)),
            Action::Quit,
            "Ctrl+X between two Ctrl+C does not stop the second one quitting"
        );
    }

    /// Ctrl+X then any other key gives both keys back to the editor, so that
    /// readline's own Ctrl+X sequences can claim them later.
    #[test]
    fn the_leader_gives_another_key_back() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (second, want) in [
            (key(Char('a'), CTRL), Action::Edit(EditKey::LineStart)),
            (key(Char('e'), CTRL), Action::Edit(EditKey::LineEnd)),
            (key(Char('k'), CTRL), Action::Edit(EditKey::KillEnd)),
            (key(KeyCode::Enter, NONE), Action::Send),
        ] {
            assert_eq!(
                keys.action(key(Char('x'), CTRL), Overlay::None, now),
                Action::Ignore
            );
            assert_eq!(
                keys.action(second, Overlay::None, now),
                want,
                "Ctrl+X then {second:?} maps as {second:?} alone"
            );
            assert_eq!(keys.take_leader(), None, "no command came of it");
        }
    }

    #[test]
    fn the_leader_gives_a_printable_key_back() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for c in ['x', 'q', '世', ' '] {
            assert_eq!(
                keys.action(key(Char('x'), CTRL), Overlay::None, now),
                Action::Ignore
            );
            assert_eq!(
                keys.action(key(Char(c), NONE), Overlay::None, now),
                Action::Edit(EditKey::Insert(c)),
                "Ctrl+X then {c:?} types {c:?}"
            );
            assert_eq!(keys.take_leader(), None);
        }
    }

    /// A second Ctrl+X is given back like any other key: it does not start a
    /// leader of its own, so Ctrl+X x b types nothing and is not a command.
    #[test]
    fn ctrl_x_then_ctrl_x_does_not_rearm() {
        let mut keys = Keys::new();
        let now = Instant::now();
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Exchange),
            "Ctrl+X Ctrl+X is the exchange of the point and the mark"
        );
        assert_eq!(
            keys.action(key(Char('b'), NONE), Overlay::None, now),
            Action::Edit(EditKey::Insert('b')),
            "both Ctrl+X went to the editor, so b is text"
        );
        assert_eq!(keys.take_leader(), None);
    }

    /// The leader keeps a flag and no clock: there is no timer and no wakeup,
    /// so a second key a day later still resolves the leader.
    #[test]
    fn the_leader_waits_with_no_timer() {
        let mut keys = Keys::new();
        let start = Instant::now();
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, start),
            Action::Ignore
        );
        assert_eq!(
            keys.action(
                key(Char('b'), NONE),
                Overlay::None,
                start + Duration::from_secs(86_400)
            ),
            Action::Ignore
        );
        assert_eq!(keys.take_leader(), Some(Leader::ToggleSidebar));
    }

    /// Ctrl+B is readline's back one char: the sidebar is Ctrl+X b instead, so
    /// the editor keeps it (TUI change 2 grill Q6).
    #[test]
    fn ctrl_b_moves_back_one_char() {
        let mut keys = Keys::new();
        let now = Instant::now();
        assert_eq!(
            keys.action(key(Char('b'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Left)
        );
        assert_eq!(
            keys.action(key(Char('B'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Left),
            "a shifted Ctrl+B is the same key"
        );
        assert_eq!(keys.take_leader(), None, "Ctrl+B is never a leader");
    }

    /// Ctrl+X m and Ctrl+X l are the model and the sessions sheets, plain keys
    /// like the sidebar's b.
    #[test]
    fn the_leader_maps_the_sheet_keys() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (second, want) in [
            (key(Char('l'), NONE), Leader::Sessions),
            (key(Char('L'), SHIFT), Leader::Sessions),
            (key(Char('m'), NONE), Leader::Model),
            (key(Char('M'), SHIFT), Leader::Model),
        ] {
            assert_eq!(
                keys.action(key(Char('x'), CTRL), Overlay::None, now),
                Action::Ignore
            );
            assert_eq!(
                keys.action(second, Overlay::None, now),
                Action::Ignore,
                "Ctrl+X then {second:?} is a command"
            );
            assert_eq!(
                keys.take_leader(),
                Some(want),
                "Ctrl+X then {second:?} is {want:?}"
            );
        }
        // A Ctrl+ key is given back, as it is for the sidebar's b.
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(
            keys.action(key(Char('m'), CTRL), Overlay::None, now),
            Action::Send,
            "Ctrl+M is still Enter"
        );
        assert_eq!(keys.take_leader(), None);
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(
            keys.action(key(Char('l'), CTRL), Overlay::None, now),
            Action::Redraw,
            "Ctrl+L is not the sessions sheet, it draws the screen again"
        );
        assert_eq!(keys.take_leader(), None);
    }

    /// Ctrl+X t is the level sheet, on t or T, a plain key as Ctrl+X m is.
    #[test]
    fn ctrl_x_t_is_the_level_key() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for second in [key(Char('t'), NONE), key(Char('T'), SHIFT)] {
            assert_eq!(
                keys.action(key(Char('x'), CTRL), Overlay::None, now),
                Action::Ignore
            );
            assert_eq!(
                keys.action(second, Overlay::None, now),
                Action::Ignore,
                "Ctrl+X then {second:?} is a command"
            );
            assert_eq!(keys.take_leader(), Some(Leader::Level));
        }
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(
            keys.action(key(Char('t'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Transpose),
            "Ctrl+T is still transpose"
        );
        assert_eq!(keys.take_leader(), None);
    }

    /// Esc closes a leader that waits and goes no further: it must not cancel
    /// the run the person aimed the Esc at the leader with.
    #[test]
    fn esc_after_the_leader_closes_it() {
        let mut keys = Keys::new();
        let now = Instant::now();
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert!(keys.leader_pending());
        assert_eq!(
            keys.action(key(KeyCode::Esc, NONE), Overlay::None, now),
            Action::Ignore,
            "Esc after Ctrl+X is no cancel"
        );
        assert!(!keys.leader_pending(), "the leader took the Esc");
        assert_eq!(keys.take_leader(), None, "Esc is no command");
        assert_eq!(
            keys.action(key(KeyCode::Esc, NONE), Overlay::None, now),
            Action::Cancel,
            "the Esc after that one cancels"
        );
    }

    #[test]
    fn leader_pending_follows_ctrl_x() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let ctrl_x = key(Char('x'), CTRL);
        assert!(!keys.leader_pending(), "no leader at first");
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert!(keys.leader_pending());
        assert_eq!(
            keys.action(key(Char('b'), NONE), Overlay::None, now),
            Action::Ignore
        );
        assert!(!keys.leader_pending(), "the leader has its key");
        assert_eq!(keys.take_leader(), Some(Leader::ToggleSidebar));
        assert_eq!(keys.action(ctrl_x, Overlay::None, now), Action::Ignore);
        assert!(keys.leader_pending());
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::Exchange)
        );
        assert!(!keys.leader_pending(), "the second Ctrl+X drops it");
        assert_eq!(keys.take_leader(), None);
    }

    #[test]
    fn ctrl_p_and_ctrl_n_are_up_and_down_in_every_place() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let (ctrl_p, ctrl_n) = (key(Char('p'), CTRL), key(Char('n'), CTRL));
        for (overlay, up, down) in [
            (
                Overlay::None,
                Action::Edit(EditKey::Up),
                Action::Edit(EditKey::Down),
            ),
            (Overlay::List, Action::Move(-1), Action::Move(1)),
            (Overlay::Sheet, Action::Move(-1), Action::Move(1)),
        ] {
            assert_eq!(keys.action(ctrl_p, overlay, now), up, "{overlay:?}");
            assert_eq!(keys.action(ctrl_n, overlay, now), down, "{overlay:?}");
            assert_eq!(
                keys.action(key(Char('P'), CTRL), overlay, now),
                up,
                "a shifted Ctrl+P is the same key"
            );
            assert_eq!(
                keys.action(key(KeyCode::Up, NONE), overlay, now),
                up,
                "Up does the same with {overlay:?}"
            );
        }
        assert_eq!(
            keys.action(key(Char('p'), CTRL | ALT), Overlay::None, now),
            Action::Ignore,
            "Ctrl+Alt+P is not the key"
        );
        assert_eq!(
            keys.action(key(Char('p'), CTRL | ALT), Overlay::Sheet, now),
            Action::Ignore,
            "nor in a sheet"
        );
        assert_eq!(
            keys.action(key(Char('p'), NONE), Overlay::None, now),
            Action::Edit(EditKey::Insert('p')),
            "p alone is text"
        );
        // Ctrl+X / opens the palette.
        assert_eq!(
            keys.action(key(Char('x'), CTRL), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(
            keys.action(key(Char('/'), NONE), Overlay::None, now),
            Action::Ignore
        );
        assert_eq!(keys.take_leader(), Some(Leader::Commands));
    }

    #[test]
    fn ctrl_r_and_ctrl_s_are_the_history_search() {
        let mut keys = Keys::new();
        let now = Instant::now();
        assert_eq!(
            keys.action(key(Char('r'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::SearchBackward)
        );
        assert_eq!(
            keys.action(key(Char('s'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::SearchForward)
        );
        assert_eq!(
            keys.action(key(Char('g'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::AbortSearch)
        );
        assert_eq!(
            keys.action(key(Char('<'), ALT), Overlay::None, now),
            Action::Edit(EditKey::HistoryOldest)
        );
        assert_eq!(
            keys.action(key(Char('>'), ALT), Overlay::None, now),
            Action::Edit(EditKey::HistoryNewest)
        );
        assert_eq!(
            keys.action(key(Char('.'), ALT), Overlay::None, now),
            Action::Edit(EditKey::YankLastArg)
        );
    }

    #[test]
    fn a_list_takes_five_keys_and_maps_the_rest() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (event, want) in [
            (key(KeyCode::Up, NONE), Action::Move(-1)),
            (key(KeyCode::Down, NONE), Action::Move(1)),
            (key(KeyCode::Tab, NONE), Action::Complete),
            (key(KeyCode::Enter, NONE), Action::Pick),
            (key(KeyCode::Esc, NONE), Action::Close),
        ] {
            assert_eq!(
                keys.action(event, Overlay::List, now),
                want,
                "{event:?} in a list"
            );
        }
        // Every other key maps as with no overlay: the text box text is the
        // query, so typing still edits it.
        for (event, want) in [
            (key(Char('a'), NONE), Action::Edit(EditKey::Insert('a'))),
            (key(Char(' '), NONE), Action::Edit(EditKey::Insert(' '))),
            (
                key(KeyCode::Backspace, NONE),
                Action::Edit(EditKey::Backspace),
            ),
            (key(Char('a'), CTRL), Action::Edit(EditKey::LineStart)),
            (key(Char('k'), CTRL), Action::Edit(EditKey::KillEnd)),
            (key(KeyCode::Left, NONE), Action::Edit(EditKey::Left)),
        ] {
            assert_eq!(
                keys.action(event, Overlay::List, now),
                keys.action(event, Overlay::None, now),
                "{event:?} maps as it does with no overlay"
            );
            assert_eq!(keys.action(event, Overlay::List, now), want);
        }
    }

    #[test]
    fn a_sheet_takes_its_keys_and_ignores_the_rest() {
        let mut keys = Keys::new();
        let now = Instant::now();
        for (event, want) in [
            (key(KeyCode::Up, NONE), Action::Move(-1)),
            (key(KeyCode::Down, NONE), Action::Move(1)),
            (key(KeyCode::PageUp, NONE), Action::Move(-10)),
            (key(KeyCode::PageDown, NONE), Action::Move(10)),
            (key(KeyCode::Enter, NONE), Action::Pick),
            (key(KeyCode::Esc, NONE), Action::Close),
            (key(KeyCode::Tab, NONE), Action::Complete),
            (key(Char('a'), NONE), Action::Edit(EditKey::Insert('a'))),
            (key(Char('A'), SHIFT), Action::Edit(EditKey::Insert('A'))),
            (key(Char('世'), NONE), Action::Edit(EditKey::Insert('世'))),
            (
                key(KeyCode::Backspace, NONE),
                Action::Edit(EditKey::Backspace),
            ),
            (key(KeyCode::Left, NONE), Action::Ignore),
            (key(Char('b'), ALT), Action::Ignore),
            (key(Char('a'), CTRL), Action::Ignore),
            (key(KeyCode::F(1), NONE), Action::Ignore),
            (key(Char('c'), CTRL), Action::QuitArmed),
        ] {
            assert_eq!(
                keys.action(event, Overlay::Sheet, now),
                want,
                "{event:?} in a sheet"
            );
        }
        assert_eq!(
            keys.action(key(Char('c'), CTRL), Overlay::Sheet, now),
            Action::Quit,
            "Ctrl+C keeps its quit meaning in a sheet"
        );
    }

    #[test]
    fn the_status_line_shows_three_keys() {
        let rows: Vec<(&str, &str)> = STATUS.iter().map(|hint| (hint.keys, hint.words)).collect();
        assert_eq!(
            rows,
            vec![("ctrl+x /", "commands"), ("ctrl+x", "more"), ("?", "keys")]
        );
    }

    /// Every leader row of the table is a key that works: the leader resolves it
    /// to the command the row promises.
    #[test]
    fn every_leader_row_resolves_through_the_leader() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let rows: Vec<&Hint> = hints(Group::Leader).collect();
        assert_eq!(rows.len(), 6, "the leader has six rows");
        let mut resolved = Vec::new();
        for row in rows {
            let second = match row.keys {
                "tab" => key(KeyCode::Tab, NONE),
                keys => key(Char(keys.chars().next().expect("one key in the row")), NONE),
            };
            assert_eq!(
                keys.action(key(Char('x'), CTRL), Overlay::None, now),
                Action::Ignore
            );
            assert_eq!(
                keys.action(second, Overlay::None, now),
                Action::Ignore,
                "{} is a command, not text",
                row.keys
            );
            resolved.push(
                keys.take_leader()
                    .unwrap_or_else(|| panic!("{} is a leader key", row.keys)),
            );
        }
        assert_eq!(
            resolved,
            vec![
                Leader::ToggleSidebar,
                Leader::Sessions,
                Leader::Model,
                Leader::Level,
                Leader::FocusSidebar,
                Leader::Commands,
            ]
        );
    }

    /// Every table row fits the status line's run, and the table has a row for
    /// each group the help sheet lists.
    #[test]
    fn the_table_fits_its_runs() {
        for row in HINTS {
            let width = row.keys.chars().count() + 1 + row.words.chars().count();
            assert!(width <= 30, "{} {} is {width} columns", row.keys, row.words);
        }
        for (group, rows) in [
            (Group::TextBox, 22),
            (Group::Transcript, 4),
            (Group::Leader, 6),
        ] {
            assert_eq!(hints(group).count(), rows, "{group:?} lists {rows} rows");
        }
        assert_eq!(
            hints(Group::TextBox).count()
                + hints(Group::Transcript).count()
                + hints(Group::Leader).count(),
            HINTS.len(),
            "the groups hold every row"
        );
    }

    /// Ctrl+D deletes in a sheet and deletes a character outside one.
    #[test]
    fn ctrl_d_in_a_sheet_is_the_delete_key() {
        let mut keys = Keys::new();
        let now = Instant::now();
        assert_eq!(
            keys.action(key(Char('d'), CTRL), Overlay::Sheet, now),
            Action::Delete
        );
        assert_eq!(
            keys.action(key(Char('d'), CTRL), Overlay::None, now),
            Action::Edit(EditKey::DeleteChar)
        );
        assert_eq!(
            keys.action(key(Char('d'), CTRL), Overlay::List, now),
            Action::Edit(EditKey::DeleteChar)
        );
    }

    #[test]
    fn a_release_is_not_a_press() {
        let mut keys = Keys::new();
        let now = Instant::now();
        let mut event = key(KeyCode::Enter, NONE);
        event.kind = KeyEventKind::Release;
        assert_eq!(keys.action(event, Overlay::None, now), Action::Ignore);
    }
}

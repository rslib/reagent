//! The terminal's input bytes, turned into [`Event`]s.
//!
//! Ported from crossterm 0.29.0, `src/event/sys/unix/parse.rs`
//! (<https://github.com/crossterm-rs/crossterm>), which is MIT licensed,
//! Copyright (c) 2019 Timon. The license text:
//!
//! > Permission is hereby granted, free of charge, to any person obtaining a
//! > copy of this software and associated documentation files (the
//! > "Software"), to deal in the Software without restriction, including
//! > without limitation the rights to use, copy, modify, merge, publish,
//! > distribute, sublicense, and/or sell copies of the Software, and to permit
//! > persons to whom the Software is furnished to do so, subject to the
//! > following conditions: The above copyright notice and this permission
//! > notice shall be included in all copies or substantial portions of the
//! > Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
//!
//! It answers the same [`crossterm::event`] types the app already uses, so the
//! rest of the TUI is unchanged. Three things never reach the keys, because a
//! report would turn into a keypress nobody typed: a cursor position report is
//! dropped, the answer to the primary device attributes query the first frame
//! sends is [`Signal::DeviceAttributes`], which ends its wait, and the answer to
//! the kitty graphics query it sends inside herdr is [`Signal::Graphics`].
//!
//! What is not as crossterm wrote it: crossterm parses one sequence out of a
//! buffer it clears after each event, so it may build a `String` per CSI
//! parameter list and `split` it. Input runs per key and per mouse move, so
//! this parser works on borrowed bytes of one buffer, reads the parameters with
//! a small integer loop into an array on the stack, and reports how many bytes
//! each event took instead of clearing what follows it.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode,
    ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
};

use crate::theme::Rgb;

/// How many bytes the parser's buffer holds before it grows: one read fills it,
/// and a 24 KB burst is about three of them.
const BUFFER: usize = 8 * 1024;

/// The most bytes of an operating system command this parser holds before it
/// drops the rest: a command that never ends must not hold the keys behind it.
/// The longest answer a terminal sends is `OSC 11;rgb:` with four hex digits a
/// channel, under thirty bytes.
const OSC_MAX: usize = 256;

/// The most bytes of an application program command this parser holds before it
/// drops the rest, by the same rule [`OSC_MAX`] follows: a command that never
/// ends must not hold the keys behind it. The answer this parser reads is the
/// kitty graphics one, a few dozen bytes.
const APC_MAX: usize = OSC_MAX;

/// How many numbers one CSI sequence may carry: a kitty key is
/// `code:alternate ; modifiers:kind ; text`.
const PARAMS: usize = 8;

/// The bytes of the terminal's input, and the parser's place in them.
///
/// Nothing here allocates per read, per sequence or per event: the reader reads
/// into [`Parser::tail`], the parser reads borrowed bytes of it. The one
/// exception is the [`String`] of an [`Event::Paste`], which its type requires.
#[derive(Debug)]
pub struct Parser {
    /// The bytes read and not yet parsed, and the room for the next read.
    buf: Vec<u8>,
    /// How many bytes of `buf` hold input.
    filled: usize,
    /// Where the next event starts in `buf`.
    at: usize,
    /// Whether the last read filled the room it was given, so more bytes may be
    /// behind it: a lone `ESC` at the end of the buffer is then the start of a
    /// sequence, not the Esc key.
    more: bool,
}

impl Default for Parser {
    fn default() -> Parser {
        Parser::new()
    }
}

impl Parser {
    /// An empty parser, its buffer ready for the first read.
    pub fn new() -> Parser {
        Parser {
            buf: vec![0; BUFFER],
            filled: 0,
            at: 0,
            more: false,
        }
    }

    /// The room for the next read.
    ///
    /// The parsed prefix moves to the front once it is half of the buffer, so
    /// the copy is paid for by the bytes that caused it and no event rescans
    /// the buffer, and a sequence longer than the buffer (a big paste) doubles
    /// it.
    pub fn tail(&mut self) -> &mut [u8] {
        if self.at == self.filled {
            self.at = 0;
            self.filled = 0;
        } else if self.at >= self.buf.len() / 2 {
            self.buf.copy_within(self.at..self.filled, 0);
            self.filled -= self.at;
            self.at = 0;
        }
        if self.filled == self.buf.len() {
            let room = self.buf.len();
            self.buf.resize(room * 2, 0);
        }
        &mut self.buf[self.filled..]
    }

    /// Takes the bytes a read left in the room [`Parser::tail`] gave it.
    ///
    /// `full` says that read filled the room, so more bytes may be waiting
    /// behind it.
    pub fn commit(&mut self, read: usize, full: bool) {
        debug_assert!(read <= self.buf.len() - self.filled, "more read than room");
        self.filled += read;
        self.more = full;
    }

    /// The next whole event, if the bytes read so far hold one.
    ///
    /// A partial sequence stays in the buffer and waits for the next read: this
    /// never asks for bytes by itself and never blocks.
    pub fn next_event(&mut self) -> Option<Signal> {
        loop {
            if self.at == self.filled {
                self.at = 0;
                self.filled = 0;
                return None;
            }
            match step(&self.buf[self.at..self.filled], self.more) {
                Step::Event(event, len) => {
                    self.at += len.max(1);
                    return Some(Signal::Event(event));
                }
                Step::Background(rgb, len) => {
                    self.at += len.max(1);
                    return Some(Signal::Background(rgb));
                }
                Step::DeviceAttributes(len) => {
                    self.at += len.max(1);
                    return Some(Signal::DeviceAttributes);
                }
                Step::Graphics(yes, len) => {
                    self.at += len.max(1);
                    return Some(Signal::Graphics(yes));
                }
                Step::ProgramStatus(len) => {
                    self.at += len.max(1);
                    return Some(Signal::ProgramStatus);
                }
                Step::Skip(len) => self.at += len.max(1),
                Step::Wait => return None,
            }
        }
    }
}

/// What one thing the terminal's bytes said: an event the app handles, or the
/// answer to one of the first frame's queries.
///
/// None of the answers is a key and none reaches the key handling: the reader
/// turns the background into the color the app picks its mode from, the device
/// attributes into the end of the first frame's wait, the graphics answer into
/// whether the terminal draws placeholders and the program status answer into
/// whether the app reports the session's status at all, and every byte of all
/// four is dropped from the buffer it was parsed out of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Signal {
    /// A key, a mouse report, a focus change.
    Event(Event),
    /// The background the terminal reported for `OSC 11`.
    Background(Rgb),
    /// The terminal's answer to the primary device attributes query, `CSI c`:
    /// every terminal answers it, so it ends the first frame's wait.
    DeviceAttributes,
    /// The terminal's answer to the kitty graphics query the first frame sends
    /// inside a herdr pane: `true` when it reads the protocol, `false` when it
    /// answered an error.
    Graphics(bool),
    /// The terminal's answer to the program status query, `OSC 7501`: it reads
    /// the protocol, so the app reports the session's status to it and to no
    /// other terminal.
    ProgramStatus,
}

/// What one look at the input bytes found.
enum Step {
    /// A whole event, and how many bytes it took.
    Event(Event, usize),
    /// The answer to the background query, and how many bytes it took.
    Background(Rgb, usize),
    /// The answer to the device attributes query, and how many bytes it took.
    DeviceAttributes(usize),
    /// The answer to the kitty graphics query, and how many bytes it took.
    Graphics(bool, usize),
    /// The answer to the program status query, and how many bytes it took.
    ProgramStatus(usize),
    /// The bytes end in a partial sequence: wait for the next read.
    Wait,
    /// Bytes that are not an event: drop them and look at what follows.
    Skip(usize),
}

/// Reads one event from `buffer`, which starts where the last one ended.
fn step(buffer: &[u8], more: bool) -> Step {
    let Some(&first) = buffer.first() else {
        return Step::Wait;
    };
    match first {
        b'\x1b' => escape(buffer, more),
        b'\r' => Step::Event(Event::Key(KeyCode::Enter.into()), 1),
        // A newline is Ctrl+J: the terminal turns a carriage return into one
        // only in cooked mode, and this reads a raw terminal.
        b'\t' => Step::Event(Event::Key(KeyCode::Tab.into()), 1),
        b'\x7f' => Step::Event(Event::Key(KeyCode::Backspace.into()), 1),
        byte @ b'\x01'..=b'\x1a' => Step::Event(
            Event::Key(KeyEvent::new(
                KeyCode::Char(char::from(byte - 0x1 + b'a')),
                KeyModifiers::CONTROL,
            )),
            1,
        ),
        byte @ b'\x1c'..=b'\x1f' => Step::Event(
            Event::Key(KeyEvent::new(
                KeyCode::Char(char::from(byte - 0x1c + b'4')),
                KeyModifiers::CONTROL,
            )),
            1,
        ),
        b'\0' => Step::Event(
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            1,
        ),
        _ => utf8(buffer),
    }
}

/// `ESC` and what follows it: the Esc key, a sequence, or a key with Alt held.
fn escape(buffer: &[u8], more: bool) -> Step {
    if buffer.len() == 1 {
        // A lone ESC is the Esc key, unless the read that ended the buffer was
        // full: then a sequence may still be on its way, and the next read
        // decides.
        return if more {
            Step::Wait
        } else {
            Step::Event(Event::Key(KeyCode::Esc.into()), 1)
        };
    }
    match buffer[1] {
        b'O' => ss3(buffer),
        b'[' => csi(buffer),
        // An operating system command: the answer to the background query is
        // the one this parser keeps, and the rest are dropped. A command names
        // its number first, so a bracket with no digit behind it is Alt+] and
        // the keys after it, not a command that swallows them.
        b']' if buffer.len() == 2 => {
            if more {
                Step::Wait
            } else {
                Step::Event(
                    Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)),
                    2,
                )
            }
        }
        b']' if buffer[2].is_ascii_digit() => osc(buffer),
        // An application program command: the kitty graphics answer starts with
        // `ESC _ G`. A lone `ESC _` at the end of a full read may be its front,
        // and one that is not followed by `G` is Alt+_ and the keys after it.
        b'_' if buffer.len() > 2 && buffer[2] == b'G' => apc(buffer),
        b'_' if buffer.len() == 2 && more => Step::Wait,
        // Two escapes in a row: the first is the key, and the second is looked
        // at again on its own.
        b'\x1b' => Step::Event(Event::Key(KeyCode::Esc.into()), 1),
        _ => match step(&buffer[1..], more) {
            // `ESC` then a key is that key with Alt held.
            Step::Event(Event::Key(mut key), len) => {
                key.modifiers |= KeyModifiers::ALT;
                Step::Event(Event::Key(key), len + 1)
            }
            Step::Event(event, len) => Step::Event(event, len + 1),
            // An answer is never a key with Alt held on it, but the match has
            // to name every arm.
            Step::Background(rgb, len) => Step::Background(rgb, len + 1),
            Step::DeviceAttributes(len) => Step::DeviceAttributes(len + 1),
            Step::Graphics(yes, len) => Step::Graphics(yes, len + 1),
            Step::ProgramStatus(len) => Step::ProgramStatus(len + 1),
            Step::Wait => Step::Wait,
            Step::Skip(len) => Step::Skip(len + 1),
        },
    }
}

/// `ESC O` and one byte: the keys an application mode terminal sends.
fn ss3(buffer: &[u8]) -> Step {
    if buffer.len() == 2 {
        return Step::Wait;
    }
    let code = match buffer[2] {
        b'D' => KeyCode::Left,
        b'C' => KeyCode::Right,
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        // F1 to F4.
        val @ b'P'..=b'S' => KeyCode::F(1 + val - b'P'),
        _ => return Step::Skip(3),
    };
    Step::Event(Event::Key(code.into()), 3)
}

/// `ESC ]` and the operating system command it starts, up to BEL or ST.
///
/// The answer to the program status query becomes [`Signal::ProgramStatus`] and
/// the answer to the background query [`Signal::Background`]; every other
/// command, and an answer this parser does not understand, is dropped whole, so
/// no byte of it is read as a key. A command that runs past [`OSC_MAX`] bytes is
/// dropped too: the keys behind a terminator that never comes must arrive.
fn osc(buffer: &[u8]) -> Step {
    let mut at = 2;
    let (end, len) = loop {
        if at >= OSC_MAX {
            return Step::Skip(at);
        }
        match buffer.get(at) {
            None => return Step::Wait,
            Some(b'\x07') => break (at, at + 1),
            Some(b'\x1b') => match buffer.get(at + 1) {
                None => return Step::Wait,
                Some(b'\\') => break (at, at + 2),
                // An escape that is not ST ends this command as far as this
                // parser cares: what follows is looked at on its own.
                Some(_) => return Step::Skip(at),
            },
            Some(_) => at += 1,
        }
    };
    let body = &buffer[2..end];
    // A terminal that knows the program status protocol answers its query with
    // `7501;?` and the states and kinds it reads: the app reports to it alone.
    if body.starts_with(b"7501;?") {
        return Step::ProgramStatus(len);
    }
    match background(body) {
        Some(rgb) => Step::Background(rgb, len),
        None => Step::Skip(len),
    }
}

/// `ESC _ G` and the kitty graphics answer it starts, up to ST.
///
/// The answer to the query the first frame sends inside a herdr pane names the
/// image it asked about (`i=31`): `OK` says the terminal reads the protocol, and
/// any other payload is its error. Every other application program command, and
/// an answer that runs past [`APC_MAX`] bytes, is dropped whole, so no byte of
/// one is read as a key.
fn apc(buffer: &[u8]) -> Step {
    let mut at = 2;
    let (end, len) = loop {
        if at >= APC_MAX {
            return Step::Skip(at);
        }
        match buffer.get(at) {
            None => return Step::Wait,
            Some(b'\x1b') => match buffer.get(at + 1) {
                None => return Step::Wait,
                Some(b'\\') => break (at, at + 2),
                // An escape that is not ST ends this command as far as this
                // parser cares: what follows is looked at on its own.
                Some(_) => return Step::Skip(at),
            },
            Some(_) => at += 1,
        }
    };
    match graphics(&buffer[2..end]) {
        Some(yes) => Step::Graphics(yes, len),
        None => Step::Skip(len),
    }
}

/// The answer the kitty command `body`, from its `G`, carries for image 31:
/// whether the terminal read the protocol. `None` for a command that answers
/// another query of the app's, which is dropped.
fn graphics(body: &[u8]) -> Option<bool> {
    let body = body.strip_prefix(b"G")?;
    let at = body.iter().position(|byte| *byte == b';')?;
    let (keys, payload) = (&body[..at], &body[at + 1..]);
    let asked = keys.split(|byte| *byte == b',').any(|key| key == b"i=31");
    asked.then_some(payload == b"OK")
}

/// The color of an `OSC 11` answer, `11;rgb:red/green/blue`, each channel one to
/// four hex digits: the form xterm, kitty and iTerm2 answer with.
fn background(body: &[u8]) -> Option<Rgb> {
    let body = body.strip_prefix(b"11;rgb:")?;
    let mut channels = body.split(|byte| *byte == b'/');
    let red = channel(channels.next()?)?;
    let green = channel(channels.next()?)?;
    let blue = channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some(Rgb::new(red, green, blue))
}

/// One channel of an `rgb:` answer, scaled to eight bits: `f` is white,
/// `ffff` is white, and `80` is half, as `XParseColor` reads them.
fn channel(digits: &[u8]) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let mut value = 0u32;
    for digit in digits {
        value = value * 16 + (*digit as char).to_digit(16)?;
    }
    let full = (1u32 << (4 * digits.len() as u32)) - 1;
    Some(((value * 255 + full / 2) / full) as u8)
}

/// `ESC [` and the sequence it starts.
fn csi(buffer: &[u8]) -> Step {
    if buffer.len() == 2 {
        return Step::Wait;
    }
    match buffer[2] {
        // `ESC [ [ A` to `ESC [ [ E`: F1 to F5, the older encoding.
        b'[' => {
            if buffer.len() == 3 {
                return Step::Wait;
            }
            match buffer[3] {
                val @ b'A'..=b'E' => Step::Event(Event::Key(KeyCode::F(1 + val - b'A').into()), 4),
                _ => Step::Skip(4),
            }
        }
        b'D' => Step::Event(Event::Key(KeyCode::Left.into()), 3),
        b'C' => Step::Event(Event::Key(KeyCode::Right.into()), 3),
        b'A' => Step::Event(Event::Key(KeyCode::Up.into()), 3),
        b'B' => Step::Event(Event::Key(KeyCode::Down.into()), 3),
        b'H' => Step::Event(Event::Key(KeyCode::Home.into()), 3),
        b'F' => Step::Event(Event::Key(KeyCode::End.into()), 3),
        b'Z' => Step::Event(
            Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            3,
        ),
        b'M' => normal_mouse(buffer),
        b'<' => sgr_mouse(buffer),
        b'I' => Step::Event(Event::FocusGained, 3),
        b'O' => Step::Event(Event::FocusLost, 3),
        // A private sequence: the answer to the device attributes query
        // (`CSI ? … c`), which ends the first frame's wait, or the keyboard
        // enhancement flags answer (`CSI ? flags u`), which is dropped.
        b'?' => private(buffer),
        // Kitty sends the function keys without the `1` when no modifier is
        // held: `CSI P` is F1, `CSI Q` F2, `CSI S` F4.
        b'P' => Step::Event(Event::Key(KeyCode::F(1).into()), 3),
        b'Q' => Step::Event(Event::Key(KeyCode::F(2).into()), 3),
        b'S' => Step::Event(Event::Key(KeyCode::F(4).into()), 3),
        // A sequence with parameters: `CSI 1;2A`, `CSI 3~`, `CSI 97;5u`.
        b'0'..=b'9' | b';' => parameters(buffer),
        _ => Step::Skip(3),
    }
}

/// `ESC [` and numbers to the first final byte of the sequence.
fn parameters(buffer: &[u8]) -> Step {
    let Some(end) = final_byte(buffer, 2) else {
        return Step::Wait;
    };
    let Some(params) = Params::parse(&buffer[2..end]) else {
        return Step::Skip(end + 1);
    };
    let len = end + 1;
    match buffer[end] {
        b'~' if params.count == 1 && params.main[0] == 200 => paste(buffer),
        b'~' if params.group(0) == Some(27) => other_key(&params, len),
        b'~' => special_key(&params, len),
        b'u' => unicode_key(&params, len),
        b'M' => rxvt_mouse(&params, len),
        // A cursor position report, `CSI row ; column R`: dropped, since the
        // app never asks where the cursor is.
        b'R' => Step::Skip(len),
        key @ (b'A' | b'B' | b'C' | b'D' | b'F' | b'H' | b'P' | b'Q' | b'S') => {
            modifier_key(&params, key, len)
        }
        _ => Step::Skip(len),
    }
}

/// `ESC [ ? …`: the answer to the primary device attributes query becomes
/// [`Signal::DeviceAttributes`], and every other private sequence is dropped
/// whole, up to and including its final byte.
///
/// The parameters are never read: the final byte `c` is the answer, so the
/// answer costs no parsing and no allocation.
fn private(buffer: &[u8]) -> Step {
    match final_byte(buffer, 2) {
        Some(end) if buffer[end] == b'c' => Step::DeviceAttributes(end + 1),
        Some(end) => Step::Skip(end + 1),
        None => Step::Wait,
    }
}

/// Where the final byte of the sequence that starts at `from` is.
///
/// A CSI sequence runs from 0x40 to 0x7e once its parameters, which are all
/// below 0x40, are over.
fn final_byte(buffer: &[u8], from: usize) -> Option<usize> {
    let rest = buffer.get(from..)?;
    let at = rest.iter().position(|byte| (0x40..=0x7e).contains(byte))?;
    Some(from + at)
}

/// `CSI 1 ; modifier [: kind] X`: an arrow, Home, End or a function key with
/// modifiers held.
fn modifier_key(params: &Params, key: u8, len: usize) -> Step {
    let code = match key {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return Step::Skip(len),
    };
    // `CSI 1 ; 2 A` carries the modifiers in the second number; some terminals
    // send `CSI 2 D` with only that number.
    let (modifiers, kind) = match modifier_and_kind(params, 1) {
        Some((mask, kind)) => (parse_modifiers(mask), parse_kind(kind)),
        None => match params.group(0) {
            Some(mask) => (
                parse_modifiers(u8::try_from(mask).unwrap_or(0)),
                KeyEventKind::Press,
            ),
            None => (KeyModifiers::NONE, KeyEventKind::Press),
        },
    };
    Step::Event(
        Event::Key(KeyEvent::new_with_kind(code, modifiers, kind)),
        len,
    )
}

/// `CSI number ~`: Insert, Delete, PageUp, PageDown, Home, End and F5 to F12.
fn special_key(params: &Params, len: usize) -> Step {
    let Some(first) = params.group(0) else {
        return Step::Skip(len);
    };
    let (modifiers, kind, state) = match modifier_and_kind(params, 1) {
        Some((mask, kind)) => (parse_modifiers(mask), parse_kind(kind), parse_state(mask)),
        None => (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE),
    };
    let code = match first {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        val @ 11..=15 => KeyCode::F((val - 10) as u8),
        val @ 17..=21 => KeyCode::F((val - 11) as u8),
        val @ 23..=26 => KeyCode::F((val - 12) as u8),
        val @ 28..=29 => KeyCode::F((val - 15) as u8),
        val @ 31..=34 => KeyCode::F((val - 17) as u8),
        _ => return Step::Skip(len),
    };
    Step::Event(
        Event::Key(KeyEvent::new_with_kind_and_state(
            code, modifiers, kind, state,
        )),
        len,
    )
}

/// The key a character code of `CSI u` or `CSI 27 ~` names.
fn text_key(c: char, modifiers: KeyModifiers) -> KeyCode {
    match c {
        '\x1b' => KeyCode::Esc,
        '\r' => KeyCode::Enter,
        '\t' if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        '\t' => KeyCode::Tab,
        '\x7f' => KeyCode::Backspace,
        c => KeyCode::Char(c),
    }
}

/// `CSI 27 ; modifiers ; code ~`: a key with modifiers in xterm's
/// modifyOtherKeys form, such as Shift+Enter.
fn other_key(params: &Params, len: usize) -> Step {
    let (Some(mask), Some(c)) = (
        params.group(1).and_then(|mask| u8::try_from(mask).ok()),
        params.group(2).and_then(char::from_u32),
    ) else {
        return Step::Skip(len);
    };
    let modifiers = parse_modifiers(mask);
    Step::Event(
        Event::Key(KeyEvent::new(text_key(c, modifiers), modifiers)),
        len,
    )
}

/// `CSI code [: alternate] ; modifiers:kind ; text u`: the keys of the kitty
/// keyboard protocol and of `CSI u`.
fn unicode_key(params: &Params, len: usize) -> Step {
    let Some(codepoint) = params.group(0) else {
        return Step::Skip(len);
    };
    let (mut modifiers, kind, state) = match modifier_and_kind(params, 1) {
        Some((mask, kind)) => (parse_modifiers(mask), parse_kind(kind), parse_state(mask)),
        None => (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE),
    };
    let (mut code, from_code) = match functional_key(codepoint) {
        Some(found) => found,
        None => match char::from_u32(codepoint) {
            Some(c) => (text_key(c, modifiers), KeyEventState::empty()),
            None => return Step::Skip(len),
        },
    };
    if let KeyCode::Modifier(modifier) = code {
        match modifier {
            ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => {
                modifiers.set(KeyModifiers::ALT, true);
            }
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => {
                modifiers.set(KeyModifiers::CONTROL, true);
            }
            ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => {
                modifiers.set(KeyModifiers::SHIFT, true);
            }
            ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => {
                modifiers.set(KeyModifiers::SUPER, true);
            }
            ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => {
                modifiers.set(KeyModifiers::HYPER, true);
            }
            ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => {
                modifiers.set(KeyModifiers::META, true);
            }
            _ => {}
        }
    }
    // With the alternate keys reported, a Shift key carries the character the
    // layout gives with Shift: `CSI 57:40;4u` is Alt and `(`.
    if modifiers.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = params.sub_of(0).and_then(char::from_u32)
    {
        code = KeyCode::Char(shifted);
        modifiers.set(KeyModifiers::SHIFT, false);
    }
    Step::Event(
        Event::Key(KeyEvent::new_with_kind_and_state(
            code,
            modifiers,
            kind,
            from_code | state,
        )),
        len,
    )
}

/// `CSI Cb ; Cx ; Cy M`, the older mouse report.
fn rxvt_mouse(params: &Params, len: usize) -> Step {
    let (Some(cb), Some(cx), Some(cy)) = (params.group(0), params.group(1), params.group(2)) else {
        return Step::Skip(len);
    };
    let Some(cb) = u8::try_from(cb).ok().and_then(|cb| cb.checked_sub(32)) else {
        return Step::Skip(len);
    };
    let Some((kind, modifiers)) = parse_button(cb) else {
        return Step::Skip(len);
    };
    // The upper left position of the terminal is 1,1: the app counts from 0.
    let (Some(column), Some(row)) = (u16::try_from(cx).ok(), u16::try_from(cy).ok()) else {
        return Step::Skip(len);
    };
    Step::Event(
        Event::Mouse(MouseEvent {
            kind,
            column: column.saturating_sub(1),
            row: row.saturating_sub(1),
            modifiers,
        }),
        len,
    )
}

/// `ESC [ M Cb Cx Cy`, the original six byte mouse report.
fn normal_mouse(buffer: &[u8]) -> Step {
    if buffer.len() < 6 {
        return Step::Wait;
    }
    let Some(cb) = buffer[3].checked_sub(32) else {
        return Step::Skip(6);
    };
    let Some((kind, modifiers)) = parse_button(cb) else {
        return Step::Skip(6);
    };
    Step::Event(
        Event::Mouse(MouseEvent {
            kind,
            column: u16::from(buffer[4].saturating_sub(32)).saturating_sub(1),
            row: u16::from(buffer[5].saturating_sub(32)).saturating_sub(1),
            modifiers,
        }),
        6,
    )
}

/// `ESC [ < Cb ; Cx ; Cy M` (press) or `m` (release), the mouse report that
/// mouse capture turns on.
fn sgr_mouse(buffer: &[u8]) -> Step {
    let Some(end) = final_byte(buffer, 3) else {
        return Step::Wait;
    };
    let len = end + 1;
    let release = match buffer[end] {
        b'M' => false,
        b'm' => true,
        _ => return Step::Skip(len),
    };
    let Some(params) = Params::parse(&buffer[3..end]) else {
        return Step::Skip(len);
    };
    let (Some(cb), Some(cx), Some(cy)) = (params.group(0), params.group(1), params.group(2)) else {
        return Step::Skip(len);
    };
    let Some(cb) = u8::try_from(cb).ok() else {
        return Step::Skip(len);
    };
    let Some((mut kind, modifiers)) = parse_button(cb) else {
        return Step::Skip(len);
    };
    // A release is the button it was pressed with, which only the lowercase
    // final byte of the report says.
    if release && let MouseEventKind::Down(button) = kind {
        kind = MouseEventKind::Up(button);
    }
    let (Some(column), Some(row)) = (u16::try_from(cx).ok(), u16::try_from(cy).ok()) else {
        return Step::Skip(len);
    };
    Step::Event(
        Event::Mouse(MouseEvent {
            kind,
            column: column.saturating_sub(1),
            row: row.saturating_sub(1),
            modifiers,
        }),
        len,
    )
}

/// The bytes between the start and the end marker of a bracketed paste.
fn paste(buffer: &[u8]) -> Step {
    // `ESC [ 200 ~` pasted text `ESC [ 201 ~`.
    const END: &[u8] = b"\x1b[201~";
    const START: usize = 6;
    let Some(at) = buffer
        .get(START..)
        .and_then(|rest| rest.windows(END.len()).position(|window| window == END))
    else {
        return Step::Wait;
    };
    // The one allocation input makes: `Event::Paste` holds a `String`, built
    // from the bytes in one pass.
    let text = String::from_utf8_lossy(&buffer[START..START + at]).into_owned();
    Step::Event(Event::Paste(text), START + at + END.len())
}

/// The first character of the buffer, which is not a control byte or an escape.
fn utf8(buffer: &[u8]) -> Step {
    // https://en.wikipedia.org/wiki/UTF-8#Description
    let width = match buffer[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Step::Skip(1),
    };
    let Some(bytes) = buffer.get(..width) else {
        return Step::Wait;
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Step::Skip(1);
    };
    match text.chars().next() {
        Some(c) => Step::Event(Event::Key(char_code_to_event(KeyCode::Char(c))), width),
        None => Step::Skip(1),
    }
}

/// The key code of a character event, with Shift for an uppercase character.
fn char_code_to_event(code: KeyCode) -> KeyEvent {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::empty(),
    };
    KeyEvent::new(code, modifiers)
}

/// The modifiers a `CSI` sequence's mask names, counted from 1.
fn parse_modifiers(mask: u8) -> KeyModifiers {
    let mask = mask.saturating_sub(1);
    let mut modifiers = KeyModifiers::empty();
    if mask & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if mask & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if mask & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    if mask & 8 != 0 {
        modifiers |= KeyModifiers::SUPER;
    }
    if mask & 16 != 0 {
        modifiers |= KeyModifiers::HYPER;
    }
    if mask & 32 != 0 {
        modifiers |= KeyModifiers::META;
    }
    modifiers
}

/// The lock states a `CSI` sequence's mask names.
fn parse_state(mask: u8) -> KeyEventState {
    let mask = mask.saturating_sub(1);
    let mut state = KeyEventState::empty();
    if mask & 64 != 0 {
        state |= KeyEventState::CAPS_LOCK;
    }
    if mask & 128 != 0 {
        state |= KeyEventState::NUM_LOCK;
    }
    state
}

/// The event kind a `CSI` sequence's kind code names.
fn parse_kind(kind: u8) -> KeyEventKind {
    match kind {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

/// The button, the modifiers and the direction in a mouse report's first
/// number.
fn parse_button(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    let kind = match (button, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some((kind, modifiers))
}

/// The key a kitty function key codepoint names: the keypad, the locks, the
/// media keys and the modifier keys have no character of their own.
fn functional_key(codepoint: u32) -> Option<(KeyCode, KeyEventState)> {
    let keypad = match codepoint {
        57399 => KeyCode::Char('0'),
        57400 => KeyCode::Char('1'),
        57401 => KeyCode::Char('2'),
        57402 => KeyCode::Char('3'),
        57403 => KeyCode::Char('4'),
        57404 => KeyCode::Char('5'),
        57405 => KeyCode::Char('6'),
        57406 => KeyCode::Char('7'),
        57407 => KeyCode::Char('8'),
        57408 => KeyCode::Char('9'),
        57409 => KeyCode::Char('.'),
        57410 => KeyCode::Char('/'),
        57411 => KeyCode::Char('*'),
        57412 => KeyCode::Char('-'),
        57413 => KeyCode::Char('+'),
        57414 => KeyCode::Enter,
        57415 => KeyCode::Char('='),
        57416 => KeyCode::Char(','),
        57417 => KeyCode::Left,
        57418 => KeyCode::Right,
        57419 => KeyCode::Up,
        57420 => KeyCode::Down,
        57421 => KeyCode::PageUp,
        57422 => KeyCode::PageDown,
        57423 => KeyCode::Home,
        57424 => KeyCode::End,
        57425 => KeyCode::Insert,
        57426 => KeyCode::Delete,
        57427 => KeyCode::KeypadBegin,
        _ => {
            let key = match codepoint {
                57358 => KeyCode::CapsLock,
                57359 => KeyCode::ScrollLock,
                57360 => KeyCode::NumLock,
                57361 => KeyCode::PrintScreen,
                57362 => KeyCode::Pause,
                57363 => KeyCode::Menu,
                57376..=57398 => KeyCode::F((13 + codepoint - 57376) as u8),
                57428 => KeyCode::Media(MediaKeyCode::Play),
                57429 => KeyCode::Media(MediaKeyCode::Pause),
                57430 => KeyCode::Media(MediaKeyCode::PlayPause),
                57431 => KeyCode::Media(MediaKeyCode::Reverse),
                57432 => KeyCode::Media(MediaKeyCode::Stop),
                57433 => KeyCode::Media(MediaKeyCode::FastForward),
                57434 => KeyCode::Media(MediaKeyCode::Rewind),
                57435 => KeyCode::Media(MediaKeyCode::TrackNext),
                57436 => KeyCode::Media(MediaKeyCode::TrackPrevious),
                57437 => KeyCode::Media(MediaKeyCode::Record),
                57438 => KeyCode::Media(MediaKeyCode::LowerVolume),
                57439 => KeyCode::Media(MediaKeyCode::RaiseVolume),
                57440 => KeyCode::Media(MediaKeyCode::MuteVolume),
                57441 => KeyCode::Modifier(ModifierKeyCode::LeftShift),
                57442 => KeyCode::Modifier(ModifierKeyCode::LeftControl),
                57443 => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
                57444 => KeyCode::Modifier(ModifierKeyCode::LeftSuper),
                57445 => KeyCode::Modifier(ModifierKeyCode::LeftHyper),
                57446 => KeyCode::Modifier(ModifierKeyCode::LeftMeta),
                57447 => KeyCode::Modifier(ModifierKeyCode::RightShift),
                57448 => KeyCode::Modifier(ModifierKeyCode::RightControl),
                57449 => KeyCode::Modifier(ModifierKeyCode::RightAlt),
                57450 => KeyCode::Modifier(ModifierKeyCode::RightSuper),
                57451 => KeyCode::Modifier(ModifierKeyCode::RightHyper),
                57452 => KeyCode::Modifier(ModifierKeyCode::RightMeta),
                57453 => KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift),
                57454 => KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift),
                _ => return None,
            };
            return Some((key, KeyEventState::empty()));
        }
    };
    Some((keypad, KeyEventState::KEYPAD))
}

/// The numbers of a CSI sequence, read from its bytes.
///
/// A number per `;`-separated group in `main`, its `:`-separated
/// sub-parameter in `sub`: `CSI 57:40;4u` is `[[57, 40], [4]]`. The bytes are
/// read once, into a fixed array on the stack: no `String`, no `split`.
#[derive(Clone, Copy, Debug, Default)]
struct Params {
    /// The number of each group.
    main: [u32; PARAMS],
    /// The sub-parameter of each group.
    sub: [u32; PARAMS],
    /// A bit per group that carried a sub-parameter.
    subs: u8,
    /// How many groups there are: one when the sequence has no `;`.
    count: usize,
}

impl Params {
    /// Reads `1;2:3` into `main [1, 2]`, `sub [0, 3]`, `count 2`.
    ///
    /// `None` when a byte is not a digit, `;` or `:`, or a number runs past
    /// [`PARAMS`] groups or past `u32`.
    fn parse(bytes: &[u8]) -> Option<Params> {
        let mut params = Params {
            count: 1,
            ..Params::default()
        };
        let mut group = 0;
        let mut sub = false;
        for &byte in bytes {
            match byte {
                b'0'..=b'9' => {
                    let number = if sub {
                        &mut params.sub[group]
                    } else {
                        &mut params.main[group]
                    };
                    *number = number
                        .checked_mul(10)?
                        .checked_add(u32::from(byte - b'0'))?;
                }
                b';' => {
                    group += 1;
                    if group == PARAMS {
                        return None;
                    }
                    params.count = group + 1;
                    sub = false;
                }
                b':' => {
                    sub = true;
                    params.subs |= 1 << group;
                }
                _ => return None,
            }
        }
        Some(params)
    }

    /// The number of the group at `index`.
    fn group(&self, index: usize) -> Option<u32> {
        (index < self.count).then(|| self.main[index])
    }

    /// The sub-parameter of the group at `index`.
    fn sub_of(&self, index: usize) -> Option<u32> {
        (index < self.count && self.subs & (1 << index) != 0).then(|| self.sub[index])
    }
}

/// The modifiers mask and event kind of the group at `index`, as
/// `CSI ; modifiers:kind` sends them: the kind is a press when the sequence
/// leaves it out.
fn modifier_and_kind(params: &Params, index: usize) -> Option<(u8, u8)> {
    let mask = u8::try_from(params.group(index)?).ok()?;
    let kind = params
        .sub_of(index)
        .and_then(|kind| u8::try_from(kind).ok())
        .unwrap_or(1);
    Some((mask, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds one read of `bytes` and answers the events the bytes hold.
    ///
    /// `full` says the read filled the room it was given, so more bytes may be
    /// behind it: a lone `ESC` at the end of it is then not the Esc key.
    fn feed(parser: &mut Parser, bytes: &[u8], full: bool) -> Vec<Event> {
        let mut events = Vec::new();
        for signal in signals(parser, bytes, full) {
            match signal {
                Signal::Event(event) => events.push(event),
                Signal::Background(color) => panic!("{bytes:?} is a key: {color:?}"),
                Signal::DeviceAttributes => panic!("{bytes:?} is a key: the device attributes"),
                Signal::Graphics(reads) => {
                    panic!("{bytes:?} is a key: the graphics answer {reads}")
                }
                Signal::ProgramStatus => {
                    panic!("{bytes:?} is a key: the program status answer")
                }
            }
        }
        events
    }

    /// Feeds one read of `bytes` and answers everything the bytes say.
    fn signals(parser: &mut Parser, bytes: &[u8], full: bool) -> Vec<Signal> {
        let room = parser.tail();
        assert!(bytes.len() <= room.len(), "{bytes:?} fits in one read");
        room[..bytes.len()].copy_from_slice(bytes);
        parser.commit(bytes.len(), full);
        let mut signals = Vec::new();
        while let Some(signal) = parser.next_event() {
            signals.push(signal);
        }
        signals
    }

    /// The events of one whole read.
    fn read(bytes: &[u8]) -> Vec<Event> {
        feed(&mut Parser::new(), bytes, false)
    }

    /// The one event of one whole read.
    fn event(bytes: &[u8]) -> Event {
        let mut events = read(bytes);
        assert_eq!(events.len(), 1, "{bytes:?} is one event: {events:?}");
        events.remove(0)
    }

    /// The key of one whole read.
    fn key(bytes: &[u8]) -> KeyEvent {
        match event(bytes) {
            Event::Key(key) => key,
            other => panic!("{bytes:?} is a key: {other:?}"),
        }
    }

    /// The first character of the test's own UTF-8, for the non-ASCII cases.
    fn typed(text: &str) -> KeyEvent {
        assert_eq!(text.chars().count(), 1, "{text:?} is one character");
        key(text.as_bytes())
    }

    #[test]
    fn the_esc_key_is_esc() {
        assert_eq!(key(b"\x1B"), KeyCode::Esc.into());
        assert_eq!(key(b"\x1B"), KeyCode::Esc.into(), "one press, one key");
    }

    #[test]
    fn an_esc_that_may_start_a_sequence_waits_for_the_next_read() {
        let mut parser = Parser::new();
        assert_eq!(feed(&mut parser, b"\x1B", true), Vec::new());
        assert_eq!(
            feed(&mut parser, b"[A", false),
            vec![Event::Key(KeyCode::Up.into())]
        );
    }

    #[test]
    fn a_sequence_in_two_reads_is_one_event() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"\x1B[1;", true).is_empty());
        assert_eq!(
            feed(&mut parser, b"2A", false),
            vec![Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))]
        );
    }

    #[test]
    fn alt_keys() {
        assert_eq!(
            key(b"\x1Bc"),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)
        );
        assert_eq!(
            key(b"\x1BH"),
            KeyEvent::new(KeyCode::Char('H'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        );
        assert_eq!(
            key(b"\x1B\x14"),
            KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )
        );
    }

    #[test]
    fn the_control_bytes() {
        assert_eq!(key(b"\r"), KeyCode::Enter.into());
        assert_eq!(key(b"\t"), KeyCode::Tab.into());
        assert_eq!(key(b"\x7F"), KeyCode::Backspace.into());
        assert_eq!(
            key(b"\0"),
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(
            key(b"\x1C"),
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn a_newline_is_ctrl_j_in_a_raw_terminal() {
        // The terminal sends a newline for Enter only in cooked mode, and this
        // reads a raw one: a newline is the Ctrl+J key.
        assert_eq!(read(b"\r"), vec![Event::Key(KeyCode::Enter.into())]);
        assert_eq!(
            read(b"\n"),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL
            ))]
        );
    }

    #[test]
    fn the_arrows_and_the_function_keys() {
        assert_eq!(key(b"\x1B[D"), KeyCode::Left.into());
        assert_eq!(key(b"\x1BOD"), KeyCode::Left.into());
        assert_eq!(
            key(b"\x1B[1;2D"),
            KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)
        );
        assert_eq!(key(b"\x1BOP"), KeyCode::F(1).into());
        assert_eq!(key(b"\x1B[3~"), KeyCode::Delete.into());
        assert_eq!(key(b"\x1B[P"), KeyCode::F(1).into());
        assert_eq!(
            key(b"\x1B[Z"),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
    }

    #[test]
    fn a_key_with_modifiers_and_a_kind() {
        assert_eq!(
            key(b"\x1B[3;2~"),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT)
        );
        assert_eq!(
            key(b"\x1B[;1:3B"),
            KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release)
        );
        assert_eq!(
            key(b"\x1B[5;1:3~"),
            KeyEvent::new_with_kind(KeyCode::PageUp, KeyModifiers::NONE, KeyEventKind::Release)
        );
    }

    #[test]
    fn focus_events() {
        assert_eq!(read(b"\x1B[O"), vec![Event::FocusLost]);
        assert_eq!(read(b"\x1B[I"), vec![Event::FocusGained]);
    }

    #[test]
    fn a_paste_across_reads_is_one_event() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"\x1B[200~on and", true).is_empty());
        assert!(feed(&mut parser, b" on\x1B[2", true).is_empty());
        assert_eq!(
            feed(&mut parser, b"D\x1B[201~", false),
            vec![Event::Paste("on and on\x1B[2D".to_owned())]
        );
    }

    #[test]
    fn an_empty_paste_is_a_paste() {
        assert_eq!(
            read(b"\x1B[200~\x1B[201~"),
            vec![Event::Paste(String::new())]
        );
    }

    #[test]
    fn the_mouse_reports() {
        assert_eq!(
            event(b"\x1B[<0;20;10M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            event(b"\x1B[<0;20;10;m"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            event(b"\x1B[M0\x60\x70"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 63,
                row: 79,
                modifiers: KeyModifiers::CONTROL,
            })
        );
        assert_eq!(
            event(b"\x1B[32;30;40;M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 29,
                row: 39,
                modifiers: KeyModifiers::NONE,
            })
        );
    }

    #[test]
    fn a_mouse_report_in_two_reads_is_one_event() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"\x1B[<64;3", true).is_empty());
        assert_eq!(
            feed(&mut parser, b";4M", false),
            vec![Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 2,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })]
        );
    }

    #[test]
    fn a_unicode_key() {
        assert_eq!(
            key(b"\x1B[97;5u"),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            key(b"\x1B[97;1:2u"),
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat)
        );
        assert_eq!(
            key(b"\x1B[57399u"),
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char('0'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                KeyEventState::KEYPAD,
            )
        );
        assert_eq!(
            key(b"\x1B[57:40;4u"),
            KeyEvent::new(KeyCode::Char('('), KeyModifiers::ALT)
        );
        assert_eq!(
            key(b"\x1B[57449u"),
            KeyEvent::new(
                KeyCode::Modifier(ModifierKeyCode::RightAlt),
                KeyModifiers::ALT
            )
        );
        assert_eq!(
            key(b"\x1B[97;65u"),
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            )
        );
    }

    #[test]
    fn shift_enter_is_its_own_key_in_both_extended_forms() {
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(key(b"\x1B[13;2u"), shift_enter, "kitty and tmux csi-u");
        assert_eq!(key(b"\x1B[27;2;13~"), shift_enter, "xterm modifyOtherKeys");
        assert_eq!(
            key(b"\x1B[27;5;106~"),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            key(b"\x1B[27;3;127~"),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)
        );
    }

    #[test]
    fn the_utf8_characters() {
        assert_eq!(
            typed("Ž"),
            KeyEvent::new(KeyCode::Char('Ž'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            typed("ñ"),
            KeyEvent::new(KeyCode::Char('ñ'), KeyModifiers::NONE)
        );
        assert_eq!(
            typed("𐌼"),
            KeyEvent::new(KeyCode::Char('𐌼'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn a_character_split_across_reads_waits() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, &"Ž".as_bytes()[..1], true).is_empty());
        assert_eq!(
            feed(&mut parser, &"Ž".as_bytes()[1..], false),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('Ž'),
                KeyModifiers::SHIFT
            ))]
        );
    }

    #[test]
    fn bytes_that_are_not_a_sequence_are_dropped() {
        // Not a key, not a sequence: every byte is skipped, and the key after
        // them still arrives.
        assert_eq!(
            read(b"\x1B[?1u").len(),
            0,
            "an enhancement answer is dropped"
        );
        // A device attributes answer is no key either: it is the first frame's
        // own event, and its parameters are never read.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1B[?62;c", false),
            vec![Signal::DeviceAttributes],
            "the device attributes answer"
        );
        assert_eq!(
            read(b"\x1B[20;10R").len(),
            0,
            "and a cursor position report"
        );
        assert_eq!(
            read(b"\x1B[xa"),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))]
        );
    }

    #[test]
    fn a_byte_that_is_no_character_is_dropped_and_the_next_one_read() {
        // A leading continuation byte starts no character: it goes, and the
        // key behind it still arrives.
        assert_eq!(
            read(b"\x80a"),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))]
        );
    }

    #[test]
    fn a_burst_of_keys_larger_than_a_read_arrives_in_full() {
        // 2,000 one-line scroll pairs are 24 KB, more than a pty buffer, then
        // one printable key that only arrives if every byte before it was read.
        let mut input = b"\x1b[1;2A\x1b[1;2B".repeat(2_000);
        input.extend_from_slice(b"x");
        for chunk in [1, 7, 4096] {
            let pieces: Vec<&[u8]> = input.chunks(chunk).collect();
            let mut parser = Parser::new();
            let mut events = 0;
            let mut last = None;
            for (index, piece) in pieces.iter().enumerate() {
                // Every read but the last one has the bytes behind it that are
                // still to come.
                let full = index + 1 < pieces.len();
                for event in feed(&mut parser, piece, full) {
                    events += 1;
                    last = Some(event);
                }
            }
            assert_eq!(events, 4_001, "one event per key, in chunks of {chunk}");
            assert_eq!(
                last,
                Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE
                ))),
                "in chunks of {chunk}"
            );
        }
    }

    #[test]
    fn a_parsed_prefix_is_compacted_and_the_bytes_keep_arriving() {
        // Enough single keys to fill, compact and grow the buffer several
        // times over, with no key lost and no sequence cut in two.
        let mut parser = Parser::new();
        let mut keys = 0;
        for _ in 0..20_000 {
            keys += feed(&mut parser, b"a", false).len();
        }
        assert_eq!(keys, 20_000);
        for _ in 0..4_000 {
            keys += feed(&mut parser, b"\x1b[1;2A", false).len();
        }
        assert_eq!(keys, 24_000);
    }

    #[test]
    fn osc_11_answers_parse_to_the_background() {
        // The four-digit form xterm answers with, then the two- and one-digit
        // forms, ended by BEL, and the same by ST.
        let cases: [(&[u8], Rgb); 5] = [
            (
                b"\x1b]11;rgb:1414/1414/1414\x07",
                Rgb::new(0x14, 0x14, 0x14),
            ),
            (
                b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\",
                Rgb::new(0xff, 0xff, 0xff),
            ),
            (b"\x1b]11;rgb:1e/1e/1e\x07", Rgb::new(0x1e, 0x1e, 0x1e)),
            (b"\x1b]11;rgb:f/f/f\x07", Rgb::new(0xff, 0xff, 0xff)),
            (b"\x1b]11;rgb:0/0/0\x07", Rgb::new(0, 0, 0)),
        ];
        for (bytes, color) in cases {
            let mut parser = Parser::new();
            assert_eq!(
                signals(&mut parser, bytes, false),
                vec![Signal::Background(color)],
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn an_osc_11_answer_is_not_a_key_and_a_bad_one_is_nothing() {
        // A key typed right behind the answer is the only event: the answer
        // itself never reaches the key handling.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b]11;rgb:2121/2121/2121\x07a", false),
            vec![
                Signal::Background(Rgb::new(0x21, 0x21, 0x21)),
                Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE
                ))),
            ]
        );
        // An answer with a channel too long, too few channels, no `rgb:` and a
        // command this parser does not keep are all dropped whole, and a key
        // behind them still arrives.
        for bytes in [
            &b"\x1b]11;rgb:14141414/1/1\x07a"[..],
            b"\x1b]11;rgb:14/14\x07a",
            b"\x1b]11;rgba:14/14/14/14\x07a",
            b"\x1b]11;rgb:zz/14/14\x07a",
            b"\x1b]52;c;Zm9v\x07a",
            // A command that names a number this parser keeps nothing of is
            // still a command: `OSC 0` sets the window title.
            b"\x1b]0;a title\x07a",
        ] {
            let mut parser = Parser::new();
            assert_eq!(
                signals(&mut parser, bytes, false),
                vec![Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE
                )))],
                "{bytes:?} is no answer"
            );
        }
    }

    #[test]
    fn program_status_answers_parse_to_their_signal() {
        // The answer a terminal that knows the program status protocol gives,
        // ended by ST and by BEL: the app tells the session's status to a
        // terminal that answered and to no other.
        for bytes in [
            &b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\"[..],
            b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x07",
        ] {
            let mut parser = Parser::new();
            assert_eq!(
                signals(&mut parser, bytes, false),
                vec![Signal::ProgramStatus],
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn program_status_answer_split_over_two_reads_parses() {
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b]7501;?:states=idle,working", true),
            Vec::new(),
            "the answer waits for its terminator"
        );
        assert_eq!(
            signals(
                &mut parser,
                b",done,blocked,error:kinds=permission,question,auth\x1b\\",
                false
            ),
            vec![Signal::ProgramStatus]
        );
    }

    #[test]
    fn program_status_answer_is_not_a_key() {
        // The query of a terminal that asks, which the app writes and reads
        // nothing of, and a key right behind the answer.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b]7501;?:states=idle\x07a", false),
            vec![
                Signal::ProgramStatus,
                Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE
                ))),
            ]
        );
    }

    #[test]
    fn alt_and_a_bracket_is_a_key_and_not_an_osc() {
        // A command names its number first, so `ESC ]` and anything that is not
        // a digit is Alt+] and the keys after it, not a command that swallows
        // them until a BEL or an ST.
        assert_eq!(
            key(b"\x1b]"),
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)
        );
        assert_eq!(
            read(b"\x1b]j"),
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)),
                Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            ],
            "the keys after Alt+] arrive"
        );
        // The text behind Alt+] arrives as keys too, rather than being held for
        // a terminator the terminal never sends.
        assert_eq!(
            read(b"\x1b];z"),
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)),
                Event::Key(KeyEvent::new(KeyCode::Char(';'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)),
            ],
            "the text behind Alt+] arrives as keys"
        );
    }

    #[test]
    fn a_key_behind_alt_and_a_bracket_arrives() {
        // The key typed after Alt+] is not held until a BEL that never comes.
        let mut parser = Parser::new();
        assert_eq!(
            feed(&mut parser, b"\x1b]", false),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT
            ))],
            "a whole read of Alt+] is the key"
        );
        assert_eq!(
            feed(&mut parser, b"x", false),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE
            ))],
            "and the next key is its own"
        );
    }

    #[test]
    fn an_osc_that_never_ends_holds_no_keys() {
        // A command with no terminator is dropped at its cap, so the keys behind
        // it are read rather than held for a BEL that never comes.
        let mut parser = Parser::new();
        let mut command = Vec::from(&b"\x1b]11;rgb:"[..]);
        command.resize(OSC_MAX, b'1');
        let mut events = feed(&mut parser, &command, false);
        events.extend(feed(&mut parser, b"k", false));
        assert_eq!(
            events,
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE
            ))],
            "the unterminated command is dropped whole"
        );
    }

    #[test]
    fn an_osc_11_answer_split_over_two_reads_is_one_event() {
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b]11;rgb:14", true),
            Vec::new(),
            "half an answer waits"
        );
        assert_eq!(
            signals(&mut parser, b"14/1414/1414\x07", false),
            vec![Signal::Background(Rgb::new(0x14, 0x14, 0x14))]
        );
    }

    #[test]
    fn a_device_attributes_answer_is_not_a_key_and_a_keyboard_answer_is_not_it() {
        // The answer to the `CSI c` query the first frame sends, in the forms
        // terminals send it: it ends the first frame's wait and never reaches
        // the keys, whichever parameters it carries.
        for bytes in [
            &b"\x1b[?1;2c"[..],
            b"\x1b[?62;22c",
            b"\x1b[?64;1;2;6;9;15;18;21;22c",
            b"\x1b[?c",
        ] {
            let mut parser = Parser::new();
            assert_eq!(
                signals(&mut parser, bytes, false),
                vec![Signal::DeviceAttributes],
                "{bytes:?}"
            );
        }
        // The keyboard enhancement flags answer (`CSI ? … u`) is still dropped
        // whole and is not the device answer, and a key behind both answers is
        // the only key.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b[?1;2c\x1b[?1ua", false),
            vec![
                Signal::DeviceAttributes,
                Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE
                ))),
            ]
        );
    }

    #[test]
    fn a_device_attributes_answer_split_over_two_reads_is_one_event() {
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b[?1;", true),
            Vec::new(),
            "half an answer waits"
        );
        assert_eq!(
            signals(&mut parser, b"2c", false),
            vec![Signal::DeviceAttributes]
        );
    }

    #[test]
    fn alt_underscore_is_still_a_key() {
        // `ESC _` is not the front of an answer unless `G` follows it: it is
        // Alt+_ and the key after it, as it was.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b_x", false),
            vec![
                Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('_'),
                    KeyModifiers::ALT
                ))),
                Signal::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE
                ))),
            ]
        );
    }

    #[test]
    fn kitty_answer_ok_is_a_graphics_signal() {
        // The answer to the kitty graphics query the first frame sends inside
        // herdr: the terminal reads the protocol.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b_Gi=31;OK\x1b\\", false),
            vec![Signal::Graphics(true)]
        );
        assert_eq!(parser.next_event(), None, "the answer is nothing else");
    }

    #[test]
    fn kitty_answer_error_is_a_graphics_signal_false() {
        // A terminal that reads the protocol and cannot give the image says so
        // with an error: it answered no, which is not the same as no answer.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b_Gi=31;ENOENT:no such file\x1b\\", false),
            vec![Signal::Graphics(false)]
        );
        assert_eq!(parser.next_event(), None, "the answer is nothing else");
    }

    #[test]
    fn kitty_answer_split_across_reads_waits() {
        // Every cut of the answer parses to the one signal, whatever byte the
        // read ended on.
        let answer = b"\x1b_Gi=31;OK\x1b\\";
        for cut in 1..answer.len() {
            let mut parser = Parser::new();
            let mut all = signals(&mut parser, &answer[..cut], true);
            all.extend(signals(&mut parser, &answer[cut..], false));
            assert_eq!(all, vec![Signal::Graphics(true)], "cut at {cut}");
        }
    }

    #[test]
    fn kitty_answer_then_device_attributes() {
        // Inside herdr the kitty query goes out before `CSI c`, so a terminal
        // that answers both in one read answers them in that order.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c", false),
            vec![Signal::Graphics(true), Signal::DeviceAttributes]
        );
    }

    #[test]
    fn other_apc_is_dropped() {
        // An application program command that answers no query of ours is
        // dropped whole, so no byte of it is read as a key.
        let mut parser = Parser::new();
        assert_eq!(
            signals(&mut parser, b"\x1b_Gi=7;OK\x1b\\a", false),
            vec![Signal::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            )))]
        );
    }

    #[test]
    fn an_apc_that_never_ends_holds_no_keys() {
        // An answer with no terminator is dropped at its cap, the bound an
        // operating system command has, so the keys behind it arrive.
        let mut parser = Parser::new();
        let mut command = Vec::from(&b"\x1b_Gi=31;OK"[..]);
        command.resize(APC_MAX, b'1');
        let mut events = feed(&mut parser, &command, false);
        events.extend(feed(&mut parser, b"k", false));
        assert_eq!(
            events,
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE
            ))],
            "the unterminated answer is dropped whole"
        );
    }
}

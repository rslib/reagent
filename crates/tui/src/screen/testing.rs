use std::fmt;
use std::io;
use std::num::NonZeroU16;

use vt100::Parser;

use super::terminal::Terminal;

/// A terminal for tests: every frame goes through a `vt100` parser.
pub struct Vt100Terminal {
    parser: Parser,
    writes: usize,
    frames: Vec<Vec<u8>>,
}

impl Vt100Terminal {
    pub fn new(rows: u16, columns: u16, scrollback: usize) -> Self {
        Vt100Terminal {
            parser: Parser::new(nonzero(rows), nonzero(columns), scrollback),
            writes: 0,
            frames: Vec::new(),
        }
    }

    pub fn writes(&self) -> usize {
        self.writes
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn rows_text(&self, columns: u16) -> Vec<String> {
        self.parser.screen().rows(0, columns).collect()
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.frames.concat()
    }
}

impl Terminal for Vt100Terminal {
    fn size(&self) -> (u16, u16) {
        let (rows, columns) = self.parser.screen().size();
        (columns.get(), rows.get())
    }

    async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.writes += 1;
        self.parser.process(frame);
        self.frames.push(frame.to_vec());
        Ok(())
    }
}

impl fmt::Debug for Vt100Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vt100Terminal")
            .field("writes", &self.writes)
            .field("frames", &self.frames.len())
            .finish()
    }
}

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

/// The runs and the line ranges of plain lines, for [`Screen::commit`].
pub(crate) fn lines<'a>(
    texts: &'a [&'a str],
) -> (Vec<super::Run<'a>>, Vec<std::ops::Range<usize>>) {
    let runs = texts.iter().map(|text| super::Run::plain(text)).collect();
    let spans = (0..texts.len()).map(|index| index..index + 1).collect();
    (runs, spans)
}

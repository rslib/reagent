use std::collections::HashMap;
use std::sync::Arc;

/// Second cell of a character that takes two columns.
pub const CONTINUATION: u32 = u32::MAX;
/// High bit of a cell text: the grapheme lives in the arena, not in the cell.
pub const GRAPHEME_FLAG: u32 = 0x8000_0000;

/// One terminal cell: a `u32` text (a scalar value, an arena index, or the
/// continuation marker) and a `u32` interned style id.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    text: u32,
    style: u32,
}

const _: () = assert!(std::mem::size_of::<Cell>() == 8);

impl Cell {
    /// A space in the default style.
    pub const BLANK: Cell = Cell {
        text: b' ' as u32,
        style: 0,
    };

    /// A cell holding a raw text value (scalar value, arena index or marker).
    pub const fn new(text: u32, style: u32) -> Self {
        Cell { text, style }
    }

    /// A cell holding one scalar value.
    pub const fn from_char(c: char, style: u32) -> Self {
        Cell {
            text: c as u32,
            style,
        }
    }

    /// A cell pointing at a multi-codepoint grapheme in the arena.
    pub const fn grapheme(index: u32, style: u32) -> Self {
        Cell {
            text: GRAPHEME_FLAG | index,
            style,
        }
    }

    /// The second cell of a two-column character.
    pub const fn continuation(style: u32) -> Self {
        Cell {
            text: CONTINUATION,
            style,
        }
    }

    /// The cell as a `u64` for the per-cell diff.
    pub const fn bits(&self) -> u64 {
        ((self.text as u64) << 32) | self.style as u64
    }

    /// The interned style id.
    pub const fn style(&self) -> u32 {
        self.style
    }

    /// True for the second cell of a two-column character.
    pub const fn is_continuation(&self) -> bool {
        self.text == CONTINUATION
    }

    /// True for a cell that points into the grapheme arena.
    pub const fn is_grapheme(&self) -> bool {
        self.text & GRAPHEME_FLAG != 0 && self.text != CONTINUATION
    }

    /// The arena index of a grapheme cell.
    pub const fn grapheme_index(&self) -> u32 {
        self.text & !GRAPHEME_FLAG
    }

    /// The scalar value of a plain cell.
    pub fn char(&self) -> Option<char> {
        if self.is_grapheme() || self.is_continuation() {
            None
        } else {
            char::from_u32(self.text)
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::BLANK
    }
}

/// A terminal color.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Color {
    /// The terminal's own color.
    #[default]
    Default,
    /// A palette entry.
    Indexed(u8),
    /// A direct color.
    Rgb(u8, u8, u8),
}

/// A cell's rendering attributes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Bold.
    pub bold: bool,
    /// Dim.
    pub dim: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Strikethrough.
    pub strikethrough: bool,
    /// Reverse video.
    pub reverse: bool,
    /// Link id; 0 is no link.
    pub link: u32,
}

/// Interns styles so a frame reuses the id of every style it saw before.
#[derive(Debug)]
pub struct StyleTable {
    ids: HashMap<Style, u32>,
    styles: Vec<Style>,
}

impl StyleTable {
    /// A table holding only the default style at id 0.
    pub fn new() -> Self {
        let mut table = StyleTable {
            ids: HashMap::new(),
            styles: Vec::new(),
        };
        table.intern(Style::default());
        table
    }

    /// The id of a style, adding it the first time it is seen.
    pub fn intern(&mut self, style: Style) -> u32 {
        if let Some(&id) = self.ids.get(&style) {
            return id;
        }
        let id = self.styles.len() as u32;
        self.styles.push(style);
        self.ids.insert(style, id);
        id
    }

    /// The style behind an id; unknown ids read as the default style.
    pub fn get(&self, id: u32) -> Style {
        self.styles.get(id as usize).copied().unwrap_or_default()
    }

    /// The number of interned styles.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// True before any style is interned.
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }
}

impl Default for StyleTable {
    fn default() -> Self {
        StyleTable::new()
    }
}

/// Interns multi-codepoint graphemes so equal graphemes compare equal in the
/// diff and a seen grapheme costs no allocation. It grows while a frame draws
/// and is rebuilt from the two grids (see `Screen::compact_graphemes`) once it
/// passes `GRAPHEME_ARENA_CAP`, so a long session of changing text keeps the
/// arena bounded.
#[derive(Debug)]
pub struct GraphemeArena {
    ids: HashMap<Arc<str>, u32>,
    graphemes: Vec<Arc<str>>,
}

impl GraphemeArena {
    /// An empty arena.
    pub fn new() -> Self {
        GraphemeArena {
            ids: HashMap::new(),
            graphemes: Vec::new(),
        }
    }

    /// The index of a grapheme, adding it the first time it is seen.
    pub fn intern(&mut self, grapheme: &str) -> u32 {
        if let Some(&id) = self.ids.get(grapheme) {
            return id;
        }
        let text: Arc<str> = Arc::from(grapheme);
        let id = self.graphemes.len() as u32;
        self.ids.insert(text.clone(), id);
        self.graphemes.push(text);
        id
    }

    /// The grapheme behind an index; unknown indexes read as a space.
    pub fn get(&self, index: u32) -> &str {
        self.graphemes
            .get(index as usize)
            .map_or(" ", |text| &**text)
    }

    /// The number of interned graphemes.
    pub fn len(&self) -> usize {
        self.graphemes.len()
    }

    /// True before any grapheme is interned.
    pub fn is_empty(&self) -> bool {
        self.graphemes.is_empty()
    }
}

impl Default for GraphemeArena {
    fn default() -> Self {
        GraphemeArena::new()
    }
}

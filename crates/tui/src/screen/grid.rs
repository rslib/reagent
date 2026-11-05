use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::cell::{Cell, GraphemeArena};

/// One contiguous `width * height` block of cells with a dirty bit per row.
#[derive(Debug)]
pub struct Grid {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
    dirty: Vec<bool>,
}

impl Grid {
    /// A grid of blank cells; every row starts dirty.
    pub fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize;
        Grid {
            width,
            height,
            cells: vec![Cell::BLANK; len],
            dirty: vec![true; height as usize],
        }
    }

    /// Columns.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Rows.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// The cells, row by row.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// One cell; out-of-range reads as blank.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        if x >= self.width || y >= self.height {
            return Cell::BLANK;
        }
        self.cells[y as usize * self.width as usize + x as usize]
    }

    /// Change the size, reusing the cell and dirty vectors.
    pub fn resize(&mut self, width: u16, height: u16) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        let len = width as usize * height as usize;
        self.cells.clear();
        self.cells.resize(len, Cell::BLANK);
        self.dirty.clear();
        self.dirty.resize(height as usize, true);
    }

    /// Blank every cell and mark every row dirty, without freeing.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.dirty.fill(true);
    }

    /// Blank one row and mark it for the diff.
    pub fn clear_row(&mut self, y: u16) {
        if y >= self.height {
            return;
        }
        let width = self.width as usize;
        let row = y as usize * width;
        self.cells[row..row + width].fill(Cell::BLANK);
        self.dirty[y as usize] = true;
    }

    /// Blank the columns `x .. x + width` of one row and mark it for the diff,
    /// so a region narrower than the grid (a panel) clears only its own cells
    /// and leaves the region beside it alone.
    pub fn clear_span(&mut self, y: u16, x: u16, width: u16) {
        if y >= self.height || width == 0 {
            return;
        }
        let stride = self.width as usize;
        let row = y as usize * stride;
        let from = usize::from(x).min(stride);
        let to = usize::from(x)
            .saturating_add(usize::from(width))
            .min(stride);
        if from >= to {
            return;
        }
        self.cells[row + from..row + to].fill(Cell::BLANK);
        self.dirty[y as usize] = true;
    }

    /// Point every grapheme cell at its text in a new arena, keeping the style.
    pub fn remap_graphemes(&mut self, old: &GraphemeArena, new: &mut GraphemeArena) {
        for cell in &mut self.cells {
            if cell.is_grapheme() {
                let style = cell.style();
                let index = new.intern(old.get(cell.grapheme_index()));
                *cell = Cell::grapheme(index, style);
            }
        }
    }

    /// Make this grid an exact copy of `other` with every row clean.
    pub fn copy_from(&mut self, other: &Grid) {
        if self.width != other.width || self.height != other.height {
            self.width = other.width;
            self.height = other.height;
        }
        self.cells.clear();
        self.cells.extend_from_slice(&other.cells);
        self.dirty.clear();
        self.dirty.resize(self.height as usize, false);
    }

    /// Mark one row for the diff.
    pub fn mark_dirty(&mut self, y: u16) {
        if let Some(dirty) = self.dirty.get_mut(y as usize) {
            *dirty = true;
        }
    }

    /// Mark every row for the diff.
    pub fn mark_all_dirty(&mut self) {
        self.dirty.fill(true);
    }

    /// Whether a row needs the diff.
    pub fn dirty(&self, y: u16) -> bool {
        self.dirty.get(y as usize).copied().unwrap_or(false)
    }

    /// Clear every row's dirty bit.
    pub fn clear_dirty(&mut self) {
        self.dirty.fill(false);
    }

    /// Whether any row is dirty.
    pub fn any_dirty(&self) -> bool {
        self.dirty.iter().any(|dirty| *dirty)
    }

    /// Write one cell.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = y as usize * self.width as usize + x as usize;
        self.cells[index] = cell;
        self.dirty[y as usize] = true;
    }

    /// Write a string at `(x, y)` in one style, measure its width, clip at the
    /// right edge and return the column after the last glyph. A tab is one
    /// space and every other control character is dropped, in ASCII and in
    /// other text alike, so text from a model or a tool cannot carry a terminal
    /// escape into the frame.
    pub fn put_str(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        style: u32,
        arena: &mut GraphemeArena,
    ) -> u16 {
        self.put_str_until(x, y, text, style, arena, self.width)
    }

    /// Write a string at `(x, y)` and stop at column `limit`, so a region
    /// narrower than the grid (a panel or a sidebar) clips its own text instead
    /// of writing into the one beside it.
    pub fn put_str_until(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        style: u32,
        arena: &mut GraphemeArena,
        limit: u16,
    ) -> u16 {
        let width = limit.min(self.width) as usize;
        if y >= self.height || usize::from(x) >= width {
            return x;
        }
        let row = y as usize * self.width as usize;
        let mut cx = x as usize;

        if text.is_ascii() {
            for &byte in text.as_bytes() {
                if cx >= width {
                    break;
                }
                let c = byte as char;
                if is_control(c) {
                    if c == '\t' {
                        self.cells[row + cx] = Cell::from_char(' ', style);
                        cx += 1;
                    }
                    continue;
                }
                self.cells[row + cx] = Cell::from_char(c, style);
                cx += 1;
            }
        } else {
            let mut i = 0;
            while i < text.len() {
                if cx >= width {
                    break;
                }
                let start = i;
                let first = text[i..].chars().next().expect("non-empty slice");
                i += first.len_utf8();
                while let Some(next) = text[i..].chars().next() {
                    if is_extend(next) {
                        i += next.len_utf8();
                    } else {
                        break;
                    }
                }
                // A control character goes before it measures or interns: a tab
                // is one space and the rest are dropped with the marks after them.
                if is_control(first) {
                    if first == '\t' {
                        self.cells[row + cx] = Cell::from_char(' ', style);
                        cx += 1;
                    }
                    continue;
                }
                let grapheme = &text[start..i];
                let columns = UnicodeWidthStr::width(grapheme);
                if columns == 0 {
                    continue;
                }
                if cx + columns > width {
                    break;
                }
                let cell = if grapheme.len() == first.len_utf8() {
                    Cell::from_char(first, style)
                } else {
                    Cell::grapheme(arena.intern(grapheme), style)
                };
                self.cells[row + cx] = cell;
                for extra in 1..columns {
                    self.cells[row + cx + extra] = Cell::continuation(style);
                }
                cx += columns;
            }
        }

        self.dirty[y as usize] = true;
        cx as u16
    }

    /// Capacity of the cell vector, for the steady-state allocation guard.
    pub fn cells_capacity(&self) -> usize {
        self.cells.capacity()
    }

    /// Capacity of the dirty vector, for the steady-state allocation guard.
    pub fn dirty_capacity(&self) -> usize {
        self.dirty.capacity()
    }
}

/// A combining mark or other zero-width extension of the grapheme before it.
fn is_extend(c: char) -> bool {
    matches!(UnicodeWidthChar::width(c), Some(0))
}

/// C0, DEL and the C1 range: nothing a grid cell may hold.
pub(crate) fn is_control(c: char) -> bool {
    matches!(c, '\0'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}')
}

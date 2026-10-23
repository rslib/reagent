//! A rectangular grid of integers stored row by row.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<i32>,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Grid {
        Grid {
            width,
            height,
            cells: vec![0; width * height],
        }
    }

    /// Builds a grid from rows. All rows must have the same length.
    pub fn from_rows(rows: &[Vec<i32>]) -> Grid {
        let height = rows.len();
        let width = rows.first().map_or(0, |r| r.len());
        let mut grid = Grid::new(width, height);
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), width, "ragged rows");
            for (x, v) in row.iter().enumerate() {
                grid.set(x, y, *v);
            }
        }
        grid
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn get(&self, x: usize, y: usize) -> Option<i32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.cells[y * self.width + x])
    }

    /// Sets a cell. Returns false when the position is outside the grid.
    pub fn set(&mut self, x: usize, y: usize, value: i32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.cells[y * self.width + x] = value;
        true
    }

    pub fn row(&self, y: usize) -> Vec<i32> {
        (0..self.width).filter_map(|x| self.get(x, y)).collect()
    }

    pub fn col(&self, x: usize) -> Vec<i32> {
        (0..self.height).filter_map(|y| self.get(x, y)).collect()
    }

    pub fn transpose(&self) -> Grid {
        let mut out = Grid::new(self.height, self.width);
        for y in 0..self.height {
            for x in 0..self.width {
                out.set(y, x, self.cells[y * self.width + x]);
            }
        }
        out
    }

    pub fn flip_horizontal(&self) -> Grid {
        let mut out = Grid::new(self.height, self.width);
        for y in 0..self.height {
            for x in 0..self.width {
                out.set(self.width - 1 - x, y, self.cells[y * self.width + x]);
            }
        }
        out
    }

    pub fn flip_vertical(&self) -> Grid {
        let mut out = Grid::new(self.width, self.height);
        for y in 0..self.height {
            for x in 0..self.width {
                out.set(x, self.height - 1 - y, self.cells[y * self.width + x]);
            }
        }
        out
    }

    /// The up to four orthogonal neighbours of a cell inside the grid, in the
    /// order up, left, right, down.
    pub fn neighbors4(&self, x: usize, y: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if y > 0 {
            out.push((x, y - 1));
        }
        if x > 0 {
            out.push((x - 1, y));
        }
        if x + 1 <= self.width {
            out.push((x + 1, y));
        }
        if y + 1 < self.height {
            out.push((x, y + 1));
        }
        out
    }

    pub fn sum(&self) -> i64 {
        self.cells.iter().map(|&v| v as i64).sum()
    }

    pub fn count(&self, value: i32) -> usize {
        self.cells.iter().filter(|&&v| v == value).count()
    }

    pub fn fill(&mut self, value: i32) {
        for cell in self.cells.iter_mut() {
            *cell = value;
        }
    }

    pub fn to_rows(&self) -> Vec<Vec<i32>> {
        (0..self.height).map(|y| self.row(y)).collect()
    }
}

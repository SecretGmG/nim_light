use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub row: usize,
    pub col: usize,
}

impl Cell {
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    pub fn toggled(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// A compact, row-major matrix of bits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BitMatrix {
    rows: usize,
    cols: usize,
    words_per_row: usize,
    data: Vec<u64>,
}

impl BitMatrix {
    pub fn new(rows: usize, cols: usize) -> Self {
        let words_per_row = cols.div_ceil(64);
        Self {
            rows,
            cols,
            words_per_row,
            data: vec![0; rows * words_per_row],
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn get(&self, row: usize, col: usize) -> bool {
        assert!(row < self.rows && col < self.cols);
        let word = self.data[row * self.words_per_row + col / 64];
        word & (1 << (col % 64)) != 0
    }

    pub fn set(&mut self, row: usize, col: usize, value: bool) {
        assert!(row < self.rows && col < self.cols);
        let index = row * self.words_per_row + col / 64;
        let mask = 1 << (col % 64);
        if value {
            self.data[index] |= mask;
        } else {
            self.data[index] &= !mask;
        }
    }

    pub fn count_ones(&self) -> usize {
        self.data
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.data.len() * std::mem::size_of::<u64>()
    }

    pub fn row_words(&self, row: usize) -> &[u64] {
        assert!(row < self.rows);
        let start = row * self.words_per_row;
        &self.data[start..start + self.words_per_row]
    }

    pub(crate) fn words(&self) -> &[u64] {
        &self.data
    }

    pub(crate) fn from_words(rows: usize, cols: usize, mut data: Vec<u64>) -> Self {
        let words_per_row = cols.div_ceil(64);
        assert_eq!(data.len(), rows * words_per_row);

        if !cols.is_multiple_of(64) && words_per_row > 0 {
            let used_bits = cols % 64;
            let mask = (1u64 << used_bits) - 1;
            for row in 0..rows {
                data[row * words_per_row + words_per_row - 1] &= mask;
            }
        }

        Self {
            rows,
            cols,
            words_per_row,
            data,
        }
    }

    pub fn row_ones(&self, row: usize) -> impl Iterator<Item = usize> + '_ {
        assert!(row < self.rows);
        self.row_words(row)
            .iter()
            .enumerate()
            .flat_map(|(word_index, &word)| SetBitIndices {
                word,
                offset: word_index * 64,
            })
            .filter(|&col| col < self.cols)
    }

    pub fn clear_row_bits(&mut self, row: usize, mask: &[u64]) {
        assert!(row < self.rows);
        assert_eq!(mask.len(), self.words_per_row);
        let start = row * self.words_per_row;
        for (word, &removed) in self.data[start..start + self.words_per_row]
            .iter_mut()
            .zip(mask)
        {
            *word &= !removed;
        }
    }

    pub fn swap_rows(&mut self, first: usize, second: usize) {
        assert!(first < self.rows && second < self.rows);
        if first == second {
            return;
        }
        for word in 0..self.words_per_row {
            self.data.swap(
                first * self.words_per_row + word,
                second * self.words_per_row + word,
            );
        }
    }

    pub fn swap_cols(&mut self, first: usize, second: usize) {
        assert!(first < self.cols && second < self.cols);
        if first == second {
            return;
        }
        for row in 0..self.rows {
            let first_set = self.get(row, first);
            let second_set = self.get(row, second);
            if first_set != second_set {
                self.set(row, first, second_set);
                self.set(row, second, first_set);
            }
        }
    }

    /// Returns a matrix whose new indices select old rows and columns.
    pub fn reordered(&self, rows: &[usize], cols: &[usize]) -> Self {
        assert!(rows.iter().all(|&row| row < self.rows));
        assert!(cols.iter().all(|&col| col < self.cols));

        let rows_identity = rows.len() == self.rows && is_identity(rows);
        let cols_identity = cols.len() == self.cols && is_identity(cols);

        if rows_identity && cols_identity {
            return self.clone();
        }

        if cols_identity {
            let mut data = Vec::with_capacity(rows.len() * self.words_per_row);
            for &row in rows {
                data.extend_from_slice(self.row_words(row));
            }
            return Self::from_words(rows.len(), self.cols, data);
        }

        let mut result = Self::new(rows.len(), cols.len());
        for (new_row, &old_row) in rows.iter().enumerate() {
            for (new_col, &old_col) in cols.iter().enumerate() {
                if self.get(old_row, old_col) {
                    result.set(new_row, new_col, true);
                }
            }
        }
        result
    }

    pub fn transposed(&self) -> Self {
        let mut result = Self::new(self.cols, self.rows);
        for row in 0..self.rows {
            for col in self.row_ones(row) {
                result.set(col, row, true);
            }
        }
        result
    }
}

fn is_identity(indices: &[usize]) -> bool {
    indices
        .iter()
        .enumerate()
        .all(|(expected, &actual)| expected == actual)
}

impl Hash for BitMatrix {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rows.hash(state);
        self.cols.hash(state);
        self.data.hash(state);
    }
}

struct SetBitIndices {
    word: u64,
    offset: usize,
}

impl Iterator for SetBitIndices {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.word == 0 {
            return None;
        }
        let bit = self.word.trailing_zeros() as usize;
        self.word &= self.word - 1;
        Some(self.offset + bit)
    }
}

/// The user-facing maze representation.
///
/// Nodes and the two wall orientations use their natural logical dimensions:
/// `rows × cols`, `rows × (cols - 1)`, and `(rows - 1) × cols`.
#[derive(Clone, Debug)]
pub struct Maze {
    rows: usize,
    cols: usize,
    nodes: BitMatrix,
    vertical_walls: BitMatrix,
    horizontal_walls: BitMatrix,
}

impl Maze {
    /// Creates a fully open maze.
    pub fn open(rows: usize, cols: usize) -> Self {
        assert!(rows > 0 && cols > 0, "a maze must contain a node");

        let mut nodes = BitMatrix::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                nodes.set(row, col, true);
            }
        }

        Self {
            rows,
            cols,
            nodes,
            vertical_walls: BitMatrix::new(rows, cols - 1),
            horizontal_walls: BitMatrix::new(rows - 1, cols),
        }
    }

    /// A small built-in board with corridors of varied lengths.
    pub fn demo() -> Self {
        let mut maze = Self::open(6, 8);

        for (row, left_col) in [
            (0, 1),
            (0, 5),
            (1, 3),
            (2, 0),
            (2, 4),
            (3, 2),
            (3, 6),
            (4, 1),
            (4, 5),
            (5, 3),
        ] {
            maze.add_vertical_wall(row, left_col);
        }

        for (top_row, col) in [
            (0, 2),
            (0, 6),
            (1, 0),
            (1, 4),
            (2, 2),
            (2, 3),
            (2, 7),
            (3, 1),
            (3, 5),
            (4, 3),
            (4, 6),
        ] {
            maze.add_horizontal_wall(top_row, col);
        }

        maze
    }

    /// Returns a resized maze, preserving overlapping nodes and walls.
    ///
    /// Newly introduced nodes are alive and newly introduced wall positions are
    /// open, matching `Maze::open`.
    pub fn resized(&self, rows: usize, cols: usize) -> Self {
        let mut resized = Self::open(rows, cols);

        for row in 0..self.rows.min(rows) {
            for col in 0..self.cols.min(cols) {
                resized.set_alive(Cell::new(row, col), self.is_alive(Cell::new(row, col)));
            }
        }

        for row in 0..self.rows.min(rows) {
            for col in 0..self.cols.saturating_sub(1).min(cols.saturating_sub(1)) {
                resized.set_vertical_wall(row, col, self.vertical_walls.get(row, col));
            }
        }

        for row in 0..self.rows.saturating_sub(1).min(rows.saturating_sub(1)) {
            for col in 0..self.cols.min(cols) {
                resized.set_horizontal_wall(row, col, self.horizontal_walls.get(row, col));
            }
        }

        resized
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn nodes(&self) -> &BitMatrix {
        &self.nodes
    }

    pub fn vertical_walls(&self) -> &BitMatrix {
        &self.vertical_walls
    }

    pub fn horizontal_walls(&self) -> &BitMatrix {
        &self.horizontal_walls
    }

    pub fn alive_count(&self) -> usize {
        self.nodes.count_ones()
    }

    pub fn is_alive(&self, cell: Cell) -> bool {
        self.contains(cell) && self.nodes.get(cell.row, cell.col)
    }

    pub fn set_alive(&mut self, cell: Cell, alive: bool) {
        assert!(self.contains(cell));
        self.nodes.set(cell.row, cell.col, alive);
    }

    pub fn toggle_alive(&mut self, cell: Cell) {
        self.set_alive(cell, !self.is_alive(cell));
    }

    pub fn contains(&self, cell: Cell) -> bool {
        cell.row < self.rows && cell.col < self.cols
    }

    /// Places a wall between `(row, left_col)` and the cell to its right.
    pub fn add_vertical_wall(&mut self, row: usize, left_col: usize) {
        assert!(row < self.rows && left_col + 1 < self.cols);
        self.vertical_walls.set(row, left_col, true);
    }

    pub fn set_vertical_wall(&mut self, row: usize, left_col: usize, wall: bool) {
        assert!(row < self.rows && left_col + 1 < self.cols);
        self.vertical_walls.set(row, left_col, wall);
    }

    pub fn toggle_vertical_wall(&mut self, row: usize, left_col: usize) {
        assert!(row < self.rows && left_col + 1 < self.cols);
        self.set_vertical_wall(row, left_col, !self.vertical_walls.get(row, left_col));
    }

    /// Places a wall between `(top_row, col)` and the cell below it.
    pub fn add_horizontal_wall(&mut self, top_row: usize, col: usize) {
        assert!(top_row + 1 < self.rows && col < self.cols);
        self.horizontal_walls.set(top_row, col, true);
    }

    pub fn set_horizontal_wall(&mut self, top_row: usize, col: usize, wall: bool) {
        assert!(top_row + 1 < self.rows && col < self.cols);
        self.horizontal_walls.set(top_row, col, wall);
    }

    pub fn toggle_horizontal_wall(&mut self, top_row: usize, col: usize) {
        assert!(top_row + 1 < self.rows && col < self.cols);
        self.set_horizontal_wall(top_row, col, !self.horizontal_walls.get(top_row, col));
    }

    pub fn connected_right(&self, row: usize, left_col: usize) -> bool {
        left_col + 1 < self.cols && !self.vertical_walls.get(row, left_col)
    }

    pub fn connected_down(&self, top_row: usize, col: usize) -> bool {
        top_row + 1 < self.rows && !self.horizontal_walls.get(top_row, col)
    }

    /// Returns the maximal horizontal or vertical hyperedge through `cell`.
    pub fn corridor(&self, cell: Cell, axis: Axis) -> Vec<Cell> {
        assert!(self.contains(cell));
        match axis {
            Axis::Horizontal => {
                let mut first = cell.col;
                while first > 0 && self.connected_right(cell.row, first - 1) {
                    first -= 1;
                }
                let mut last = cell.col;
                while self.connected_right(cell.row, last) {
                    last += 1;
                }
                (first..=last).map(|col| Cell::new(cell.row, col)).collect()
            }
            Axis::Vertical => {
                let mut first = cell.row;
                while first > 0 && self.connected_down(first - 1, cell.col) {
                    first -= 1;
                }
                let mut last = cell.row;
                while self.connected_down(last, cell.col) {
                    last += 1;
                }
                (first..=last).map(|row| Cell::new(row, cell.col)).collect()
            }
        }
    }

    pub fn apply_move(
        &mut self,
        axis: Axis,
        anchor: Cell,
        selected: &[Cell],
    ) -> Result<(), MoveError> {
        if selected.is_empty() {
            return Err(MoveError::Empty);
        }
        if !self.contains(anchor) {
            return Err(MoveError::OutsideBoard);
        }

        let corridor = self.corridor(anchor, axis);
        for (index, &cell) in selected.iter().enumerate() {
            if !self.contains(cell) {
                return Err(MoveError::OutsideBoard);
            }
            if !corridor.contains(&cell) {
                return Err(MoveError::DifferentCorridors);
            }
            if !self.is_alive(cell) {
                return Err(MoveError::AlreadyTaken);
            }
            if selected[..index].contains(&cell) {
                return Err(MoveError::DuplicateNode);
            }
        }

        for cell in selected {
            self.nodes.set(cell.row, cell.col, false);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveError {
    Empty,
    OutsideBoard,
    DifferentCorridors,
    AlreadyTaken,
    DuplicateNode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_matrix_handles_word_boundaries() {
        let mut matrix = BitMatrix::new(2, 130);
        for col in [0, 63, 64, 129] {
            matrix.set(1, col, true);
        }
        assert_eq!(matrix.count_ones(), 4);
        assert!(matrix.get(1, 63));
        assert!(matrix.get(1, 64));
        assert!(!matrix.get(0, 64));
    }

    #[test]
    fn bit_matrix_swaps_and_transposes() {
        let mut matrix = BitMatrix::new(2, 3);
        matrix.set(0, 0, true);
        matrix.set(0, 2, true);
        matrix.set(1, 1, true);

        matrix.swap_rows(0, 1);
        matrix.swap_cols(0, 2);
        assert_eq!(matrix.row_ones(0).collect::<Vec<_>>(), vec![1]);
        assert_eq!(matrix.row_ones(1).collect::<Vec<_>>(), vec![0, 2]);

        let transposed = matrix.transposed();
        assert_eq!((transposed.rows(), transposed.cols()), (3, 2));
        assert!(transposed.get(1, 0));
        assert!(transposed.get(0, 1));
        assert!(transposed.get(2, 1));
    }

    #[test]
    fn maze_uses_natural_node_and_wall_dimensions() {
        let maze = Maze::open(2, 3);
        assert_eq!((maze.nodes().rows(), maze.nodes().cols()), (2, 3));
        assert_eq!(
            (maze.vertical_walls().rows(), maze.vertical_walls().cols()),
            (2, 2)
        );
        assert_eq!(
            (
                maze.horizontal_walls().rows(),
                maze.horizontal_walls().cols()
            ),
            (1, 3)
        );
        assert_eq!(maze.nodes().count_ones(), 6);
        assert_eq!(maze.vertical_walls().count_ones(), 0);
        assert_eq!(maze.horizontal_walls().count_ones(), 0);
    }

    #[test]
    fn resizing_preserves_overlapping_nodes_and_walls() {
        let mut maze = Maze::open(2, 3);
        maze.set_alive(Cell::new(1, 2), false);
        maze.add_vertical_wall(1, 0);
        maze.add_horizontal_wall(0, 2);

        let grown = maze.resized(3, 4);
        assert_eq!((grown.rows(), grown.cols()), (3, 4));
        assert!(!grown.is_alive(Cell::new(1, 2)));
        assert!(grown.is_alive(Cell::new(2, 3)));
        assert!(!grown.connected_right(1, 0));
        assert!(!grown.connected_down(0, 2));
        assert!(grown.connected_right(2, 2));

        let shrunk = grown.resized(2, 2);
        assert_eq!((shrunk.rows(), shrunk.cols()), (2, 2));
        assert!(!shrunk.connected_right(1, 0));
        assert!(shrunk.connected_down(0, 1));
    }

    #[test]
    fn walls_split_corridors() {
        let mut maze = Maze::open(3, 4);
        maze.add_vertical_wall(1, 1);
        maze.add_horizontal_wall(0, 2);

        assert_eq!(
            maze.corridor(Cell::new(1, 0), Axis::Horizontal),
            vec![Cell::new(1, 0), Cell::new(1, 1)]
        );
        assert_eq!(
            maze.corridor(Cell::new(2, 2), Axis::Vertical),
            vec![Cell::new(1, 2), Cell::new(2, 2)]
        );
    }

    #[test]
    fn taking_a_node_does_not_cut_the_corridor() {
        let mut maze = Maze::open(1, 3);
        maze.apply_move(Axis::Horizontal, Cell::new(0, 1), &[Cell::new(0, 1)])
            .unwrap();

        assert_eq!(
            maze.corridor(Cell::new(0, 0), Axis::Horizontal),
            vec![Cell::new(0, 0), Cell::new(0, 1), Cell::new(0, 2)]
        );
        assert!(!maze.is_alive(Cell::new(0, 1)));
    }

    #[test]
    fn move_must_stay_in_one_corridor() {
        let mut maze = Maze::open(2, 2);
        let result = maze.apply_move(
            Axis::Horizontal,
            Cell::new(0, 0),
            &[Cell::new(0, 0), Cell::new(1, 0)],
        );
        assert_eq!(result, Err(MoveError::DifferentCorridors));
        assert_eq!(maze.alive_count(), 4);
    }
}

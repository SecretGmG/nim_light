//! Dense solver representation and inexpensive, deliberately incomplete
//! canonicalization.
//!
//! Rows and columns are the two families of maze corridors. A set bit is a
//! remaining game node at their intersection. Consequently, relabelings are
//! row permutations, column permutations, and (per connected component)
//! transposition.

use std::{cmp::min, collections::VecDeque};

use crate::board::{BitMatrix, Cell, Maze};

pub trait Canonicalizer: Send + Sync {
    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PseudoCanonicalizer;

impl Canonicalizer for PseudoCanonicalizer {
    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame {
        let components: Vec<_> = split_components(&matrix)
            .into_iter()
            .map(|component| {
                let normal = refine_and_order(&component);
                let transposed = refine_and_order(&component.transposed());
                min(normal, transposed)
            })
            .collect();
        CanonicalGame::from_components(components)
    }
}

/// Compiles the current maze position into its wall-free incidence matrix.
///
/// A vertical wall starts a new matrix row; a horizontal wall starts a new
/// matrix column. Removed maze nodes are represented by zero bits.
pub fn compile_maze(maze: &Maze) -> BitMatrix {
    let mut horizontal = vec![0; maze.rows() * maze.cols()];
    let mut row_count = 0;
    for row in 0..maze.rows() {
        let mut corridor = row_count;
        row_count += 1;
        for col in 0..maze.cols() {
            horizontal[row * maze.cols() + col] = corridor;
            if col + 1 < maze.cols() && !maze.connected_right(row, col) {
                corridor = row_count;
                row_count += 1;
            }
        }
    }

    let mut vertical = vec![0; maze.rows() * maze.cols()];
    let mut col_count = 0;
    for col in 0..maze.cols() {
        let mut corridor = col_count;
        col_count += 1;
        for row in 0..maze.rows() {
            vertical[row * maze.cols() + col] = corridor;
            if row + 1 < maze.rows() && !maze.connected_down(row, col) {
                corridor = col_count;
                col_count += 1;
            }
        }
    }

    let mut matrix = BitMatrix::new(row_count, col_count);
    for row in 0..maze.rows() {
        for col in 0..maze.cols() {
            if maze.is_alive(Cell::new(row, col)) {
                let index = row * maze.cols() + col;
                matrix.set(horizontal[index], vertical[index], true);
            }
        }
    }
    matrix
}

/// Canonicalized independent game components, sorted for direct hashing.
///
/// The canonicalization is intentionally heuristic: it may retain more than
/// one representation of an isomorphism class, but every transformation it
/// performs preserves the represented game.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalGame {
    components: Vec<BitMatrix>,
}

impl CanonicalGame {
    pub fn from_matrix(matrix: &BitMatrix) -> Self {
        PseudoCanonicalizer.canonicalize(matrix.clone())
    }

    pub fn from_maze(maze: &Maze) -> Self {
        Self::from_matrix(&compile_maze(maze))
    }

    pub fn components(&self) -> &[BitMatrix] {
        &self.components
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub(crate) fn component(&self, index: usize) -> Self {
        Self {
            components: vec![self.components[index].clone()],
        }
    }

    pub(crate) fn from_canonical_components(components: Vec<BitMatrix>) -> Self {
        Self { components }
    }

    fn from_components(mut components: Vec<BitMatrix>) -> Self {
        components.sort();

        // Equal independent components have equal nimbers, so pairs XOR to 0.
        let mut reduced = Vec::with_capacity(components.len());
        let mut index = 0;
        while index < components.len() {
            let mut end = index + 1;
            while end < components.len() && components[end] == components[index] {
                end += 1;
            }
            if (end - index) % 2 == 1 {
                reduced.push(components[index].clone());
            }
            index = end;
        }
        Self {
            components: reduced,
        }
    }
}

/// Deletes empty dimensions. Nonempty singleton dimensions are retained as
/// the canonical two-incidence completion discussed in the solver design.
fn remove_empty_dimensions(matrix: &BitMatrix) -> BitMatrix {
    let rows: Vec<_> = (0..matrix.rows())
        .filter(|&row| matrix.row_ones(row).next().is_some())
        .collect();
    let mut used_cols = vec![false; matrix.cols()];
    for &row in &rows {
        for col in matrix.row_ones(row) {
            used_cols[col] = true;
        }
    }
    let cols: Vec<_> = (0..matrix.cols()).filter(|&col| used_cols[col]).collect();
    matrix.reordered(&rows, &cols)
}

/// Finds components of the bipartite graph represented by the matrix.
fn split_components(matrix: &BitMatrix) -> Vec<BitMatrix> {
    let matrix = remove_empty_dimensions(matrix);
    if matrix.count_ones() == 0 {
        return Vec::new();
    }

    let transposed = matrix.transposed();
    let mut seen_rows = vec![false; matrix.rows()];
    let mut seen_cols = vec![false; matrix.cols()];
    let mut result = Vec::new();

    for start in 0..matrix.rows() {
        if seen_rows[start] {
            continue;
        }

        let mut rows = Vec::new();
        let mut cols = Vec::new();
        let mut queue = VecDeque::from([Vertex::Row(start)]);
        seen_rows[start] = true;

        while let Some(vertex) = queue.pop_front() {
            match vertex {
                Vertex::Row(row) => {
                    rows.push(row);
                    for col in matrix.row_ones(row) {
                        if !seen_cols[col] {
                            seen_cols[col] = true;
                            queue.push_back(Vertex::Col(col));
                        }
                    }
                }
                Vertex::Col(col) => {
                    cols.push(col);
                    for row in transposed.row_ones(col) {
                        if !seen_rows[row] {
                            seen_rows[row] = true;
                            queue.push_back(Vertex::Row(row));
                        }
                    }
                }
            }
        }

        rows.sort_unstable();
        cols.sort_unstable();
        result.push(matrix.reordered(&rows, &cols));
    }
    result
}

#[derive(Clone, Copy)]
enum Vertex {
    Row(usize),
    Col(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Signature {
    side: u8,
    previous: usize,
    neighbours: Vec<usize>,
}

/// One-dimensional Weisfeiler-Lehman refinement on the bipartite graph.
///
/// Previous colors are included in signatures, so color classes only split.
/// The process therefore stabilizes after at most `rows + columns` splits.
pub(crate) fn refine_colors(matrix: &BitMatrix) -> (Vec<usize>, Vec<usize>) {
    let transposed = matrix.transposed();
    let mut row_colors = vec![0; matrix.rows()];
    let mut col_colors = vec![0; matrix.cols()];
    let mut previous_color_count = 0;

    loop {
        let row_signatures: Vec<_> = (0..matrix.rows())
            .map(|row| {
                let mut neighbours: Vec<_> =
                    matrix.row_ones(row).map(|col| col_colors[col]).collect();
                neighbours.sort_unstable();
                Signature {
                    side: 0,
                    previous: row_colors[row],
                    neighbours,
                }
            })
            .collect();
        let col_signatures: Vec<_> = (0..matrix.cols())
            .map(|col| {
                let mut neighbours: Vec<_> = transposed
                    .row_ones(col)
                    .map(|row| row_colors[row])
                    .collect();
                neighbours.sort_unstable();
                Signature {
                    side: 1,
                    previous: col_colors[col],
                    neighbours,
                }
            })
            .collect();

        let (new_rows, new_cols, color_count) = compress_signatures(row_signatures, col_signatures);
        row_colors = new_rows;
        col_colors = new_cols;

        if color_count == previous_color_count {
            return (row_colors, col_colors);
        }
        previous_color_count = color_count;
    }
}

fn compress_signatures(
    rows: Vec<Signature>,
    cols: Vec<Signature>,
) -> (Vec<usize>, Vec<usize>, usize) {
    let row_count = rows.len();
    let mut indexed: Vec<_> = rows
        .into_iter()
        .enumerate()
        .map(|(index, signature)| (signature, Vertex::Row(index)))
        .chain(
            cols.into_iter()
                .enumerate()
                .map(|(index, signature)| (signature, Vertex::Col(index))),
        )
        .collect();
    indexed.sort_by(|left, right| left.0.cmp(&right.0));

    let mut row_colors = vec![0; row_count];
    let mut col_colors = vec![0; indexed.len() - row_count];
    let mut color = 0;
    let mut previous: Option<&Signature> = None;
    for (signature, vertex) in &indexed {
        if previous.is_some_and(|old| old != signature) {
            color += 1;
        }
        match *vertex {
            Vertex::Row(row) => row_colors[row] = color,
            Vertex::Col(col) => col_colors[col] = color,
        }
        previous = Some(signature);
    }
    (
        row_colors,
        col_colors,
        color + usize::from(!indexed.is_empty()),
    )
}

/// Orders refined color classes lexicographically. Alternating row and column
/// ordering is a fast heuristic for the color classes that refinement leaves
/// unresolved. Cycle detection provides a deterministic stop condition.
fn refine_and_order(matrix: &BitMatrix) -> BitMatrix {
    let (row_colors, col_colors) = refine_colors(matrix);
    let mut rows: Vec<_> = (0..matrix.rows()).collect();
    let mut cols: Vec<_> = (0..matrix.cols()).collect();
    rows.sort_by_key(|&row| row_colors[row]);
    cols.sort_by_key(|&col| col_colors[col]);

    let mut best = matrix.reordered(&rows, &cols);
    let rounds = (matrix.rows() + matrix.cols()).clamp(1, 16);

    for _ in 0..rounds {
        let old_rows = rows.clone();
        let old_cols = cols.clone();

        let mut row_entries: Vec<_> = (0..matrix.rows())
            .map(|row| (row_colors[row], packed_row(matrix, row, &cols), row))
            .collect();
        row_entries.sort_unstable();
        rows = row_entries.into_iter().map(|(_, _, row)| row).collect();

        let mut col_entries: Vec<_> = (0..matrix.cols())
            .map(|col| (col_colors[col], packed_col(matrix, col, &rows), col))
            .collect();
        col_entries.sort_unstable();
        cols = col_entries.into_iter().map(|(_, _, col)| col).collect();

        let candidate = matrix.reordered(&rows, &cols);
        if candidate < best {
            best = candidate.clone();
        }
        if rows == old_rows && cols == old_cols {
            break;
        }
    }
    best
}

fn packed_row(matrix: &BitMatrix, row: usize, cols: &[usize]) -> Vec<u64> {
    let mut result = vec![0; cols.len().div_ceil(64)];
    for (new_col, &old_col) in cols.iter().enumerate() {
        if matrix.get(row, old_col) {
            result[new_col / 64] |= 1 << (new_col % 64);
        }
    }
    result
}

fn packed_col(matrix: &BitMatrix, col: usize, rows: &[usize]) -> Vec<u64> {
    let mut result = vec![0; rows.len().div_ceil(64)];
    for (new_row, &old_row) in rows.iter().enumerate() {
        if matrix.get(old_row, col) {
            result[new_row / 64] |= 1 << (new_row % 64);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Axis;

    #[test]
    fn compiles_walls_into_additional_rows_and_columns() {
        let mut maze = Maze::open(2, 2);
        maze.add_vertical_wall(0, 0);
        maze.add_horizontal_wall(0, 0);

        let matrix = compile_maze(&maze);
        assert_eq!((matrix.rows(), matrix.cols()), (3, 3));
        assert_eq!(matrix.row_ones(0).collect::<Vec<_>>(), vec![0]);
        assert_eq!(matrix.row_ones(1).collect::<Vec<_>>(), vec![2]);
        assert_eq!(matrix.row_ones(2).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn compilation_respects_removed_nodes_without_changing_topology() {
        let mut maze = Maze::open(1, 3);
        maze.apply_move(Axis::Horizontal, Cell::new(0, 1), &[Cell::new(0, 1)])
            .unwrap();

        let matrix = compile_maze(&maze);
        assert_eq!((matrix.rows(), matrix.cols()), (1, 3));
        assert_eq!(matrix.row_ones(0).collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn canonicalization_removes_empty_dimensions_and_splits_components() {
        let mut matrix = BitMatrix::new(5, 6);
        matrix.set(1, 1, true);
        matrix.set(1, 2, true);
        matrix.set(3, 4, true);

        let canonical = CanonicalGame::from_matrix(&matrix);
        assert_eq!(canonical.components().len(), 2);
        assert_eq!(
            canonical
                .components()
                .iter()
                .map(BitMatrix::count_ones)
                .sum::<usize>(),
            3
        );
        assert!(
            canonical
                .components()
                .iter()
                .all(|part| part.rows() <= 2 && part.cols() <= 2)
        );
    }

    #[test]
    fn canonicalization_cancels_pairs_of_identical_components() {
        let mut matrix = BitMatrix::new(3, 3);
        matrix.set(0, 0, true);
        matrix.set(1, 1, true);

        assert!(CanonicalGame::from_matrix(&matrix).is_empty());

        matrix.set(2, 2, true);
        let canonical = CanonicalGame::from_matrix(&matrix);
        assert_eq!(canonical.components().len(), 1);
        assert_eq!(canonical.components()[0].count_ones(), 1);
    }

    #[test]
    fn canonicalization_handles_permutation_and_transposition() {
        let mut random = TestRandom::new(0xfeed_beef);

        for case in 0..40 {
            // Exercise both small matrices and storage spanning several words.
            let (rows, cols) = if case == 0 {
                (70, 73)
            } else {
                (6 + random.index(5), 6 + random.index(5))
            };
            let mut matrix = BitMatrix::new(rows, cols);
            for row in 0..rows {
                for col in 0..cols {
                    if random.index(100) < 38 {
                        matrix.set(row, col, true);
                    }
                }
            }

            let expected = CanonicalGame::from_matrix(&matrix);
            for _permutation in 0..20 {
                let row_order = random.permutation(rows);
                let col_order = random.permutation(cols);
                let permuted = matrix.reordered(&row_order, &col_order);
                assert_eq!(CanonicalGame::from_matrix(&permuted), expected);
                assert_eq!(CanonicalGame::from_matrix(&permuted.transposed()), expected);
            }
        }
    }

    #[test]
    fn canonicalization_fuzzes_compiled_random_mazes() {
        let mut random = TestRandom::new(0x5eed_cafe);

        for _case in 0..30 {
            let rows = 5 + random.index(5);
            let cols = 5 + random.index(5);
            let mut maze = Maze::open(rows, cols);
            for row in 0..rows {
                for col in 0..cols - 1 {
                    if random.index(100) < 35 {
                        maze.add_vertical_wall(row, col);
                    }
                }
            }
            for row in 0..rows - 1 {
                for col in 0..cols {
                    if random.index(100) < 35 {
                        maze.add_horizontal_wall(row, col);
                    }
                }
            }

            let matrix = compile_maze(&maze);
            let expected = CanonicalGame::from_matrix(&matrix);
            for _permutation in 0..12 {
                let mut permuted = matrix.reordered(
                    &random.permutation(matrix.rows()),
                    &random.permutation(matrix.cols()),
                );
                if random.index(2) == 1 {
                    permuted = permuted.transposed();
                }
                assert_eq!(CanonicalGame::from_matrix(&permuted), expected);
            }
        }
    }

    struct TestRandom(u64);

    impl TestRandom {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn index(&mut self, upper: usize) -> usize {
            self.next() as usize % upper
        }

        fn permutation(&mut self, len: usize) -> Vec<usize> {
            let mut result: Vec<_> = (0..len).collect();
            for index in (1..len).rev() {
                result.swap(index, self.index(index + 1));
            }
            result
        }
    }
}

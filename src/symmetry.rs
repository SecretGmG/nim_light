use std::collections::HashSet;

use crate::{board::BitMatrix, evaluator::SymmetryFinder, solver::refine_colors};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symmetry {
    row_map: Vec<usize>,
    col_map: Vec<usize>,
}

impl Symmetry {
    pub fn row_map(&self) -> &[usize] {
        &self.row_map
    }

    pub fn col_map(&self) -> &[usize] {
        &self.col_map
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InvolutionSymmetryFinder;

impl InvolutionSymmetryFinder {
    pub fn find(&self, matrix: &BitMatrix) -> Option<Symmetry> {
        if matrix.count_ones() == 0
            || !matrix.count_ones().is_multiple_of(2)
            || !matrix.rows().is_multiple_of(2)
            || !matrix.cols().is_multiple_of(2)
        {
            return None;
        }

        let (row_colors, col_colors) = refine_colors(matrix);
        if !color_classes_are_even(&row_colors) || !color_classes_are_even(&col_colors) {
            return None;
        }

        let mut search = SymmetrySearch {
            matrix,
            row_colors,
            col_colors,
            row_map: vec![None; matrix.rows()],
            col_map: vec![None; matrix.cols()],
        };
        search.search().then(|| Symmetry {
            row_map: search.row_map.into_iter().map(Option::unwrap).collect(),
            col_map: search.col_map.into_iter().map(Option::unwrap).collect(),
        })
    }
}

impl SymmetryFinder for InvolutionSymmetryFinder {
    fn proves_zero(&self, component: &BitMatrix) -> bool {
        self.find(component).is_some()
    }
}

struct SymmetrySearch<'a> {
    matrix: &'a BitMatrix,
    row_colors: Vec<usize>,
    col_colors: Vec<usize>,
    row_map: Vec<Option<usize>>,
    col_map: Vec<Option<usize>>,
}

impl SymmetrySearch<'_> {
    fn search(&mut self) -> bool {
        let Some(choice) = self.best_choice() else {
            return true;
        };

        match choice {
            Choice::Row(node, candidates) => {
                for candidate in candidates {
                    self.row_map[node] = Some(candidate);
                    self.row_map[candidate] = Some(node);
                    if self.search() {
                        return true;
                    }
                    self.row_map[node] = None;
                    self.row_map[candidate] = None;
                }
            }
            Choice::Col(node, candidates) => {
                for candidate in candidates {
                    self.col_map[node] = Some(candidate);
                    self.col_map[candidate] = Some(node);
                    if self.search() {
                        return true;
                    }
                    self.col_map[node] = None;
                    self.col_map[candidate] = None;
                }
            }
        }
        false
    }

    fn best_choice(&self) -> Option<Choice> {
        let mut best: Option<Choice> = None;
        let mut seen_colors = HashSet::new();

        for row in 0..self.matrix.rows() {
            if self.row_map[row].is_some() || !seen_colors.insert(self.row_colors[row]) {
                continue;
            }
            let candidates = (0..self.matrix.rows())
                .filter(|&candidate| {
                    self.row_map[candidate].is_none()
                        && self.row_colors[candidate] == self.row_colors[row]
                        && candidate != row
                        && self.valid_row_pair(row, candidate)
                })
                .collect();
            update_best(&mut best, Choice::Row(row, candidates));
        }

        seen_colors.clear();
        for col in 0..self.matrix.cols() {
            if self.col_map[col].is_some() || !seen_colors.insert(self.col_colors[col]) {
                continue;
            }
            let candidates = (0..self.matrix.cols())
                .filter(|&candidate| {
                    self.col_map[candidate].is_none()
                        && self.col_colors[candidate] == self.col_colors[col]
                        && candidate != col
                        && self.valid_col_pair(col, candidate)
                })
                .collect();
            update_best(&mut best, Choice::Col(col, candidates));
        }
        best
    }

    fn valid_row_pair(&self, first: usize, second: usize) -> bool {
        for col in 0..self.matrix.cols() {
            if let Some(mapped_col) = self.col_map[col]
                && self.matrix.get(first, col) != self.matrix.get(second, mapped_col)
            {
                return false;
            }
        }
        pattern_classes_can_pair(
            self.col_map.iter().map(Option::is_none),
            &self.col_colors,
            |col| self.matrix.get(first, col),
            |col| self.matrix.get(second, col),
        )
    }

    fn valid_col_pair(&self, first: usize, second: usize) -> bool {
        for row in 0..self.matrix.rows() {
            if let Some(mapped_row) = self.row_map[row]
                && self.matrix.get(row, first) != self.matrix.get(mapped_row, second)
            {
                return false;
            }
        }
        pattern_classes_can_pair(
            self.row_map.iter().map(Option::is_none),
            &self.row_colors,
            |row| self.matrix.get(row, first),
            |row| self.matrix.get(row, second),
        )
    }
}

enum Choice {
    Row(usize, Vec<usize>),
    Col(usize, Vec<usize>),
}

impl Choice {
    fn candidate_count(&self) -> usize {
        match self {
            Self::Row(_, candidates) | Self::Col(_, candidates) => candidates.len(),
        }
    }
}

fn update_best(best: &mut Option<Choice>, candidate: Choice) {
    if best
        .as_ref()
        .is_none_or(|current| candidate.candidate_count() < current.candidate_count())
    {
        *best = Some(candidate);
    }
}

fn pattern_classes_can_pair(
    unmapped: impl Iterator<Item = bool>,
    colors: &[usize],
    first: impl Fn(usize) -> bool,
    second: impl Fn(usize) -> bool,
) -> bool {
    let color_count = colors.iter().copied().max().unwrap_or(0) + 1;
    let mut patterns = vec![[0usize; 4]; color_count];
    for (index, is_unmapped) in unmapped.enumerate() {
        if is_unmapped {
            let pattern = ((first(index) as usize) << 1) | second(index) as usize;
            patterns[colors[index]][pattern] += 1;
        }
    }
    patterns.into_iter().all(|counts| {
        counts[0].is_multiple_of(2) && counts[3].is_multiple_of(2) && counts[1] == counts[2]
    })
}

fn color_classes_are_even(colors: &[usize]) -> bool {
    let mut counts = vec![0usize; colors.iter().copied().max().unwrap_or(0) + 1];
    for &color in colors {
        counts[color] += 1;
    }
    counts.into_iter().all(|count| count.is_multiple_of(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_even_grid_has_a_valid_symmetry() {
        let matrix = dense(4, 4);
        let symmetry = InvolutionSymmetryFinder.find(&matrix).unwrap();
        assert_valid(&matrix, &symmetry);
    }

    #[test]
    fn finds_nontrivial_side_preserving_symmetry() {
        let matrix = from_rows(&["1100", "0011", "1010", "1010"]);
        let symmetry = InvolutionSymmetryFinder.find(&matrix).unwrap();
        assert_valid(&matrix, &symmetry);
    }

    #[test]
    fn odd_nodes_rows_or_columns_fail_immediately() {
        assert!(InvolutionSymmetryFinder.find(&dense(3, 4)).is_none());
        assert!(InvolutionSymmetryFinder.find(&dense(4, 3)).is_none());

        let odd_nodes = from_rows(&["1100", "0011", "1000", "0000"]);
        assert!(odd_nodes.count_ones() % 2 == 1);
        assert!(InvolutionSymmetryFinder.find(&odd_nodes).is_none());
    }

    #[test]
    fn incompatible_structural_classes_are_not_symmetric() {
        let matrix = from_rows(&["1111", "1110", "1100", "1000"]);
        assert!(InvolutionSymmetryFinder.find(&matrix).is_none());
    }

    #[test]
    fn finder_matches_exhaustive_search_on_random_four_by_four_matrices() {
        let mut random = TestRandom(0x5eed_1e55);
        for _case in 0..300 {
            let mut matrix = BitMatrix::new(4, 4);
            for row in 0..4 {
                for col in 0..4 {
                    matrix.set(row, col, random.next() & 1 == 1);
                }
            }
            assert_eq!(
                InvolutionSymmetryFinder.find(&matrix).is_some(),
                brute_force_has_symmetry(&matrix)
            );
        }
    }

    fn assert_valid(matrix: &BitMatrix, symmetry: &Symmetry) {
        for row in 0..matrix.rows() {
            assert_ne!(symmetry.row_map[row], row);
            assert_eq!(symmetry.row_map[symmetry.row_map[row]], row);
        }
        for col in 0..matrix.cols() {
            assert_ne!(symmetry.col_map[col], col);
            assert_eq!(symmetry.col_map[symmetry.col_map[col]], col);
        }
        for row in 0..matrix.rows() {
            for col in 0..matrix.cols() {
                assert_eq!(
                    matrix.get(row, col),
                    matrix.get(symmetry.row_map[row], symmetry.col_map[col])
                );
            }
        }
    }

    fn dense(rows: usize, cols: usize) -> BitMatrix {
        let mut matrix = BitMatrix::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                matrix.set(row, col, true);
            }
        }
        matrix
    }

    fn from_rows(rows: &[&str]) -> BitMatrix {
        let mut matrix = BitMatrix::new(rows.len(), rows[0].len());
        for (row, bits) in rows.iter().enumerate() {
            for (col, bit) in bits.bytes().enumerate() {
                matrix.set(row, col, bit == b'1');
            }
        }
        matrix
    }

    fn brute_force_has_symmetry(matrix: &BitMatrix) -> bool {
        fixed_point_free_involutions(matrix.rows())
            .into_iter()
            .any(|row_map| {
                fixed_point_free_involutions(matrix.cols())
                    .into_iter()
                    .any(|col_map| {
                        (0..matrix.rows()).all(|row| {
                            (0..matrix.cols()).all(|col| {
                                matrix.get(row, col) == matrix.get(row_map[row], col_map[col])
                            })
                        })
                    })
            })
    }

    fn fixed_point_free_involutions(size: usize) -> Vec<Vec<usize>> {
        fn generate(mapping: &mut [Option<usize>], result: &mut Vec<Vec<usize>>) {
            let Some(first) = mapping.iter().position(Option::is_none) else {
                result.push(mapping.iter().copied().map(Option::unwrap).collect());
                return;
            };
            for second in first + 1..mapping.len() {
                if mapping[second].is_none() {
                    mapping[first] = Some(second);
                    mapping[second] = Some(first);
                    generate(mapping, result);
                    mapping[first] = None;
                    mapping[second] = None;
                }
            }
        }

        if !size.is_multiple_of(2) {
            return Vec::new();
        }
        let mut result = Vec::new();
        generate(&mut vec![None; size], &mut result);
        result
    }

    struct TestRandom(u64);

    impl TestRandom {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }
}

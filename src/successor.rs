use crate::{
    board::BitMatrix,
    solver::{CanonicalGame, Canonicalizer},
};

pub trait SuccessorGenerator: Send + Sync {
    type Successors<'a>: Iterator<Item = CanonicalGame> + Send + 'a
    where
        Self: 'a;

    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame;

    fn successors<'a>(&'a self, component: &'a BitMatrix) -> Self::Successors<'a>;

    fn estimated_successors(&self, component: &BitMatrix) -> usize;
}

/// Generates one representative for permutations of leaf nodes on an edge.
///
/// For a row with `s` nodes that also belong to non-singleton columns and `u`
/// nodes whose columns are singletons, only `2^s * (u + 1) - 1` moves are
/// required. The singleton-column nodes are interchangeable, so removing the
/// first `k` represents every choice of `k` such nodes.
#[derive(Clone, Debug)]
pub struct CanonicalMoveGenerator<C> {
    canonicalizer: C,
}

impl<C> CanonicalMoveGenerator<C> {
    pub fn new(canonicalizer: C) -> Self {
        Self { canonicalizer }
    }

    pub fn canonicalizer(&self) -> &C {
        &self.canonicalizer
    }
}

impl<C> SuccessorGenerator for CanonicalMoveGenerator<C>
where
    C: Canonicalizer,
{
    type Successors<'a>
        = CanonicalSuccessors<'a, C>
    where
        C: 'a;

    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame {
        self.canonicalizer.canonicalize(matrix)
    }

    fn successors<'a>(&'a self, component: &'a BitMatrix) -> Self::Successors<'a> {
        CanonicalSuccessors::new(component, &self.canonicalizer)
    }

    fn estimated_successors(&self, component: &BitMatrix) -> usize {
        let transposed = component.transposed();
        representative_move_count(component).saturating_add(representative_move_count(&transposed))
    }
}

pub struct CanonicalSuccessors<'a, C> {
    canonicalizer: &'a C,
    orientations: [BitMatrix; 2],
    orientation: usize,
    row: usize,
    column_counts: Vec<usize>,
    shared_candidates: Vec<u64>,
    shared_removed: Vec<u64>,
    leaf_columns: Vec<usize>,
    leaf_removed: Vec<u64>,
    leaf_count: usize,
    row_ready: bool,
    previous_row: Option<Vec<u64>>,
}

impl<'a, C> CanonicalSuccessors<'a, C>
where
    C: Canonicalizer,
{
    fn new(component: &BitMatrix, canonicalizer: &'a C) -> Self {
        let orientations = [component.clone(), component.transposed()];
        let column_counts = column_counts(&orientations[0]);
        Self {
            canonicalizer,
            orientations,
            orientation: 0,
            row: 0,
            column_counts,
            shared_candidates: Vec::new(),
            shared_removed: Vec::new(),
            leaf_columns: Vec::new(),
            leaf_removed: Vec::new(),
            leaf_count: 0,
            row_ready: false,
            previous_row: None,
        }
    }

    fn next_raw(&mut self) -> Option<BitMatrix> {
        loop {
            if !self.row_ready && !self.prepare_row() {
                return None;
            }

            if self.leaf_count < self.leaf_columns.len() {
                let col = self.leaf_columns[self.leaf_count];
                self.leaf_removed[col / 64] |= 1 << (col % 64);
                self.leaf_count += 1;
            } else {
                self.leaf_count = 0;
                self.leaf_removed.fill(0);
                if !increment_masked(&mut self.shared_removed, &self.shared_candidates) {
                    self.row += 1;
                    self.row_ready = false;
                    continue;
                }
            }

            let matrix = &self.orientations[self.orientation];
            let mut result = matrix.clone();
            result.clear_row_bits(self.row, &self.shared_removed);
            result.clear_row_bits(self.row, &self.leaf_removed);
            return Some(result);
        }
    }

    fn prepare_row(&mut self) -> bool {
        loop {
            while self.row == self.orientations[self.orientation].rows() {
                self.orientation += 1;
                if self.orientation == self.orientations.len() {
                    return false;
                }
                self.row = 0;
                self.column_counts = column_counts(&self.orientations[self.orientation]);
                // Horizontal and vertical edge families are deliberately
                // deduplicated independently.
                self.previous_row = None;
            }

            let row_pattern = self.orientations[self.orientation]
                .row_words(self.row)
                .to_vec();
            if self.previous_row.as_ref() != Some(&row_pattern) {
                self.previous_row = Some(row_pattern);
                break;
            }
            self.row += 1;
        }

        let matrix = &self.orientations[self.orientation];
        let word_count = matrix.cols().div_ceil(64);
        self.shared_candidates = vec![0; word_count];
        self.shared_removed = vec![0; word_count];
        self.leaf_columns.clear();
        self.leaf_removed = vec![0; word_count];
        self.leaf_count = 0;

        for col in matrix.row_ones(self.row) {
            if self.column_counts[col] == 1 {
                self.leaf_columns.push(col);
            } else {
                self.shared_candidates[col / 64] |= 1 << (col % 64);
            }
        }
        self.row_ready = true;
        true
    }
}

impl<C> Iterator for CanonicalSuccessors<'_, C>
where
    C: Canonicalizer,
{
    type Item = CanonicalGame;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_raw()
            .map(|raw| self.canonicalizer.canonicalize(raw))
    }
}

fn column_counts(matrix: &BitMatrix) -> Vec<usize> {
    let mut counts = vec![0; matrix.cols()];
    for row in 0..matrix.rows() {
        for col in matrix.row_ones(row) {
            counts[col] += 1;
        }
    }
    counts
}

fn representative_move_count(matrix: &BitMatrix) -> usize {
    let counts = column_counts(matrix);
    let mut previous_row = None;
    (0..matrix.rows())
        .filter(|&row| {
            let pattern = matrix.row_words(row).to_vec();
            if previous_row.as_ref() == Some(&pattern) {
                false
            } else {
                previous_row = Some(pattern);
                true
            }
        })
        .map(|row| {
            let mut shared = 0;
            let mut leaves = 0usize;
            for col in matrix.row_ones(row) {
                if counts[col] == 1 {
                    leaves += 1;
                } else {
                    shared += 1;
                }
            }
            subset_count_including_empty(shared)
                .saturating_mul(leaves + 1)
                .saturating_sub(1)
        })
        .fold(0, usize::saturating_add)
}

fn subset_count_including_empty(size: usize) -> usize {
    1usize.checked_shl(size as u32).unwrap_or(usize::MAX)
}

/// Increments a bitset while skipping every bit absent from `allowed`.
///
/// Each word acts as one digit whose values are all subsets of its allowed
/// bits. Carrying between words makes this work for arbitrarily wide rows.
fn increment_masked(value: &mut [u64], allowed: &[u64]) -> bool {
    for (word, &mask) in value.iter_mut().zip(allowed) {
        *word = word.wrapping_sub(mask) & mask;
        if *word != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::solver::PseudoCanonicalizer;

    fn heap(size: usize) -> BitMatrix {
        let mut result = BitMatrix::new(1, size);
        for col in 0..size {
            result.set(0, col, true);
        }
        result
    }

    #[test]
    fn leaf_nodes_reduce_the_number_of_generated_representatives() {
        let component = heap(3);
        let generator = CanonicalMoveGenerator::new(PseudoCanonicalizer);

        // Three row representatives plus one representative for the three
        // identical singleton-column rows. Brute force would generate ten.
        // Final canonical deduplication is intentionally left to the evaluator
        // cache, so the two ways to reach the empty game are both yielded here.
        assert_eq!(generator.estimated_successors(&component), 4);
        let successors: Vec<_> = generator.successors(&component).collect();
        assert_eq!(successors.len(), 4);
        assert_eq!(successors.into_iter().collect::<HashSet<_>>().len(), 3);
    }

    #[test]
    fn dense_five_by_five_grid_has_five_unique_canonical_successors() {
        struct CountingCanonicalizer {
            calls: Arc<AtomicUsize>,
        }

        impl Canonicalizer for CountingCanonicalizer {
            fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame {
                self.calls.fetch_add(1, Ordering::Relaxed);
                PseudoCanonicalizer.canonicalize(matrix)
            }
        }

        let mut grid = BitMatrix::new(5, 5);
        for row in 0..5 {
            for col in 0..5 {
                grid.set(row, col, true);
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let generator = CanonicalMoveGenerator::new(CountingCanonicalizer {
            calls: Arc::clone(&calls),
        });
        let successors: Vec<_> = generator.successors(&grid).collect();
        let unique: HashSet<_> = successors.iter().cloned().collect();
        let mut remaining_nodes: Vec<_> = successors
            .iter()
            .map(|game| {
                game.components()
                    .iter()
                    .map(BitMatrix::count_ones)
                    .sum::<usize>()
            })
            .collect();
        remaining_nodes.sort_unstable();
        remaining_nodes.dedup();

        assert_eq!(unique.len(), 5);
        assert_eq!(remaining_nodes, vec![20, 21, 22, 23, 24]);
        assert_eq!(generator.estimated_successors(&grid), 62);
        assert_eq!(successors.len(), 62);
        assert_eq!(calls.load(Ordering::Relaxed), 62);
    }

    #[test]
    fn masked_counter_crosses_word_boundaries() {
        let mut allowed = vec![0; 2];
        allowed[0] = 1 << 63;
        allowed[1] = 1;
        let mut value = vec![0; 2];

        assert!(increment_masked(&mut value, &allowed));
        assert_eq!(value, vec![1 << 63, 0]);
        assert!(increment_masked(&mut value, &allowed));
        assert_eq!(value, vec![0, 1]);
        assert!(increment_masked(&mut value, &allowed));
        assert_eq!(value, vec![1 << 63, 1]);
        assert!(!increment_masked(&mut value, &allowed));
        assert_eq!(value, vec![0, 0]);
    }

    #[test]
    fn optimized_generator_matches_brute_force_on_random_components() {
        let canonicalizer = PseudoCanonicalizer;
        let generator = CanonicalMoveGenerator::new(canonicalizer);
        let mut random = TestRandom(0x51cc_3550);

        for _case in 0..80 {
            let rows = 2 + random.index(6);
            let cols = 2 + random.index(6);
            let mut matrix = BitMatrix::new(rows, cols);
            for row in 0..rows {
                for col in 0..cols {
                    if random.index(100) < 45 {
                        matrix.set(row, col, true);
                    }
                }
            }

            let game = canonicalizer.canonicalize(matrix);
            for component in game.components() {
                let optimized: HashSet<_> = generator.successors(component).collect();
                let brute = brute_force_successors(component, canonicalizer);
                assert_eq!(optimized, brute);
            }
        }
    }

    fn brute_force_successors(
        component: &BitMatrix,
        canonicalizer: PseudoCanonicalizer,
    ) -> HashSet<CanonicalGame> {
        let mut successors = HashSet::new();
        for matrix in [component.clone(), component.transposed()] {
            for row in 0..matrix.rows() {
                let columns: Vec<_> = matrix.row_ones(row).collect();
                let mut removed = vec![false; columns.len()];
                while increment_binary(&mut removed) {
                    let mut result = matrix.clone();
                    for (&col, &is_removed) in columns.iter().zip(&removed) {
                        if is_removed {
                            result.set(row, col, false);
                        }
                    }
                    successors.insert(canonicalizer.canonicalize(result));
                }
            }
        }
        successors
    }

    fn increment_binary(bits: &mut [bool]) -> bool {
        for bit in bits {
            if *bit {
                *bit = false;
            } else {
                *bit = true;
                return true;
            }
        }
        false
    }

    struct TestRandom(u64);

    impl TestRandom {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn index(&mut self, upper: usize) -> usize {
            self.next() as usize % upper
        }
    }
}

use std::collections::HashMap;

use crate::{
    board::BitMatrix,
    solver::{CanonicalGame, Canonicalizer},
};

pub trait SuccessorGenerator: Send + Sync {
    type Successors<'a>: Iterator<Item = CanonicalGame> + Send + 'a
    where
        Self: 'a;
    type SuccessorGroups<'a>: IndexedSuccessorGroups + Send + 'a
    where
        Self: 'a;

    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame;

    fn successors<'a>(&'a self, component: &'a BitMatrix) -> Self::Successors<'a>;

    fn successor_groups<'a>(&'a self, component: &'a BitMatrix) -> Self::SuccessorGroups<'a>;

    fn estimated_successors(&self, component: &BitMatrix) -> usize;
}

pub trait IndexedSuccessorGroups {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn group_len(&self, group_index: usize) -> usize;

    fn successor(&self, group_index: usize, move_index: usize) -> CanonicalGame;
}

/// Generates one representative for permutations of equivalent nodes on an edge.
///
/// Columns with identical bit patterns are interchangeable. A class containing
/// `k` columns therefore contributes `k + 1` choices: remove zero through all
/// `k` nodes in that class.
#[derive(Clone, Debug)]
pub struct CanonicalMoveGenerator<C> {
    canonicalizer: C,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderedSuccessor {
    pub removed_nodes: usize,
    pub game: CanonicalGame,
}

impl<C> CanonicalMoveGenerator<C> {
    pub fn new(canonicalizer: C) -> Self {
        Self { canonicalizer }
    }

    pub fn canonicalizer(&self) -> &C {
        &self.canonicalizer
    }
}

impl<C> CanonicalMoveGenerator<C>
where
    C: Canonicalizer,
{
    pub fn ordered_successors(&self, component: &BitMatrix) -> Vec<OrderedSuccessor> {
        let original_nodes = component.count_ones();
        let groups = self.successor_groups(component);
        let mut successors = Vec::new();
        for group_index in 0..groups.len() {
            for move_index in 0..groups.group_len(group_index) {
                let raw = groups.raw_successor(group_index, move_index);
                if let Some(removed_nodes) = original_nodes.checked_sub(raw.count_ones()) {
                    successors.push(OrderedSuccessor {
                        removed_nodes,
                        game: self.canonicalizer.canonicalize(raw),
                    });
                }
            }
        }
        successors.sort_by_key(|successor| std::cmp::Reverse(successor.removed_nodes));
        successors
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
    type SuccessorGroups<'a>
        = CanonicalSuccessorGroups<'a, C>
    where
        C: 'a;

    fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame {
        self.canonicalizer.canonicalize(matrix)
    }

    fn successors<'a>(&'a self, component: &'a BitMatrix) -> Self::Successors<'a> {
        CanonicalSuccessors::new(self.successor_groups(component))
    }

    fn successor_groups<'a>(&'a self, component: &'a BitMatrix) -> Self::SuccessorGroups<'a> {
        CanonicalSuccessorGroups::new(component, &self.canonicalizer)
    }

    fn estimated_successors(&self, component: &BitMatrix) -> usize {
        let transposed = component.transposed();
        representative_move_count(component).saturating_add(representative_move_count(&transposed))
    }
}

pub struct CanonicalSuccessors<'a, C> {
    groups: CanonicalSuccessorGroups<'a, C>,
    group_index: usize,
    move_index: usize,
}

impl<'a, C> CanonicalSuccessors<'a, C>
where
    C: Canonicalizer,
{
    fn new(groups: CanonicalSuccessorGroups<'a, C>) -> Self {
        Self {
            groups,
            group_index: 0,
            move_index: 0,
        }
    }
}

impl<C> Iterator for CanonicalSuccessors<'_, C>
where
    C: Canonicalizer,
{
    type Item = CanonicalGame;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.group_index == self.groups.len() {
                return None;
            }
            let group_len = self.groups.group_len(self.group_index);
            if self.move_index < group_len {
                let successor = self.groups.successor(self.group_index, self.move_index);
                self.move_index += 1;
                return Some(successor);
            }
            self.group_index += 1;
            self.move_index = 0;
        }
    }
}

pub struct CanonicalSuccessorGroups<'a, C> {
    canonicalizer: &'a C,
    orientations: [BitMatrix; 2],
    specs: Vec<RowGroupSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Orientation {
    Original,
    Transposed,
}

#[derive(Clone, Debug)]
struct RowGroupSpec {
    orientation: Orientation,
    row: usize,
    column_classes: Vec<Vec<usize>>,
    len: usize,
}

impl<'a, C> CanonicalSuccessorGroups<'a, C>
where
    C: Canonicalizer,
{
    fn new(component: &BitMatrix, canonicalizer: &'a C) -> Self {
        let orientations = [component.clone(), component.transposed()];
        let mut specs = Vec::new();
        collect_group_specs(
            &orientations[0],
            &orientations[1],
            Orientation::Original,
            &mut specs,
        );
        collect_group_specs(
            &orientations[1],
            &orientations[0],
            Orientation::Transposed,
            &mut specs,
        );
        Self {
            canonicalizer,
            orientations,
            specs,
        }
    }

    fn raw_successor(&self, group_index: usize, move_index: usize) -> BitMatrix {
        let spec = &self.specs[group_index];
        assert!(move_index < spec.len);
        let matrix = match spec.orientation {
            Orientation::Original => &self.orientations[0],
            Orientation::Transposed => &self.orientations[1],
        };
        let removed = removal_mask(spec, move_index, matrix.cols());
        let mut result = matrix.clone();
        result.clear_row_bits(spec.row, &removed);
        result
    }
}

impl<C> IndexedSuccessorGroups for CanonicalSuccessorGroups<'_, C>
where
    C: Canonicalizer,
{
    fn len(&self) -> usize {
        self.specs.len()
    }

    fn group_len(&self, group_index: usize) -> usize {
        self.specs[group_index].len
    }

    fn successor(&self, group_index: usize, move_index: usize) -> CanonicalGame {
        self.canonicalizer
            .canonicalize(self.raw_successor(group_index, move_index))
    }
}

fn collect_group_specs(
    matrix: &BitMatrix,
    transposed: &BitMatrix,
    orientation: Orientation,
    specs: &mut Vec<RowGroupSpec>,
) {
    let column_classes = column_classes(transposed);
    let mut previous_row = None;
    for row in 0..matrix.rows() {
        let row_pattern = matrix.row_words(row).to_vec();
        if previous_row.as_ref() == Some(&row_pattern) {
            continue;
        }
        previous_row = Some(row_pattern);

        let row_classes: Vec<_> = column_classes
            .iter()
            .filter(|class| matrix.get(row, class[0]))
            .cloned()
            .collect();
        let len = row_classes
            .iter()
            .map(|class| class.len() + 1)
            .fold(1usize, usize::saturating_mul)
            .saturating_sub(1);
        if len != 0 {
            specs.push(RowGroupSpec {
                orientation,
                row,
                column_classes: row_classes,
                len,
            });
        }
    }
}

fn column_classes(transposed: &BitMatrix) -> Vec<Vec<usize>> {
    let mut class_by_pattern = HashMap::<Vec<u64>, usize>::new();
    let mut classes = Vec::<Vec<usize>>::new();
    for col in 0..transposed.rows() {
        let pattern = transposed.row_words(col);
        if let Some(&class) = class_by_pattern.get(pattern) {
            classes[class].push(col);
        } else {
            class_by_pattern.insert(pattern.to_vec(), classes.len());
            classes.push(vec![col]);
        }
    }
    classes
}

fn removal_mask(spec: &RowGroupSpec, move_index: usize, columns: usize) -> Vec<u64> {
    let mut logical_index = move_index.saturating_add(1);
    let mut removed = vec![0; columns.div_ceil(64)];
    for class in &spec.column_classes {
        let remove_count = logical_index % (class.len() + 1);
        logical_index /= class.len() + 1;
        for &col in class.iter().take(remove_count) {
            removed[col / 64] |= 1 << (col % 64);
        }
    }
    removed
}

fn representative_move_count(matrix: &BitMatrix) -> usize {
    let transposed = matrix.transposed();
    let column_classes = column_classes(&transposed);
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
            column_classes
                .iter()
                .filter(|class| matrix.get(row, class[0]))
                .map(|class| class.len() + 1)
                .fold(1usize, usize::saturating_mul)
                .saturating_sub(1)
        })
        .fold(0, usize::saturating_add)
}

/// Increments a bitset while skipping every bit absent from `allowed`.
///
/// Each word acts as one digit whose values are all subsets of its allowed
/// bits. Carrying between words makes this work for arbitrarily wide rows.
#[cfg(test)]
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
        assert_eq!(generator.estimated_successors(&grid), 10);
        assert_eq!(successors.len(), 10);
        assert_eq!(calls.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn identical_shared_columns_are_enumerated_by_removal_count() {
        let mut matrix = BitMatrix::new(2, 3);
        for col in 0..3 {
            matrix.set(0, col, true);
        }
        matrix.set(1, 0, true);
        matrix.set(1, 1, true);

        let canonicalizer = PseudoCanonicalizer;
        let generator = CanonicalMoveGenerator::new(canonicalizer);
        let optimized: Vec<_> = generator.successors(&matrix).collect();
        let brute = brute_force_successors(&matrix, canonicalizer);

        assert_eq!(generator.estimated_successors(&matrix), 11);
        assert_eq!(optimized.len(), 11);
        assert_eq!(optimized.into_iter().collect::<HashSet<_>>(), brute);
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

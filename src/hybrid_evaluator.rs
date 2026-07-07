//! Serial bounded ruling-out search layered on top of the parallel evaluator.
//!
//! This mirrors the useful part of the impartial-analyzer algorithm without
//! replacing the main evaluator. It caches only monotone facts: if a canonical
//! game has a successor of nimber `k`, then `k` is impossible for that game.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    board::BitMatrix, evaluator::DfsSolver, solver::CanonicalGame, successor::SuccessorGenerator,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundedNimber {
    Exact(usize),
    ExceedsBound,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroProof {
    Zero,
    NonZero,
    Cancelled,
}

#[derive(Debug)]
pub struct HybridEvaluator {
    max_depth: usize,
    ruled_out: Mutex<HashMap<CanonicalGame, NimberBitSet>>,
}

impl HybridEvaluator {
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
            ruled_out: Mutex::new(HashMap::new()),
        }
    }

    pub fn exact_nimber(
        &self,
        matrix: &BitMatrix,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        let game = evaluator.generator().canonicalize(matrix.clone());
        self.exact_nimber_of_canonical(&game, evaluator, cancel)
    }

    pub fn exact_nimber_of_canonical(
        &self,
        game: &CanonicalGame,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        self.bounded_nimber_of_canonical(game, None, evaluator, cancel)
    }

    pub fn bounded_nimber_of_canonical(
        &self,
        game: &CanonicalGame,
        bound: Option<usize>,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        self.bounded_game(game, bound, self.max_depth, evaluator, cancel)
    }

    pub fn prove_zero_of_canonical(
        &self,
        game: &CanonicalGame,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> ZeroProof {
        match self.bounded_nimber_of_canonical(game, Some(0), evaluator, cancel) {
            BoundedNimber::Exact(0) => ZeroProof::Zero,
            BoundedNimber::Exact(_) | BoundedNimber::ExceedsBound => ZeroProof::NonZero,
            BoundedNimber::Cancelled => ZeroProof::Cancelled,
        }
    }

    pub fn ruled_out_cache_len(&self) -> usize {
        self.ruled_out
            .lock()
            .expect("ruled-out cache poisoned")
            .len()
    }

    fn bounded_game(
        &self,
        game: &CanonicalGame,
        bound: Option<usize>,
        depth: usize,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        if cancel.load(Ordering::Relaxed) {
            return BoundedNimber::Cancelled;
        }
        if game.is_empty() {
            return bounded_exact(0, bound);
        }
        if let Some(nimber) = evaluator.cached_nimber_of_canonical(game) {
            return bounded_exact(nimber, bound);
        }
        if game.components().len() > 1 {
            self.bounded_split_game(game, bound, depth, evaluator, cancel)
        } else {
            self.bounded_component(game, &game.components()[0], bound, depth, evaluator, cancel)
        }
    }

    fn bounded_split_game(
        &self,
        game: &CanonicalGame,
        bound: Option<usize>,
        depth: usize,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        let hard_index = game
            .components()
            .iter()
            .enumerate()
            .max_by_key(|(_, component)| evaluator.generator().estimated_successors(component))
            .map(|(index, _)| index)
            .expect("split game has at least one component");

        let mut modifier = 0;
        for index in 0..game.components().len() {
            if index == hard_index {
                continue;
            }
            let part = game.component(index);
            let part_depth = depth.saturating_sub(1);
            let nimber = match self.bounded_game(&part, None, part_depth, evaluator, cancel) {
                BoundedNimber::Exact(nimber) => nimber,
                BoundedNimber::Cancelled => return BoundedNimber::Cancelled,
                BoundedNimber::ExceedsBound => {
                    unreachable!("unbounded exact component evaluation cannot exceed its bound")
                }
            };
            modifier ^= nimber;
        }

        let adjusted_bound = bound.map(|bound| bound | modifier);
        match self.bounded_game(
            &game.component(hard_index),
            adjusted_bound,
            depth,
            evaluator,
            cancel,
        ) {
            BoundedNimber::Exact(nimber) => bounded_exact(modifier ^ nimber, bound),
            BoundedNimber::ExceedsBound => BoundedNimber::ExceedsBound,
            BoundedNimber::Cancelled => BoundedNimber::Cancelled,
        }
    }

    fn bounded_component(
        &self,
        game: &CanonicalGame,
        component: &BitMatrix,
        bound: Option<usize>,
        depth: usize,
        evaluator: &DfsSolver,
        cancel: &Arc<AtomicBool>,
    ) -> BoundedNimber {
        if depth == 0 {
            let Some(nimber) = evaluator.nimber_of_canonical_cancellable(game, cancel) else {
                return BoundedNimber::Cancelled;
            };
            return bounded_exact(nimber, bound);
        }

        let mut impossible = self.cached_impossible(game);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return BoundedNimber::Cancelled;
            }
            let candidate = impossible.lowest_zero_bit();
            if bound.is_some_and(|bound| candidate > bound) {
                return BoundedNimber::ExceedsBound;
            }

            let mut ruled_out = false;
            for successor in evaluator.generator().ordered_successors(component) {
                if cancel.load(Ordering::Relaxed) {
                    return BoundedNimber::Cancelled;
                }
                match self.bounded_game(
                    &successor.game,
                    Some(candidate),
                    depth - 1,
                    evaluator,
                    cancel,
                ) {
                    BoundedNimber::Exact(nimber) if nimber == candidate => {
                        impossible.set(candidate);
                        self.mark_impossible(game, candidate);
                        ruled_out = true;
                        break;
                    }
                    BoundedNimber::Exact(_) | BoundedNimber::ExceedsBound => {}
                    BoundedNimber::Cancelled => return BoundedNimber::Cancelled,
                }
            }

            if !ruled_out {
                evaluator.publish_nimber_of_canonical(game.clone(), candidate);
                return BoundedNimber::Exact(candidate);
            }
        }
    }

    fn cached_impossible(&self, game: &CanonicalGame) -> NimberBitSet {
        self.ruled_out
            .lock()
            .expect("ruled-out cache poisoned")
            .get(game)
            .cloned()
            .unwrap_or_default()
    }

    fn mark_impossible(&self, game: &CanonicalGame, nimber: usize) {
        self.ruled_out
            .lock()
            .expect("ruled-out cache poisoned")
            .entry(game.clone())
            .or_default()
            .set(nimber);
    }
}

fn bounded_exact(nimber: usize, bound: Option<usize>) -> BoundedNimber {
    if bound.is_some_and(|bound| nimber > bound) {
        BoundedNimber::ExceedsBound
    } else {
        BoundedNimber::Exact(nimber)
    }
}

#[derive(Clone, Debug, Default)]
struct NimberBitSet {
    words: Vec<u64>,
}

impl NimberBitSet {
    fn set(&mut self, nimber: usize) {
        let word = nimber / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << (nimber % 64);
    }

    fn lowest_zero_bit(&self) -> usize {
        for (index, &word) in self.words.iter().enumerate() {
            let missing = !word;
            if missing != 0 {
                return index * 64 + missing.trailing_zeros() as usize;
            }
        }
        self.words.len() * 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{evaluator::DfsSolver, solver::CanonicalGame};

    fn dense_rectangle(rows: usize, cols: usize) -> BitMatrix {
        let mut grid = BitMatrix::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                grid.set(row, col, true);
            }
        }
        grid
    }

    #[test]
    fn exact_hybrid_nimber_matches_evaluator() {
        let evaluator = DfsSolver::default();
        let hybrid = HybridEvaluator::new(2);
        let cancel = Arc::new(AtomicBool::new(false));

        for matrix in [
            dense_rectangle(1, 4),
            dense_rectangle(2, 3),
            dense_rectangle(3, 3),
        ] {
            let expected = evaluator.nimber(&matrix);
            assert_eq!(
                hybrid.exact_nimber(&matrix, &evaluator, &cancel),
                BoundedNimber::Exact(expected)
            );
        }
    }

    #[test]
    fn zero_proof_distinguishes_zero_and_nonzero_games() {
        let evaluator = DfsSolver::default();
        let hybrid = HybridEvaluator::new(2);
        let cancel = Arc::new(AtomicBool::new(false));
        let zero = CanonicalGame::from_matrix(&dense_rectangle(2, 2));
        let nonzero = CanonicalGame::from_matrix(&dense_rectangle(1, 3));

        assert_eq!(
            hybrid.prove_zero_of_canonical(&zero, &evaluator, &cancel),
            ZeroProof::Zero
        );
        assert_eq!(
            hybrid.prove_zero_of_canonical(&nonzero, &evaluator, &cancel),
            ZeroProof::NonZero
        );
    }

    #[test]
    fn ruled_out_facts_are_reused() {
        let evaluator = DfsSolver::default();
        let hybrid = HybridEvaluator::new(2);
        let cancel = Arc::new(AtomicBool::new(false));
        let game = CanonicalGame::from_matrix(&dense_rectangle(1, 4));

        assert_eq!(
            hybrid.prove_zero_of_canonical(&game, &evaluator, &cancel),
            ZeroProof::NonZero
        );
        assert!(hybrid.ruled_out_cache_len() > 0);
        assert_eq!(
            hybrid.prove_zero_of_canonical(&game, &evaluator, &cancel),
            ZeroProof::NonZero
        );
    }
}

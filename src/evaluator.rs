//! Parallel depth-first Sprague-Grundy evaluation.

use std::{
    collections::{HashMap, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rayon::{
    ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder,
    iter::{ParallelBridge, ParallelIterator},
    prelude::IntoParallelIterator,
};

use crate::{
    board::BitMatrix,
    solver::{CanonicalGame, PseudoCanonicalizer},
    successor::{CanonicalMoveGenerator, SuccessorGenerator},
    symmetry::InvolutionSymmetryFinder,
};

/// A conservative zero certificate evaluated after a cache miss.
pub trait SymmetryFinder: Send + Sync {
    /// `false` means "not proven"; implementations must never guess `true`.
    fn proves_zero(&self, component: &BitMatrix) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoSymmetryFinder;

impl SymmetryFinder for NoSymmetryFinder {
    fn proves_zero(&self, _component: &BitMatrix) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EvaluatorConfig {
    pub threads: Option<usize>,
    pub cache_shards: usize,
    pub parallel_depth: usize,
    pub parallel_move_threshold: usize,
    pub max_cache_entries: Option<usize>,
    pub cache_low_watermark: f64,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            threads: Some(6),
            cache_shards: 64,
            parallel_depth: 2,
            parallel_move_threshold: 32,
            max_cache_entries: None,
            cache_low_watermark: 0.95,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvaluatorStats {
    /// Total computations started: unique claims plus forced duplicates.
    pub evaluation_attempts: usize,
    /// Reads of an already completed cached nimber.
    pub completed_cache_hits: usize,
    /// Unique positions claimed through `Vacant -> Processing`.
    pub unique_positions_claimed: usize,
    /// Unique positions published through `Processing -> Done`.
    pub completed_positions: usize,
    /// Publications that found the position already completed.
    pub duplicate_publish_races: usize,
    /// Reads that encountered another worker's `Processing` entry.
    pub processing_hits: usize,
    /// Deferred positions completed by their owner before being revisited.
    pub deferred_resolved: usize,
    /// Deferred positions still busy when revisited and computed again.
    pub forced_duplicate_evaluations: usize,
    /// Evaluation attempts terminated by a symmetry zero certificate.
    pub symmetry_zero_certificates: usize,
    /// Position evaluations that selected a parallel expansion path.
    pub parallel_expansions: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EvaluatorProgress {
    pub elapsed: Duration,
    pub cache_entries: usize,
    pub cache_done_entries: usize,
    pub cache_processing_entries: usize,
    pub estimated_cache_bytes: usize,
    pub stats: EvaluatorStats,
    pub evaluations_per_second: f64,
    pub cache_hits_per_second: f64,
    pub unique_positions_per_second: f64,
}

#[derive(Default)]
struct AtomicEvaluatorStats {
    evaluation_attempts: AtomicUsize,
    completed_cache_hits: AtomicUsize,
    unique_positions_claimed: AtomicUsize,
    completed_positions: AtomicUsize,
    duplicate_publish_races: AtomicUsize,
    processing_hits: AtomicUsize,
    deferred_resolved: AtomicUsize,
    forced_duplicate_evaluations: AtomicUsize,
    symmetry_zero_certificates: AtomicUsize,
    parallel_expansions: AtomicUsize,
}

impl AtomicEvaluatorStats {
    fn snapshot(&self) -> EvaluatorStats {
        EvaluatorStats {
            evaluation_attempts: self.evaluation_attempts.load(Ordering::Relaxed),
            completed_cache_hits: self.completed_cache_hits.load(Ordering::Relaxed),
            unique_positions_claimed: self.unique_positions_claimed.load(Ordering::Relaxed),
            completed_positions: self.completed_positions.load(Ordering::Relaxed),
            duplicate_publish_races: self.duplicate_publish_races.load(Ordering::Relaxed),
            processing_hits: self.processing_hits.load(Ordering::Relaxed),
            deferred_resolved: self.deferred_resolved.load(Ordering::Relaxed),
            forced_duplicate_evaluations: self.forced_duplicate_evaluations.load(Ordering::Relaxed),
            symmetry_zero_certificates: self.symmetry_zero_certificates.load(Ordering::Relaxed),
            parallel_expansions: self.parallel_expansions.load(Ordering::Relaxed),
        }
    }
}

struct ShardedCache {
    shards: Vec<Mutex<HashMap<CanonicalGame, CacheEntry>>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CacheProfile {
    entries: usize,
    done: usize,
    processing: usize,
    estimated_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheEntry {
    Processing,
    Done(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheProbe {
    Claimed,
    Busy,
    Done(usize),
}

impl ShardedCache {
    fn new(shard_count: usize) -> Self {
        assert!(shard_count > 0, "the evaluator needs a cache shard");
        Self {
            shards: (0..shard_count)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
        }
    }

    fn probe(&self, game: &CanonicalGame) -> CacheProbe {
        let shard = self.shard(game);
        let mut guard = self.shards[shard].lock().expect("cache shard poisoned");
        match guard.entry(game.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(CacheEntry::Processing);
                CacheProbe::Claimed
            }
            Entry::Occupied(entry) => match entry.get() {
                CacheEntry::Processing => CacheProbe::Busy,
                CacheEntry::Done(nimber) => CacheProbe::Done(*nimber),
            },
        }
    }

    fn get_done(&self, game: &CanonicalGame) -> Option<usize> {
        match self.shards[self.shard(game)]
            .lock()
            .expect("cache shard poisoned")
            .get(game)
        {
            Some(CacheEntry::Done(nimber)) => Some(*nimber),
            Some(CacheEntry::Processing) | None => None,
        }
    }

    /// Returns true if this call inserted the value. A false result is a
    /// harmless completed-result race.
    fn insert_if_absent(
        &self,
        game: CanonicalGame,
        nimber: usize,
        max_entries: Option<usize>,
        low_watermark: f64,
    ) -> bool {
        let shard = self.shard(&game);
        let mut guard = self.shards[shard].lock().expect("cache shard poisoned");
        if let Some(max_entries) = max_entries {
            let shard_max = max_entries.div_ceil(self.shards.len()).max(1);
            if guard.len() >= shard_max {
                let target = ((shard_max as f64) * low_watermark.clamp(0.0, 1.0))
                    .floor()
                    .max(1.0) as usize;
                evict_done_entries(&mut guard, target, stable_game_hash(&game));
            }
        }
        match guard.entry(game) {
            Entry::Vacant(entry) => {
                entry.insert(CacheEntry::Done(nimber));
                true
            }
            Entry::Occupied(mut entry) => match entry.get() {
                CacheEntry::Processing => {
                    entry.insert(CacheEntry::Done(nimber));
                    true
                }
                CacheEntry::Done(existing) => {
                    debug_assert_eq!(*existing, nimber);
                    false
                }
            },
        }
    }

    fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.lock().expect("cache shard poisoned").len())
            .sum()
    }

    fn profile(&self) -> CacheProfile {
        let mut profile = CacheProfile::default();
        for shard in &self.shards {
            let guard = shard.lock().expect("cache shard poisoned");
            profile.entries += guard.len();
            profile.estimated_bytes += std::mem::size_of::<HashMap<CanonicalGame, CacheEntry>>();
            for (game, entry) in guard.iter() {
                match entry {
                    CacheEntry::Processing => profile.processing += 1,
                    CacheEntry::Done(_) => profile.done += 1,
                }
                profile.estimated_bytes += estimate_cache_entry_bytes(game);
            }
        }
        profile
    }

    fn clear(&self) {
        for shard in &self.shards {
            shard.lock().expect("cache shard poisoned").clear();
        }
    }

    fn clear_processing(&self) {
        for shard in &self.shards {
            shard
                .lock()
                .expect("cache shard poisoned")
                .retain(|_, entry| matches!(entry, CacheEntry::Done(_)));
        }
    }

    fn shard(&self, game: &CanonicalGame) -> usize {
        let mut hasher = DefaultHasher::new();
        game.hash(&mut hasher);
        hasher.finish() as usize % self.shards.len()
    }
}

fn evict_done_entries(
    guard: &mut HashMap<CanonicalGame, CacheEntry>,
    target: usize,
    new_game_hash: u64,
) {
    while guard.len() > target {
        let Some(victim) = guard
            .iter()
            .filter(|(_, entry)| matches!(entry, CacheEntry::Done(_)))
            .max_by_key(|(game, _)| stable_game_hash(game) ^ new_game_hash.rotate_left(17))
            .map(|(game, _)| game.clone())
        else {
            return;
        };
        guard.remove(&victim);
    }
}

fn estimate_cache_entry_bytes(game: &CanonicalGame) -> usize {
    std::mem::size_of::<CanonicalGame>()
        + std::mem::size_of::<CacheEntry>()
        + game
            .components()
            .iter()
            .map(BitMatrix::estimated_bytes)
            .sum::<usize>()
}

fn stable_game_hash(game: &CanonicalGame) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for component in game.components() {
        hash = fnv_mix(hash, component.rows() as u64);
        hash = fnv_mix(hash, component.cols() as u64);
        for row in 0..component.rows() {
            for &word in component.row_words(row) {
                hash = fnv_mix(hash, word);
            }
        }
    }
    hash
}

fn fnv_mix(mut hash: u64, value: u64) -> u64 {
    for byte in value.to_le_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The evaluator only depends on canonical successor generation and an
/// optional symmetry-based zero certificate.
pub struct Evaluator<G, S = NoSymmetryFinder> {
    generator: G,
    symmetry_finder: S,
    cache: ShardedCache,
    pool: ThreadPool,
    config: EvaluatorConfig,
    stats: AtomicEvaluatorStats,
    created_at: Instant,
}

impl<G, S> Evaluator<G, S>
where
    G: SuccessorGenerator,
    S: SymmetryFinder,
{
    pub fn new(generator: G, symmetry_finder: S) -> Self {
        Self::with_config(generator, symmetry_finder, EvaluatorConfig::default())
            .expect("failed to create evaluator worker pool")
    }

    pub fn with_config(
        generator: G,
        symmetry_finder: S,
        config: EvaluatorConfig,
    ) -> Result<Self, ThreadPoolBuildError> {
        assert!(config.cache_shards > 0, "cache_shards must be positive");
        let mut builder = ThreadPoolBuilder::new();
        if let Some(threads) = config.threads {
            assert!(threads > 0, "threads must be positive");
            builder = builder.num_threads(threads);
        }

        Ok(Self {
            generator,
            symmetry_finder,
            cache: ShardedCache::new(config.cache_shards),
            pool: builder.build()?,
            config,
            stats: AtomicEvaluatorStats::default(),
            created_at: Instant::now(),
        })
    }

    pub fn nimber(&self, matrix: &BitMatrix) -> usize {
        let game = self.generator.canonicalize(matrix.clone());
        self.nimber_of_canonical(&game)
    }

    pub fn nimber_root_parallel(&self, matrix: &BitMatrix) -> usize {
        let game = self.generator.canonicalize(matrix.clone());
        self.nimber_of_canonical_root_parallel(&game)
    }

    pub fn nimber_grouped_parallel(&self, matrix: &BitMatrix, spread_depth: usize) -> usize {
        let game = self.generator.canonicalize(matrix.clone());
        self.nimber_of_canonical_grouped_parallel(&game, spread_depth)
    }

    pub fn nimber_cancellable(
        &self,
        matrix: &BitMatrix,
        cancel: &Arc<AtomicBool>,
    ) -> Option<usize> {
        let game = self.generator.canonicalize(matrix.clone());
        let nimber = self.nimber_of_canonical_cancellable(&game, cancel);
        if nimber.is_none() {
            self.cache.clear_processing();
        }
        nimber
    }

    pub fn nimber_of_canonical(&self, game: &CanonicalGame) -> usize {
        self.pool
            .install(|| self.nimber_inner(game, self.config.parallel_depth, None))
            .expect("uncancellable evaluation must not be cancelled")
    }

    pub fn nimber_of_canonical_root_parallel(&self, game: &CanonicalGame) -> usize {
        self.pool
            .install(|| self.nimber_root_parallel_inner(game, None))
            .expect("uncancellable evaluation must not be cancelled")
    }

    pub fn nimber_of_canonical_grouped_parallel(
        &self,
        game: &CanonicalGame,
        spread_depth: usize,
    ) -> usize {
        self.pool
            .install(|| self.nimber_grouped_inner(game, spread_depth, None))
            .expect("uncancellable evaluation must not be cancelled")
    }

    pub fn nimber_of_canonical_cancellable(
        &self,
        game: &CanonicalGame,
        cancel: &Arc<AtomicBool>,
    ) -> Option<usize> {
        let nimber = self
            .pool
            .install(|| self.nimber_inner(game, self.config.parallel_depth, Some(cancel.as_ref())));
        if nimber.is_none() {
            self.cache.clear_processing();
        }
        nimber
    }

    pub fn cached_nimber_of_canonical(&self, game: &CanonicalGame) -> Option<usize> {
        self.cache.get_done(game)
    }

    pub fn publish_nimber_of_canonical(&self, game: CanonicalGame, nimber: usize) {
        self.cache_result(game, nimber);
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    pub fn stats(&self) -> EvaluatorStats {
        self.stats.snapshot()
    }

    pub fn progress(&self) -> EvaluatorProgress {
        let elapsed = self.created_at.elapsed();
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        let stats = self.stats();
        let cache = self.cache.profile();
        EvaluatorProgress {
            elapsed,
            cache_entries: cache.entries,
            cache_done_entries: cache.done,
            cache_processing_entries: cache.processing,
            estimated_cache_bytes: cache.estimated_bytes,
            stats,
            evaluations_per_second: stats.evaluation_attempts as f64 / seconds,
            cache_hits_per_second: stats.completed_cache_hits as f64 / seconds,
            unique_positions_per_second: stats.unique_positions_claimed as f64 / seconds,
        }
    }

    pub fn cheap_progress(&self) -> EvaluatorProgress {
        let elapsed = self.created_at.elapsed();
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        let stats = self.stats();
        EvaluatorProgress {
            elapsed,
            cache_entries: self.cache.len(),
            cache_done_entries: stats.completed_positions,
            cache_processing_entries: stats
                .unique_positions_claimed
                .saturating_sub(stats.completed_positions),
            estimated_cache_bytes: 0,
            stats,
            evaluations_per_second: stats.evaluation_attempts as f64 / seconds,
            cache_hits_per_second: stats.completed_cache_hits as f64 / seconds,
            unique_positions_per_second: stats.unique_positions_claimed as f64 / seconds,
        }
    }

    pub fn generator(&self) -> &G {
        &self.generator
    }

    fn nimber_inner(
        &self,
        game: &CanonicalGame,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        match self.try_nimber(game, parallel_depth, cancel)? {
            Evaluation::Ready(nimber) => nimber,
            Evaluation::Busy => {
                if let Some(nimber) = self.cache.get_done(game) {
                    self.stats
                        .completed_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    return Some(nimber);
                }
                self.evaluate_duplicate(game, parallel_depth, cancel)?
            }
        }
        .into()
    }

    fn nimber_root_parallel_inner(
        &self,
        game: &CanonicalGame,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if game.is_empty() {
            return Some(0);
        }

        Some(match self.cache.probe(game) {
            CacheProbe::Done(nimber) => {
                self.stats
                    .completed_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                nimber
            }
            CacheProbe::Busy => {
                self.stats.processing_hits.fetch_add(1, Ordering::Relaxed);
                if let Some(nimber) = self.cache.get_done(game) {
                    self.stats
                        .completed_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    nimber
                } else {
                    self.evaluate_duplicate(game, self.config.parallel_depth, cancel)?
                }
            }
            CacheProbe::Claimed => {
                self.stats
                    .unique_positions_claimed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .evaluation_attempts
                    .fetch_add(1, Ordering::Relaxed);
                let nimber = self.compute_position_root_parallel(game, cancel)?;
                self.cache_result(game.clone(), nimber);
                nimber
            }
        })
    }

    fn nimber_grouped_inner(
        &self,
        game: &CanonicalGame,
        spread_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        match self.try_nimber_grouped(game, spread_depth, cancel)? {
            Evaluation::Ready(nimber) => Some(nimber),
            Evaluation::Busy => {
                if let Some(nimber) = self.cache.get_done(game) {
                    self.stats
                        .completed_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    return Some(nimber);
                }
                self.evaluate_duplicate(game, self.config.parallel_depth, cancel)
            }
        }
    }

    fn try_nimber(
        &self,
        game: &CanonicalGame,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<Evaluation> {
        if is_cancelled(cancel) {
            return None;
        }
        if game.is_empty() {
            return Some(Evaluation::Ready(0));
        }

        Some(match self.cache.probe(game) {
            CacheProbe::Done(nimber) => {
                self.stats
                    .completed_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                Evaluation::Ready(nimber)
            }
            CacheProbe::Busy => {
                self.stats.processing_hits.fetch_add(1, Ordering::Relaxed);
                Evaluation::Busy
            }
            CacheProbe::Claimed => {
                self.stats
                    .unique_positions_claimed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .evaluation_attempts
                    .fetch_add(1, Ordering::Relaxed);
                let nimber = self.compute_position(game, parallel_depth, cancel)?;
                self.cache_result(game.clone(), nimber);
                Evaluation::Ready(nimber)
            }
        })
    }

    fn try_nimber_grouped(
        &self,
        game: &CanonicalGame,
        spread_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<Evaluation> {
        if is_cancelled(cancel) {
            return None;
        }
        if spread_depth == 0 {
            return self.try_nimber(game, self.config.parallel_depth, cancel);
        }
        if game.is_empty() {
            return Some(Evaluation::Ready(0));
        }

        Some(match self.cache.probe(game) {
            CacheProbe::Done(nimber) => {
                self.stats
                    .completed_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                Evaluation::Ready(nimber)
            }
            CacheProbe::Busy => {
                self.stats.processing_hits.fetch_add(1, Ordering::Relaxed);
                Evaluation::Busy
            }
            CacheProbe::Claimed => {
                self.stats
                    .unique_positions_claimed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .evaluation_attempts
                    .fetch_add(1, Ordering::Relaxed);
                let nimber = self.compute_position_grouped(game, spread_depth, cancel)?;
                self.cache_result(game.clone(), nimber);
                Evaluation::Ready(nimber)
            }
        })
    }

    fn compute_position(
        &self,
        game: &CanonicalGame,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if game.components().len() > 1 {
            self.nimber_of_components(game, parallel_depth, cancel)
        } else {
            self.nimber_of_component(&game.components()[0], parallel_depth, cancel)
        }
    }

    fn compute_position_root_parallel(
        &self,
        game: &CanonicalGame,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if game.components().len() > 1 {
            self.stats
                .parallel_expansions
                .fetch_add(1, Ordering::Relaxed);
            let nimbers: Option<Vec<_>> = (0..game.components().len())
                .into_par_iter()
                .map(|index| {
                    self.nimber_inner(&game.component(index), self.config.parallel_depth, cancel)
                })
                .collect();
            nimbers.map(|nimbers| nimbers.into_iter().fold(0, |left, right| left ^ right))
        } else {
            self.nimber_of_component_root_parallel(&game.components()[0], cancel)
        }
    }

    fn compute_position_grouped(
        &self,
        game: &CanonicalGame,
        spread_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if spread_depth == 0 {
            return self.compute_position(game, self.config.parallel_depth, cancel);
        }
        if game.components().len() > 1 {
            self.stats
                .parallel_expansions
                .fetch_add(1, Ordering::Relaxed);
            let nimbers: Option<Vec<_>> = (0..game.components().len())
                .into_par_iter()
                .map(|index| {
                    self.nimber_grouped_inner(&game.component(index), spread_depth - 1, cancel)
                })
                .collect();
            nimbers.map(|nimbers| nimbers.into_iter().fold(0, |left, right| left ^ right))
        } else {
            self.nimber_of_component_grouped(&game.components()[0], spread_depth, cancel)
        }
    }

    fn evaluate_duplicate(
        &self,
        game: &CanonicalGame,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        self.stats
            .forced_duplicate_evaluations
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .evaluation_attempts
            .fetch_add(1, Ordering::Relaxed);
        let nimber = self.compute_position(game, parallel_depth, cancel)?;
        self.cache_result(game.clone(), nimber);
        Some(nimber)
    }

    fn nimber_of_components(
        &self,
        game: &CanonicalGame,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if parallel_depth > 0 {
            self.stats
                .parallel_expansions
                .fetch_add(1, Ordering::Relaxed);
            let nimbers: Option<Vec<_>> = (0..game.components().len())
                .into_par_iter()
                .map(|index| self.nimber_inner(&game.component(index), parallel_depth - 1, cancel))
                .collect();
            nimbers.map(|nimbers| nimbers.into_iter().fold(0, |left, right| left ^ right))
        } else {
            let mut nimber = 0;
            for index in 0..game.components().len() {
                nimber ^= self.nimber_inner(&game.component(index), 0, cancel)?;
            }
            Some(nimber)
        }
    }

    fn nimber_of_component(
        &self,
        component: &BitMatrix,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if self.symmetry_finder.proves_zero(component) {
            self.stats
                .symmetry_zero_certificates
                .fetch_add(1, Ordering::Relaxed);
            return Some(0);
        }

        let bound = component.count_ones() + 1;
        let estimated_moves = self.generator.estimated_successors(component);
        let reachable =
            if parallel_depth > 0 && estimated_moves >= self.config.parallel_move_threshold {
                self.stats
                    .parallel_expansions
                    .fetch_add(1, Ordering::Relaxed);
                let worker = self
                    .generator
                    .successors(component)
                    .par_bridge()
                    .fold(
                        || WorkerResult::new(bound),
                        |mut worker, successor| {
                            if is_cancelled(cancel) {
                                worker.cancelled = true;
                            } else {
                                worker.push(
                                    self.try_nimber(&successor, parallel_depth - 1, cancel),
                                    successor,
                                );
                            }
                            worker
                        },
                    )
                    .map(|mut worker| {
                        self.resolve_pending(&mut worker, parallel_depth - 1, cancel);
                        worker
                    })
                    .reduce(
                        || WorkerResult::new(bound),
                        |mut left, right| {
                            left.cancelled |= right.cancelled;
                            left.reachable.union_with(&right.reachable);
                            left
                        },
                    );
                if worker.cancelled {
                    return None;
                }
                worker.reachable
            } else {
                let mut worker = WorkerResult::new(bound);
                for successor in self.generator.successors(component) {
                    if is_cancelled(cancel) {
                        return None;
                    }
                    match self.try_nimber(&successor, parallel_depth, cancel)? {
                        Evaluation::Ready(nimber) => worker.reachable.insert(nimber),
                        Evaluation::Busy => worker.pending.push(successor),
                    }
                }
                self.resolve_pending(&mut worker, parallel_depth, cancel);
                if worker.cancelled {
                    return None;
                }
                worker.reachable
            };
        Some(reachable.mex())
    }

    fn nimber_of_component_root_parallel(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if self.symmetry_finder.proves_zero(component) {
            self.stats
                .symmetry_zero_certificates
                .fetch_add(1, Ordering::Relaxed);
            return Some(0);
        }

        self.stats
            .parallel_expansions
            .fetch_add(1, Ordering::Relaxed);
        let bound = component.count_ones() + 1;
        let successors: Vec<_> = self.generator.successors(component).collect();
        let worker = successors
            .into_par_iter()
            .fold(
                || WorkerResult::new(bound),
                |mut worker, successor| {
                    if is_cancelled(cancel) {
                        worker.cancelled = true;
                    } else {
                        worker.push(
                            self.try_nimber(&successor, self.config.parallel_depth, cancel),
                            successor,
                        );
                    }
                    worker
                },
            )
            .map(|mut worker| {
                self.resolve_pending(&mut worker, self.config.parallel_depth, cancel);
                worker
            })
            .reduce(
                || WorkerResult::new(bound),
                |mut left, right| {
                    left.cancelled |= right.cancelled;
                    left.reachable.union_with(&right.reachable);
                    left
                },
            );
        if worker.cancelled {
            return None;
        }
        Some(worker.reachable.mex())
    }

    fn nimber_of_component_grouped(
        &self,
        component: &BitMatrix,
        spread_depth: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if spread_depth == 0 {
            return self.nimber_of_component(component, self.config.parallel_depth, cancel);
        }
        if self.symmetry_finder.proves_zero(component) {
            self.stats
                .symmetry_zero_certificates
                .fetch_add(1, Ordering::Relaxed);
            return Some(0);
        }

        self.stats
            .parallel_expansions
            .fetch_add(1, Ordering::Relaxed);
        let bound = component.count_ones() + 1;
        let groups: Vec<_> = self.generator.successor_groups(component).collect();
        let worker = groups
            .into_par_iter()
            .fold(
                || WorkerResult::new(bound),
                |mut worker, group| {
                    for successor in group {
                        if is_cancelled(cancel) {
                            worker.cancelled = true;
                            break;
                        }
                        worker.push(
                            self.try_nimber_grouped(&successor, spread_depth - 1, cancel),
                            successor,
                        );
                    }
                    worker
                },
            )
            .map(|mut worker| {
                self.resolve_pending(&mut worker, self.config.parallel_depth, cancel);
                worker
            })
            .reduce(
                || WorkerResult::new(bound),
                |mut left, right| {
                    left.cancelled |= right.cancelled;
                    left.reachable.union_with(&right.reachable);
                    left
                },
            );
        if worker.cancelled {
            return None;
        }
        Some(worker.reachable.mex())
    }

    fn resolve_pending(
        &self,
        worker: &mut WorkerResult,
        parallel_depth: usize,
        cancel: Option<&AtomicBool>,
    ) {
        for game in worker.pending.drain(..) {
            if is_cancelled(cancel) {
                worker.cancelled = true;
                return;
            }
            let nimber = if let Some(nimber) = self.cache.get_done(&game) {
                self.stats
                    .completed_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                self.stats.deferred_resolved.fetch_add(1, Ordering::Relaxed);
                nimber
            } else {
                let Some(nimber) = self.evaluate_duplicate(&game, parallel_depth, cancel) else {
                    worker.cancelled = true;
                    return;
                };
                nimber
            };
            worker.reachable.insert(nimber);
        }
    }

    fn cache_result(&self, game: CanonicalGame, nimber: usize) {
        if self.cache.insert_if_absent(
            game,
            nimber,
            self.config.max_cache_entries,
            self.config.cache_low_watermark,
        ) {
            self.stats
                .completed_positions
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats
                .duplicate_publish_races
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
enum Evaluation {
    Ready(usize),
    Busy,
}

struct WorkerResult {
    reachable: NimberSet,
    pending: Vec<CanonicalGame>,
    cancelled: bool,
}

impl WorkerResult {
    fn new(bit_count: usize) -> Self {
        Self {
            reachable: NimberSet::new(bit_count),
            pending: Vec::new(),
            cancelled: false,
        }
    }

    fn push(&mut self, evaluation: Option<Evaluation>, game: CanonicalGame) {
        match evaluation {
            Some(Evaluation::Ready(nimber)) => self.reachable.insert(nimber),
            Some(Evaluation::Busy) => self.pending.push(game),
            None => self.cancelled = true,
        }
    }
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

#[derive(Clone)]
struct NimberSet {
    words: Vec<u64>,
}

impl NimberSet {
    fn new(bit_count: usize) -> Self {
        Self {
            words: vec![0; bit_count.div_ceil(64)],
        }
    }

    fn insert(&mut self, nimber: usize) {
        if nimber >= self.words.len() * 64 {
            self.words.resize(nimber.div_ceil(64) + 1, 0);
        }
        self.words[nimber / 64] |= 1 << (nimber % 64);
    }

    fn union_with(&mut self, other: &Self) {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (left, right) in self.words.iter_mut().zip(&other.words) {
            *left |= right;
        }
    }

    fn mex(&self) -> usize {
        for (index, &word) in self.words.iter().enumerate() {
            let missing = !word;
            if missing != 0 {
                return index * 64 + missing.trailing_zeros() as usize;
            }
        }
        self.words.len() * 64
    }
}

pub type DfsSolver =
    Evaluator<CanonicalMoveGenerator<PseudoCanonicalizer>, InvolutionSymmetryFinder>;

impl Default for DfsSolver {
    fn default() -> Self {
        Self::new(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            InvolutionSymmetryFinder,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize},
        },
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        board::Maze,
        solver::{Canonicalizer, PseudoCanonicalizer},
        successor::CanonicalMoveGenerator,
    };

    fn heap(size: usize) -> BitMatrix {
        let mut result = BitMatrix::new(1, size);
        for col in 0..size {
            result.set(0, col, true);
        }
        result
    }

    #[test]
    fn single_row_positions_have_heap_nimbers() {
        let solver = DfsSolver::default();
        for size in 1..=5 {
            assert_eq!(solver.nimber(&heap(size)), size);
        }
    }

    #[test]
    fn square_grid_is_a_zero_position() {
        let mut square = BitMatrix::new(2, 2);
        for row in 0..2 {
            for col in 0..2 {
                square.set(row, col, true);
            }
        }
        let evaluator = DfsSolver::default();
        assert_eq!(evaluator.nimber(&square), 0);
        assert_eq!(evaluator.stats().symmetry_zero_certificates, 1);
        assert_eq!(evaluator.stats().evaluation_attempts, 1);
    }

    #[test]
    fn dense_five_by_five_grid_has_nimber_zero() {
        let grid = dense_grid(5);

        let evaluator = DfsSolver::default();
        let nimber = evaluator.nimber(&grid);
        let stats = evaluator.stats();
        println!("{stats:#?}");
        assert_eq!(nimber, 0);
        assert_eq!(
            stats.evaluation_attempts,
            stats.unique_positions_claimed + stats.forced_duplicate_evaluations
        );
        assert_eq!(stats.completed_positions, evaluator.cache_len());
    }

    #[test]
    #[ignore = "manual multicore scaling benchmark"]
    fn dense_five_by_five_grid_scaling() {
        let available = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let mut thread_counts = vec![1, 2, 4, 8, available];
        thread_counts.retain(|&threads| threads <= available);
        thread_counts.sort_unstable();
        thread_counts.dedup();

        let grid = dense_grid(5);
        let mut baseline_time = Duration::ZERO;
        let mut baseline_cache_size = None;

        println!("threads  seconds  speedup  attempts  proc_hits  deferred  forced  unique_states");
        for threads in thread_counts {
            let evaluator = Evaluator::with_config(
                CanonicalMoveGenerator::new(PseudoCanonicalizer),
                NoSymmetryFinder,
                EvaluatorConfig {
                    threads: Some(threads),
                    ..EvaluatorConfig::default()
                },
            )
            .unwrap();

            let start = Instant::now();
            let nimber = evaluator.nimber(&grid);
            let elapsed = start.elapsed();
            let stats = evaluator.stats();
            let cache_size = evaluator.cache_len();

            assert_eq!(nimber, 0);
            if threads == 1 {
                baseline_time = elapsed;
                baseline_cache_size = Some(cache_size);
            } else {
                assert_eq!(Some(cache_size), baseline_cache_size);
            }

            let speedup = baseline_time.as_secs_f64() / elapsed.as_secs_f64();
            println!(
                "{threads:>7}  {:>7.3}  {speedup:>7.2}  {:>9}  {:>9}  {:>8}  {:>6}  {cache_size:>13}",
                elapsed.as_secs_f64(),
                stats.evaluation_attempts,
                stats.processing_hits,
                stats.deferred_resolved,
                stats.forced_duplicate_evaluations,
            );
        }
    }

    #[test]
    #[ignore = "manual symmetry finder A/B benchmark"]
    fn dense_grid_symmetry_ab_benchmark() {
        println!(
            "grid  symmetry    seconds  attempts  done_hits  unique  forced  sym_certs  cache_entries"
        );
        for (label, grid) in [("5x5", dense_grid(5)), ("4x6", dense_rectangle(4, 6))] {
            let without = Evaluator::with_config(
                CanonicalMoveGenerator::new(PseudoCanonicalizer),
                NoSymmetryFinder,
                EvaluatorConfig::default(),
            )
            .unwrap();
            let start = Instant::now();
            let without_nimber = without.nimber(&grid);
            let without_elapsed = start.elapsed();
            print_ab_result(label, "off", without_elapsed, &without);

            let with = Evaluator::with_config(
                CanonicalMoveGenerator::new(PseudoCanonicalizer),
                InvolutionSymmetryFinder,
                EvaluatorConfig::default(),
            )
            .unwrap();
            let start = Instant::now();
            let with_nimber = with.nimber(&grid);
            let with_elapsed = start.elapsed();
            print_ab_result(label, "on", with_elapsed, &with);

            assert_eq!(with_nimber, without_nimber);
            assert_eq!(with_nimber, 0);
        }
    }

    #[test]
    #[ignore = "manual shared-cache performance benchmark"]
    fn shared_cache_benchmark_suite() {
        let games = [
            ("dense 5x5", dense_grid(5)),
            ("dense 3x7", dense_rectangle(3, 7)),
            ("spiral 5x5", spiral_maze_game(5, 5)),
            ("chambers 5x7", chambered_maze_game()),
        ];
        let evaluator = Evaluator::with_config(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            InvolutionSymmetryFinder,
            EvaluatorConfig {
                threads: Some(8),
                parallel_move_threshold: 32,
                ..EvaluatorConfig::default()
            },
        )
        .unwrap();
        let suite_start = Instant::now();

        println!(
            "game          matrix     nodes  nimber  seconds  cache  attempts  hits  unique  forced  sym"
        );
        for (label, game) in games {
            let start = Instant::now();
            let nimber = evaluator.nimber(&game);
            let elapsed = start.elapsed();
            let stats = evaluator.stats();
            println!(
                "{label:<12}  {:>3}x{:<3}  {:>5}  {:>6}  {:>7.3}  {:>5}  {:>8}  {:>4}  {:>6}  {:>6}  {:>3}",
                game.rows(),
                game.cols(),
                game.count_ones(),
                nimber,
                elapsed.as_secs_f64(),
                evaluator.cache_len(),
                stats.evaluation_attempts,
                stats.completed_cache_hits,
                stats.unique_positions_claimed,
                stats.forced_duplicate_evaluations,
                stats.symmetry_zero_certificates,
            );
        }

        let stats = evaluator.stats();
        println!(
            "\ntotal seconds: {:.3}",
            suite_start.elapsed().as_secs_f64()
        );
        println!("final cache entries: {}", evaluator.cache_len());
        println!("final stats: {stats:#?}");
        assert_eq!(
            stats.evaluation_attempts,
            stats.unique_positions_claimed + stats.forced_duplicate_evaluations
        );
        assert_eq!(stats.completed_positions, evaluator.cache_len());
    }

    #[test]
    fn independent_components_xor_and_identical_pairs_cancel() {
        let mut pair = BitMatrix::new(2, 2);
        pair.set(0, 0, true);
        pair.set(1, 1, true);
        let solver = DfsSolver::default();
        assert_eq!(solver.nimber(&pair), 0);
        assert_eq!(solver.cache_len(), 0);

        let mut different = BitMatrix::new(2, 3);
        different.set(0, 0, true);
        different.set(1, 1, true);
        different.set(1, 2, true);
        assert_eq!(solver.nimber(&different), 1 ^ 2);
    }

    #[test]
    fn repeated_evaluation_uses_the_shared_cache() {
        let solver = DfsSolver::default();
        let heap = heap(5);
        let first = solver.nimber(&heap);
        let cache_size = solver.cache_len();
        let hits = solver.stats().completed_cache_hits;
        assert_eq!(solver.nimber(&heap), first);
        assert_eq!(solver.cache_len(), cache_size);
        assert!(solver.stats().completed_cache_hits > hits);
    }

    #[test]
    fn cancellable_evaluation_can_stop_before_claiming_work() {
        let solver = DfsSolver::default();
        let cancel = Arc::new(AtomicBool::new(true));
        assert_eq!(solver.nimber_cancellable(&heap(5), &cancel), None);
        assert_eq!(solver.cache_len(), 0);
    }

    #[test]
    fn cache_claims_processing_positions_before_publishing() {
        let cache = ShardedCache::new(4);
        let game = PseudoCanonicalizer.canonicalize(heap(2));

        assert_eq!(cache.probe(&game), CacheProbe::Claimed);
        assert_eq!(cache.probe(&game), CacheProbe::Busy);
        assert_eq!(cache.get_done(&game), None);
        assert!(cache.insert_if_absent(game.clone(), 2, None, 0.95));
        assert_eq!(cache.probe(&game), CacheProbe::Done(2));
        assert!(!cache.insert_if_absent(game, 2, None, 0.95));
    }

    #[test]
    fn evaluator_calls_the_independent_symmetry_finder() {
        struct Certificate;

        impl SymmetryFinder for Certificate {
            fn proves_zero(&self, component: &BitMatrix) -> bool {
                component.count_ones() == 3
            }
        }

        let evaluator = Evaluator::new(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            Certificate,
        );
        assert_eq!(evaluator.nimber(&heap(3)), 0);
        assert_eq!(evaluator.stats().symmetry_zero_certificates, 1);
    }

    #[test]
    fn evaluator_is_generic_over_the_generators_canonicalizer() {
        struct CountingCanonicalizer {
            calls: Arc<AtomicUsize>,
        }

        impl Canonicalizer for CountingCanonicalizer {
            fn canonicalize(&self, matrix: BitMatrix) -> CanonicalGame {
                self.calls.fetch_add(1, Ordering::Relaxed);
                PseudoCanonicalizer.canonicalize(matrix)
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let evaluator = Evaluator::new(
            CanonicalMoveGenerator::new(CountingCanonicalizer {
                calls: Arc::clone(&calls),
            }),
            NoSymmetryFinder,
        );
        assert_eq!(evaluator.nimber(&heap(3)), 3);
        assert!(calls.load(Ordering::Relaxed) > 1);
    }

    #[test]
    fn parallel_and_sequential_evaluators_agree() {
        let matrix = heap(6);
        let sequential = Evaluator::with_config(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            NoSymmetryFinder,
            EvaluatorConfig {
                threads: Some(1),
                parallel_depth: 0,
                ..EvaluatorConfig::default()
            },
        )
        .unwrap();
        let parallel = Evaluator::with_config(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            NoSymmetryFinder,
            EvaluatorConfig {
                threads: Some(4),
                parallel_depth: 2,
                parallel_move_threshold: 2,
                ..EvaluatorConfig::default()
            },
        )
        .unwrap();

        assert_eq!(parallel.nimber(&matrix), sequential.nimber(&matrix));
        assert!(parallel.stats().parallel_expansions > 0);
    }

    #[test]
    fn root_parallel_evaluator_agrees_with_regular_evaluator() {
        for matrix in [
            heap(6),
            dense_rectangle(3, 4),
            dense_rectangle(2, 5),
            spiral_maze_game(3, 3),
        ] {
            let regular = DfsSolver::default();
            let root_parallel = DfsSolver::default();
            assert_eq!(
                root_parallel.nimber_root_parallel(&matrix),
                regular.nimber(&matrix)
            );
            assert!(root_parallel.stats().parallel_expansions > 0);
        }
    }

    #[test]
    fn grouped_parallel_evaluator_agrees_with_regular_evaluator() {
        for matrix in [
            heap(6),
            dense_rectangle(3, 4),
            dense_rectangle(2, 5),
            spiral_maze_game(3, 3),
        ] {
            let expected = DfsSolver::default().nimber(&matrix);
            for spread_depth in 1..=3 {
                let grouped = DfsSolver::default();
                assert_eq!(
                    grouped.nimber_grouped_parallel(&matrix, spread_depth),
                    expected
                );
                assert!(grouped.stats().parallel_expansions > 0);
            }
        }
    }

    fn dense_grid(size: usize) -> BitMatrix {
        dense_rectangle(size, size)
    }

    fn dense_rectangle(rows: usize, cols: usize) -> BitMatrix {
        let mut grid = BitMatrix::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                grid.set(row, col, true);
            }
        }
        grid
    }

    fn spiral_maze_game(rows: usize, cols: usize) -> BitMatrix {
        assert!(rows > 0 && cols > 0);
        let mut maze = closed_maze(rows, cols);
        let path = spiral_path(rows, cols);

        for pair in path.windows(2) {
            let (first_row, first_col) = pair[0];
            let (second_row, second_col) = pair[1];
            if first_row == second_row {
                maze.set_vertical_wall(first_row, first_col.min(second_col), false);
            } else {
                maze.set_horizontal_wall(first_row.min(second_row), first_col, false);
            }
        }

        crate::solver::compile_maze(&maze)
    }

    fn chambered_maze_game() -> BitMatrix {
        let mut maze = Maze::open(5, 7);

        for row in 0..maze.rows() {
            for col in [1, 4] {
                if !matches!((row, col), (1, 1) | (3, 4)) {
                    maze.add_vertical_wall(row, col);
                }
            }
        }
        for row in [1, 3] {
            for col in 0..maze.cols() {
                if !matches!((row, col), (0, 0) | (1, 3) | (3, 6)) {
                    maze.add_horizontal_wall(row, col);
                }
            }
        }

        crate::solver::compile_maze(&maze)
    }

    fn closed_maze(rows: usize, cols: usize) -> Maze {
        let mut maze = Maze::open(rows, cols);
        for row in 0..rows {
            for col in 0..cols.saturating_sub(1) {
                maze.add_vertical_wall(row, col);
            }
        }
        for row in 0..rows.saturating_sub(1) {
            for col in 0..cols {
                maze.add_horizontal_wall(row, col);
            }
        }
        maze
    }

    fn spiral_path(rows: usize, cols: usize) -> Vec<(usize, usize)> {
        let mut path = Vec::with_capacity(rows * cols);
        let (mut top, mut bottom, mut left, mut right) = (0, rows - 1, 0, cols - 1);

        while left <= right && top <= bottom {
            for col in left..=right {
                path.push((top, col));
            }
            top += 1;

            for row in top..=bottom {
                path.push((row, right));
            }
            let Some(new_right) = right.checked_sub(1) else {
                break;
            };
            right = new_right;

            if top <= bottom {
                for col in (left..=right).rev() {
                    path.push((bottom, col));
                }
                let Some(new_bottom) = bottom.checked_sub(1) else {
                    break;
                };
                bottom = new_bottom;
            }

            if left <= right {
                for row in (top..=bottom).rev() {
                    path.push((row, left));
                }
                left += 1;
            }
        }
        path
    }

    fn print_ab_result<G, S>(
        label: &str,
        symmetry: &str,
        elapsed: Duration,
        evaluator: &Evaluator<G, S>,
    ) where
        G: SuccessorGenerator,
        S: SymmetryFinder,
    {
        let stats = evaluator.stats();
        println!(
            "{label:>4}  {symmetry:>8}  {:>9.6}  {:>9}  {:>10}  {:>6}  {:>6}  {:>9}  {:>13}",
            elapsed.as_secs_f64(),
            stats.evaluation_attempts,
            stats.completed_cache_hits,
            stats.unique_positions_claimed,
            stats.forced_duplicate_evaluations,
            stats.symmetry_zero_certificates,
            evaluator.cache_len(),
        );
    }
}

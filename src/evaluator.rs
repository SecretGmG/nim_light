//! Parallel depth-first Sprague-Grundy evaluation.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs::File,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rayon::{ThreadPool, ThreadPoolBuildError, ThreadPoolBuilder};

use crate::{
    board::BitMatrix,
    solver::{CanonicalGame, RankCanonicalizer},
    successor::{CanonicalMoveGenerator, IndexedSuccessorGroups, SuccessorGenerator},
    symmetry::InvolutionSymmetryFinder,
};

const CACHE_FILE_MAGIC: &[u8; 8] = b"NLCACH02";
const COLLECT_STATS: bool = cfg!(test);

pub const fn stats_collection_enabled() -> bool {
    COLLECT_STATS
}

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
pub struct ToggleableSymmetryFinder {
    enabled: bool,
}

impl ToggleableSymmetryFinder {
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

impl Default for ToggleableSymmetryFinder {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SymmetryFinder for ToggleableSymmetryFinder {
    fn proves_zero(&self, component: &BitMatrix) -> bool {
        self.enabled && InvolutionSymmetryFinder.proves_zero(component)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EvaluatorConfig {
    pub threads: Option<usize>,
    pub cache_shards: usize,
    pub max_cache_entries: Option<usize>,
    pub cache_low_watermark: f64,
}

pub fn recommended_cache_shards(threads: usize) -> usize {
    let factor = if threads >= 64 { 32 } else { 8 };
    threads.saturating_mul(factor).next_power_of_two().max(64)
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        let threads = 6;
        Self {
            threads: Some(threads),
            cache_shards: recommended_cache_shards(threads),
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
    /// Cooperative root/component fanout regions opened.
    pub cooperative_regions: usize,
    /// Successor groups pulled by legacy cooperative workers.
    pub cooperative_group_pulls: usize,
    /// Worker entries into the recursive cooperative evaluator.
    pub cooperative_worker_entries: usize,
    /// Revisited busy positions entered recursively instead of only forced at the leaf.
    pub deferred_descents: usize,
    /// Successor groups deferred because a busy successor was encountered.
    pub group_deferrals: usize,
    /// Deferred successor groups revisited after fresh groups were exhausted.
    pub group_revisits: usize,
    /// Maximum simultaneously active cooperative workers observed.
    pub max_active_workers: usize,
    /// Time-weighted active worker integral, in microseconds.
    pub active_worker_micros: usize,
    /// Successor groups entered by any evaluator path.
    pub successor_groups_started: usize,
    /// Canonical successors contained in entered groups.
    pub successor_group_successors: usize,
    /// Successor groups that claimed at least one new position.
    pub successor_groups_with_new_claim: usize,
    /// Successor groups that encountered at least one busy position.
    pub successor_groups_with_busy: usize,
    /// Revisited deferred groups that were still busy.
    pub successor_revisit_groups_with_busy: usize,
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
    pub time_weighted_active_workers: f64,
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
    cooperative_regions: AtomicUsize,
    cooperative_group_pulls: AtomicUsize,
    cooperative_worker_entries: AtomicUsize,
    deferred_descents: AtomicUsize,
    group_deferrals: AtomicUsize,
    group_revisits: AtomicUsize,
    active_workers: AtomicUsize,
    max_active_workers: AtomicUsize,
    active_worker_clock: Mutex<ActiveWorkerClock>,
    successor_groups_started: AtomicUsize,
    successor_group_successors: AtomicUsize,
    successor_groups_with_new_claim: AtomicUsize,
    successor_groups_with_busy: AtomicUsize,
    successor_revisit_groups_with_busy: AtomicUsize,
}

impl AtomicEvaluatorStats {
    fn snapshot(&self) -> EvaluatorStats {
        if !COLLECT_STATS {
            return EvaluatorStats::default();
        }
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
            cooperative_regions: self.cooperative_regions.load(Ordering::Relaxed),
            cooperative_group_pulls: self.cooperative_group_pulls.load(Ordering::Relaxed),
            cooperative_worker_entries: self.cooperative_worker_entries.load(Ordering::Relaxed),
            deferred_descents: self.deferred_descents.load(Ordering::Relaxed),
            group_deferrals: self.group_deferrals.load(Ordering::Relaxed),
            group_revisits: self.group_revisits.load(Ordering::Relaxed),
            max_active_workers: self.max_active_workers.load(Ordering::Relaxed),
            active_worker_micros: self.active_worker_clock.lock().unwrap().active_micros(),
            successor_groups_started: self.successor_groups_started.load(Ordering::Relaxed),
            successor_group_successors: self.successor_group_successors.load(Ordering::Relaxed),
            successor_groups_with_new_claim: self
                .successor_groups_with_new_claim
                .load(Ordering::Relaxed),
            successor_groups_with_busy: self.successor_groups_with_busy.load(Ordering::Relaxed),
            successor_revisit_groups_with_busy: self
                .successor_revisit_groups_with_busy
                .load(Ordering::Relaxed),
        }
    }

    #[inline]
    fn increment(counter: &AtomicUsize) {
        if COLLECT_STATS {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn completed_cache_hit(&self) {
        Self::increment(&self.completed_cache_hits);
    }

    #[inline]
    fn unique_position_claimed(&self) {
        Self::increment(&self.unique_positions_claimed);
    }

    #[inline]
    fn completed_position(&self) {
        Self::increment(&self.completed_positions);
    }

    #[inline]
    fn duplicate_publish_race(&self) {
        Self::increment(&self.duplicate_publish_races);
    }

    #[inline]
    fn processing_hit(&self) {
        Self::increment(&self.processing_hits);
    }

    #[inline]
    fn deferred_resolved(&self) {
        Self::increment(&self.deferred_resolved);
    }

    #[inline]
    fn forced_duplicate_evaluation(&self) {
        Self::increment(&self.forced_duplicate_evaluations);
    }

    #[inline]
    fn symmetry_zero_certificate(&self) {
        Self::increment(&self.symmetry_zero_certificates);
    }

    #[inline]
    fn cooperative_region(&self) {
        Self::increment(&self.cooperative_regions);
    }

    #[inline]
    fn cooperative_worker_entry(&self) {
        Self::increment(&self.cooperative_worker_entries);
    }

    #[inline]
    fn deferred_descent(&self) {
        Self::increment(&self.deferred_descents);
    }

    #[inline]
    fn group_deferral(&self) {
        Self::increment(&self.group_deferrals);
    }

    #[inline]
    fn group_revisit(&self) {
        Self::increment(&self.group_revisits);
    }

    #[inline]
    fn evaluation_attempt(&self) {
        Self::increment(&self.evaluation_attempts);
    }

    fn observe_active_workers(&self, active: usize) {
        if !COLLECT_STATS {
            return;
        }
        let mut previous = self.max_active_workers.load(Ordering::Relaxed);
        while active > previous {
            match self.max_active_workers.compare_exchange_weak(
                previous,
                active,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => previous = current,
            }
        }
    }

    fn worker_started(&self) {
        if !COLLECT_STATS {
            return;
        }
        let active = self.active_workers.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_worker_clock.lock().unwrap().set_active(active);
        self.observe_active_workers(active);
    }

    fn worker_finished(&self) {
        if !COLLECT_STATS {
            return;
        }
        let active = self
            .active_workers
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        self.active_worker_clock.lock().unwrap().set_active(active);
    }

    fn flush_distribution(&self, distribution: DistributionStats) {
        if !COLLECT_STATS {
            return;
        }
        self.successor_groups_started
            .fetch_add(distribution.successor_groups_started, Ordering::Relaxed);
        self.successor_group_successors
            .fetch_add(distribution.successor_group_successors, Ordering::Relaxed);
        self.successor_groups_with_new_claim.fetch_add(
            distribution.successor_groups_with_new_claim,
            Ordering::Relaxed,
        );
        self.successor_groups_with_busy
            .fetch_add(distribution.successor_groups_with_busy, Ordering::Relaxed);
        self.successor_revisit_groups_with_busy.fetch_add(
            distribution.successor_revisit_groups_with_busy,
            Ordering::Relaxed,
        );
    }
}

#[derive(Debug)]
struct ActiveWorkerClock {
    active: usize,
    last_changed: Instant,
    active_micros: u128,
}

impl Default for ActiveWorkerClock {
    fn default() -> Self {
        Self {
            active: 0,
            last_changed: Instant::now(),
            active_micros: 0,
        }
    }
}

impl ActiveWorkerClock {
    fn set_active(&mut self, active: usize) {
        self.roll_forward();
        self.active = active;
    }

    fn active_micros(&mut self) -> usize {
        self.roll_forward();
        self.active_micros.min(usize::MAX as u128) as usize
    }

    fn roll_forward(&mut self) {
        let now = Instant::now();
        self.active_micros +=
            self.active as u128 * now.duration_since(self.last_changed).as_micros();
        self.last_changed = now;
    }
}

struct ShardedCache {
    shards: Vec<Mutex<HashMap<BitMatrix, CacheEntry>>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheIoReport {
    pub entries: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheSnapshot {
    pub entries: usize,
    pub estimated_bytes: usize,
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

    fn probe(&self, component: &BitMatrix) -> CacheProbe {
        let shard = self.shard(component);
        let mut guard = self.shards[shard].lock().expect("cache shard poisoned");
        match guard.entry(component.clone()) {
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

    fn get_done(&self, component: &BitMatrix) -> Option<usize> {
        match self.shards[self.shard(component)]
            .lock()
            .expect("cache shard poisoned")
            .get(component)
        {
            Some(CacheEntry::Done(nimber)) => Some(*nimber),
            Some(CacheEntry::Processing) | None => None,
        }
    }

    /// Returns true if this call inserted the value. A false result is a
    /// harmless completed-result race.
    fn insert_if_absent(
        &self,
        component: BitMatrix,
        nimber: usize,
        max_entries: Option<usize>,
        low_watermark: f64,
    ) -> bool {
        let shard = self.shard(&component);
        let mut guard = self.shards[shard].lock().expect("cache shard poisoned");
        if let Some(max_entries) = max_entries {
            let shard_max = max_entries.div_ceil(self.shards.len()).max(1);
            if guard.len() >= shard_max {
                let target = ((shard_max as f64) * low_watermark.clamp(0.0, 1.0))
                    .floor()
                    .max(1.0) as usize;
                evict_done_entries(&mut guard, target, stable_matrix_hash(&component));
            }
        }
        match guard.entry(component) {
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
            profile.estimated_bytes += std::mem::size_of::<HashMap<BitMatrix, CacheEntry>>();
            for (component, entry) in guard.iter() {
                match entry {
                    CacheEntry::Processing => profile.processing += 1,
                    CacheEntry::Done(_) => profile.done += 1,
                }
                profile.estimated_bytes += estimate_cache_entry_bytes(component);
            }
        }
        profile
    }

    fn clone_done_into_shards(&self, shard_count: usize) -> Self {
        let copied = Self::new(shard_count);
        for shard in &self.shards {
            let guard = shard.lock().expect("cache shard poisoned");
            for (component, entry) in guard.iter() {
                if let CacheEntry::Done(nimber) = entry {
                    copied.insert_done(component.clone(), *nimber);
                }
            }
        }
        copied
    }

    fn insert_done(&self, component: BitMatrix, nimber: usize) {
        let shard = self.shard(&component);
        self.shards[shard]
            .lock()
            .expect("cache shard poisoned")
            .insert(component, CacheEntry::Done(nimber));
    }

    fn save_to(&self, path: impl AsRef<Path>) -> io::Result<CacheIoReport> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(CACHE_FILE_MAGIC)?;
        write_u64(&mut file, 0)?;
        let mut entries = 0;
        for shard in &self.shards {
            let shard_entries: Vec<_> = {
                let guard = shard.lock().expect("cache shard poisoned");
                guard
                    .iter()
                    .filter_map(|(component, entry)| match entry {
                        CacheEntry::Done(nimber) => Some((component.clone(), *nimber)),
                        CacheEntry::Processing => None,
                    })
                    .collect()
            };

            entries += shard_entries.len();
            for (component, nimber) in shard_entries {
                write_cache_entry(&mut file, &component, nimber)?;
            }
        }
        file.flush()?;
        file.seek(SeekFrom::Start(CACHE_FILE_MAGIC.len() as u64))?;
        write_u64(&mut file, entries)?;
        file.flush()?;
        Ok(CacheIoReport { entries })
    }

    fn load_from(&self, path: impl AsRef<Path>) -> io::Result<CacheIoReport> {
        let mut file = BufReader::new(File::open(path)?);
        let mut magic = [0; CACHE_FILE_MAGIC.len()];
        file.read_exact(&mut magic)?;
        if &magic != CACHE_FILE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a nim_light cache file",
            ));
        }

        let entries = read_usize(&mut file, "cache entry count")?;
        self.clear();
        for _ in 0..entries {
            let (component, nimber) = read_cache_entry(&mut file)?;
            self.insert_done(component, nimber);
        }
        Ok(CacheIoReport { entries })
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

    fn shard(&self, component: &BitMatrix) -> usize {
        let mut hasher = DefaultHasher::new();
        component.hash(&mut hasher);
        hasher.finish() as usize % self.shards.len()
    }
}

fn write_cache_entry(
    writer: &mut impl Write,
    component: &BitMatrix,
    nimber: usize,
) -> io::Result<()> {
    write_u64(writer, nimber)?;
    write_matrix(writer, component)?;
    Ok(())
}

fn read_cache_entry(reader: &mut impl Read) -> io::Result<(BitMatrix, usize)> {
    let nimber = read_usize(reader, "nimber")?;
    let component = read_matrix(reader)?;
    Ok((component, nimber))
}

fn write_matrix(writer: &mut impl Write, matrix: &BitMatrix) -> io::Result<()> {
    write_u64(writer, matrix.rows())?;
    write_u64(writer, matrix.cols())?;
    write_u64(writer, matrix.words().len())?;
    for &word in matrix.words() {
        writer.write_all(&word.to_le_bytes())?;
    }
    Ok(())
}

fn read_matrix(reader: &mut impl Read) -> io::Result<BitMatrix> {
    let rows = read_usize(reader, "matrix rows")?;
    let cols = read_usize(reader, "matrix columns")?;
    let word_count = read_usize(reader, "matrix word count")?;
    let expected_words = rows
        .checked_mul(cols.div_ceil(64))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "matrix dimensions overflow"))?;
    if word_count != expected_words {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "matrix word count does not match dimensions",
        ));
    }

    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        words.push(read_u64(reader)?);
    }
    Ok(BitMatrix::from_words(rows, cols, words))
}

fn write_u64(writer: &mut impl Write, value: usize) -> io::Result<()> {
    writer.write_all(&(value as u64).to_le_bytes())
}

fn read_usize(reader: &mut impl Read, name: &'static str) -> io::Result<usize> {
    let value = read_u64(reader)?;
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} does not fit in usize"),
        )
    })
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; std::mem::size_of::<u64>()];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn evict_done_entries(
    guard: &mut HashMap<BitMatrix, CacheEntry>,
    target: usize,
    new_component_hash: u64,
) {
    while guard.len() > target {
        let Some(victim) = guard
            .iter()
            .filter(|(_, entry)| matches!(entry, CacheEntry::Done(_)))
            .max_by_key(|(component, _)| {
                stable_matrix_hash(component) ^ new_component_hash.rotate_left(17)
            })
            .map(|(component, _)| component.clone())
        else {
            return;
        };
        guard.remove(&victim);
    }
}

fn estimate_cache_entry_bytes(component: &BitMatrix) -> usize {
    std::mem::size_of::<BitMatrix>()
        + std::mem::size_of::<CacheEntry>()
        + component.estimated_bytes()
}

fn stable_matrix_hash(matrix: &BitMatrix) -> u64 {
    stable_matrix_hash_from(0xcbf2_9ce4_8422_2325u64, matrix)
}

fn stable_matrix_hash_from(mut hash: u64, matrix: &BitMatrix) -> u64 {
    hash = fnv_mix(hash, matrix.rows() as u64);
    hash = fnv_mix(hash, matrix.cols() as u64);
    for row in 0..matrix.rows() {
        for &word in matrix.row_words(row) {
            hash = fnv_mix(hash, word);
        }
    }
    hash
}

fn traversal_seed(component: &BitMatrix, depth: usize, worker_seed: usize) -> usize {
    let mut hash = stable_matrix_hash(component);
    hash = fnv_mix(hash, depth as u64);
    hash = fnv_mix(hash, worker_seed as u64);
    (hash ^ hash.rotate_right(32)) as usize
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
    cache: Arc<ShardedCache>,
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
            cache: Arc::new(ShardedCache::new(config.cache_shards)),
            pool: builder.build()?,
            config,
            stats: AtomicEvaluatorStats::default(),
            created_at: Instant::now(),
        })
    }

    fn with_config_and_cache(
        generator: G,
        symmetry_finder: S,
        config: EvaluatorConfig,
        cache: ShardedCache,
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
            cache: Arc::new(cache),
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

    pub fn nimber_cancellable(
        &self,
        matrix: &BitMatrix,
        cancel: &Arc<AtomicBool>,
    ) -> Option<usize> {
        let game = self.generator.canonicalize(matrix.clone());
        self.nimber_of_canonical_cancellable(&game, cancel)
    }

    pub fn nimber_of_canonical(&self, game: &CanonicalGame) -> usize {
        self.pool
            .install(|| self.nimber_cooperative_root_inner(game, None))
            .expect("uncancellable evaluation must not be cancelled")
    }

    pub fn nimber_of_canonical_cancellable(
        &self,
        game: &CanonicalGame,
        cancel: &Arc<AtomicBool>,
    ) -> Option<usize> {
        let nimber = self
            .pool
            .install(|| self.nimber_cooperative_root_inner(game, Some(cancel.as_ref())));
        if nimber.is_none() {
            self.cache.clear_processing();
        }
        nimber
    }

    pub fn cached_nimber_of_canonical(&self, game: &CanonicalGame) -> Option<usize> {
        self.cached_nimber_of_components(game)
    }

    pub fn publish_nimber_of_canonical(&self, game: CanonicalGame, nimber: usize) {
        if let [component] = game.components() {
            self.cache_result(component.clone(), nimber);
        }
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_snapshot(&self) -> CacheSnapshot {
        let profile = self.cache.profile();
        CacheSnapshot {
            entries: profile.entries,
            estimated_bytes: profile.estimated_bytes,
        }
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    pub fn save_cache(&self, path: impl AsRef<Path>) -> io::Result<CacheIoReport> {
        self.cache.save_to(path)
    }

    pub fn load_cache(&self, path: impl AsRef<Path>) -> io::Result<CacheIoReport> {
        self.cache.load_from(path)
    }

    pub fn stats(&self) -> EvaluatorStats {
        self.stats.snapshot()
    }

    pub fn progress(&self) -> EvaluatorProgress {
        let elapsed = self.created_at.elapsed();
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        let stats = self.stats();
        if !COLLECT_STATS {
            return EvaluatorProgress {
                elapsed,
                stats,
                ..EvaluatorProgress::default()
            };
        }
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
            time_weighted_active_workers: stats.active_worker_micros as f64
                / elapsed.as_micros().max(1) as f64,
        }
    }

    pub fn cheap_progress(&self) -> EvaluatorProgress {
        let elapsed = self.created_at.elapsed();
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        let stats = self.stats();
        if !COLLECT_STATS {
            return EvaluatorProgress {
                elapsed,
                stats,
                ..EvaluatorProgress::default()
            };
        }
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
            time_weighted_active_workers: stats.active_worker_micros as f64
                / elapsed.as_micros().max(1) as f64,
        }
    }

    pub fn generator(&self) -> &G {
        &self.generator
    }

    pub fn with_threads_preserving_cache(
        &self,
        threads: usize,
        cache_shards: usize,
    ) -> Result<Self, ThreadPoolBuildError>
    where
        G: Clone,
        S: Clone,
    {
        let mut config = self.config;
        config.threads = Some(threads);
        config.cache_shards = cache_shards;
        let cache = self.cache.clone_done_into_shards(cache_shards);
        Self::with_config_and_cache(
            self.generator.clone(),
            self.symmetry_finder.clone(),
            config,
            cache,
        )
    }

    fn nimber_inner(&self, component: &BitMatrix, cancel: Option<&AtomicBool>) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        match self.try_component_nimber(component, cancel, EvaluationPolicy::Serial, 0)? {
            Evaluation::Ready { nimber, .. } => nimber,
            Evaluation::Busy => {
                if let Some(nimber) = self.cache.get_done(component) {
                    self.stats.completed_cache_hit();
                    return Some(nimber);
                }
                self.evaluate_duplicate_component(component, cancel, 0)?
            }
        }
        .into()
    }

    fn nimber_cooperative_root_inner(
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

        if game.components().len() > 1 {
            return self.nimber_of_components(game, cancel, EvaluationPolicy::Serial, 0);
        }
        let component = &game.components()[0];

        Some(match self.cache.probe(component) {
            CacheProbe::Done(nimber) => {
                self.stats.completed_cache_hit();
                nimber
            }
            CacheProbe::Busy => {
                self.stats.processing_hit();
                if let Some(nimber) = self.cache.get_done(component) {
                    self.stats.completed_cache_hit();
                    nimber
                } else {
                    self.evaluate_duplicate_component(component, cancel, 0)?
                }
            }
            CacheProbe::Claimed => {
                self.stats.unique_position_claimed();
                self.stats.evaluation_attempt();
                let nimber = self.compute_component_cooperative_root(component, cancel)?;
                self.cache_result(component.clone(), nimber);
                nimber
            }
        })
    }

    fn try_nimber(
        &self,
        game: &CanonicalGame,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<Evaluation> {
        if is_cancelled(cancel) {
            return None;
        }
        if game.is_empty() {
            return Some(Evaluation::Ready {
                nimber: 0,
                claimed: false,
            });
        }

        if game.components().len() > 1 {
            return Some(Evaluation::Ready {
                nimber: self.nimber_of_components(game, cancel, policy, depth)?,
                claimed: false,
            });
        }
        self.try_component_nimber(&game.components()[0], cancel, policy, depth)
    }

    fn try_component_nimber(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<Evaluation> {
        if is_cancelled(cancel) {
            return None;
        }

        Some(match self.cache.probe(component) {
            CacheProbe::Done(nimber) => {
                self.stats.completed_cache_hit();
                Evaluation::Ready {
                    nimber,
                    claimed: false,
                }
            }
            CacheProbe::Busy => {
                self.stats.processing_hit();
                Evaluation::Busy
            }
            CacheProbe::Claimed => {
                self.stats.unique_position_claimed();
                self.stats.evaluation_attempt();
                let nimber = self.compute_component(component, cancel, policy, depth)?;
                self.cache_result(component.clone(), nimber);
                Evaluation::Ready {
                    nimber,
                    claimed: true,
                }
            }
        })
    }

    fn compute_component(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        self.evaluate_component_uncached(component, cancel, policy, depth)
    }

    fn compute_component_cooperative_root(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        self.evaluate_component_cooperative_root_uncached(component, cancel)
    }

    fn evaluate_duplicate_component(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
        depth: usize,
    ) -> Option<usize> {
        self.evaluate_duplicate_component_with_policy(
            component,
            cancel,
            EvaluationPolicy::Serial,
            depth,
        )
    }

    fn evaluate_duplicate_component_with_policy(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        self.stats.forced_duplicate_evaluation();
        self.stats.evaluation_attempt();
        let nimber = self.compute_component(component, cancel, policy, depth)?;
        self.cache_result(component.clone(), nimber);
        Some(nimber)
    }

    fn cached_nimber_of_components(&self, game: &CanonicalGame) -> Option<usize> {
        let mut nimber = 0;
        for component in game.components() {
            nimber ^= self.cache.get_done(component)?;
        }
        Some(nimber)
    }

    fn nimber_of_components(
        &self,
        game: &CanonicalGame,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<usize> {
        let mut nimber = 0;
        for component in game.components() {
            nimber ^= match policy {
                EvaluationPolicy::Serial => self.nimber_inner(component, cancel)?,
                EvaluationPolicy::CooperativeAssist => {
                    match self.try_component_nimber(component, cancel, policy, depth + 1)? {
                        Evaluation::Ready { nimber, .. } => nimber,
                        Evaluation::Busy => {
                            self.stats.deferred_descent();
                            self.evaluate_duplicate_component_with_policy(
                                component,
                                cancel,
                                policy,
                                depth + 1,
                            )?
                        }
                    }
                }
            };
        }
        Some(nimber)
    }

    fn evaluate_component_uncached(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if self.symmetry_finder.proves_zero(component) {
            self.stats.symmetry_zero_certificate();
            return Some(0);
        }

        let bound = component.count_ones() + 1;
        let mut worker = WorkerResult::new(bound);
        self.evaluate_successor_groups(
            self.generator.successor_groups(component),
            &mut worker,
            policy,
            traversal_seed(
                component,
                depth,
                self.pool.current_thread_index().unwrap_or(0),
            ),
            depth,
            cancel,
        );
        self.stats.flush_distribution(worker.distribution);
        if worker.cancelled {
            return None;
        }
        Some(worker.reachable.mex())
    }

    fn evaluate_component_cooperative_root_uncached(
        &self,
        component: &BitMatrix,
        cancel: Option<&AtomicBool>,
    ) -> Option<usize> {
        if is_cancelled(cancel) {
            return None;
        }
        if self.symmetry_finder.proves_zero(component) {
            self.stats.symmetry_zero_certificate();
            return Some(0);
        }

        let bound = component.count_ones() + 1;
        let result = Mutex::new(WorkerResult::new(bound));
        let worker_count = self.pool.current_num_threads().max(1);

        self.stats.cooperative_region();

        rayon::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|_| {
                    let mut local = WorkerResult::new(bound);
                    let worker_seed = rayon::current_thread_index().unwrap_or(0);
                    self.stats.cooperative_worker_entry();
                    self.stats.worker_started();
                    self.evaluate_successor_groups_with_seed(
                        self.generator.successor_groups(component),
                        &mut local,
                        EvaluationPolicy::CooperativeAssist,
                        traversal_seed(component, 0, worker_seed),
                        0,
                        cancel,
                    );
                    self.stats.worker_finished();

                    let mut result = result.lock().expect("root result poisoned");
                    result.cancelled |= local.cancelled;
                    result.reachable.union_with(&local.reachable);
                    result.pending.extend(local.pending);
                    result.distribution.merge(local.distribution);
                });
            }
        });

        let mut result = result.into_inner().expect("root result poisoned");
        self.stats.flush_distribution(result.distribution);
        self.resolve_pending(&mut result, cancel);
        if result.cancelled {
            return None;
        }
        Some(result.reachable.mex())
    }

    fn evaluate_successor_groups<IG>(
        &self,
        groups: IG,
        worker: &mut WorkerResult,
        policy: EvaluationPolicy,
        seed: usize,
        depth: usize,
        cancel: Option<&AtomicBool>,
    ) where
        IG: IndexedSuccessorGroups,
    {
        self.evaluate_successor_groups_with_seed(groups, worker, policy, seed, depth, cancel);
    }

    fn evaluate_successor_groups_with_seed<IG>(
        &self,
        groups: IG,
        worker: &mut WorkerResult,
        policy: EvaluationPolicy,
        seed: usize,
        depth: usize,
        cancel: Option<&AtomicBool>,
    ) where
        IG: IndexedSuccessorGroups,
    {
        let mut deferred = Vec::new();
        for group_index in StridedIndices::new(groups.len(), seed) {
            self.evaluate_successor_group_with_deferral(
                &groups,
                DeferredGroup::new(group_index, groups.group_len(group_index), seed),
                worker,
                &mut deferred,
                GroupEvalContext::new(policy, DeferMode::Fresh, depth, cancel),
            );
            if worker.cancelled {
                return;
            }
        }
        self.revisit_deferred_groups(
            deferred,
            &groups,
            worker,
            seed,
            GroupEvalContext::new(policy, DeferMode::Revisit, depth, cancel),
        );
        self.resolve_pending(worker, cancel);
    }

    fn revisit_deferred_groups<IG>(
        &self,
        deferred: Vec<DeferredGroup>,
        groups: &IG,
        worker: &mut WorkerResult,
        seed: usize,
        context: GroupEvalContext<'_>,
    ) where
        IG: IndexedSuccessorGroups,
    {
        for index in StridedIndices::new(deferred.len(), seed) {
            let group = deferred[index];
            self.stats.group_revisit();
            self.evaluate_successor_group_with_deferral(
                groups,
                group,
                worker,
                &mut Vec::new(),
                context,
            );
            if worker.cancelled {
                return;
            }
        }
    }

    fn evaluate_successor_group_with_deferral<IG>(
        &self,
        groups: &IG,
        group: DeferredGroup,
        worker: &mut WorkerResult,
        deferred: &mut Vec<DeferredGroup>,
        context: GroupEvalContext<'_>,
    ) where
        IG: IndexedSuccessorGroups,
    {
        let group_len = groups.group_len(group.group_index);
        let successor_count = group_len.saturating_sub(group.visited);
        let mut had_new_claim = false;
        let mut had_busy = false;
        let mut revisit_busy = false;
        for visited in group.visited..group_len {
            if is_cancelled(context.cancel) {
                worker.cancelled = true;
                worker.record_successor_group(
                    successor_count,
                    had_new_claim,
                    had_busy,
                    revisit_busy,
                );
                return;
            }
            let move_index = group.move_index(visited);
            let successor = groups.successor(group.group_index, move_index);
            match self.try_nimber(
                &successor,
                context.cancel,
                context.policy,
                context.depth + 1,
            ) {
                Some(Evaluation::Ready { nimber, claimed }) => {
                    had_new_claim |= claimed;
                    worker.reachable.insert(nimber);
                }
                Some(Evaluation::Busy) => {
                    had_busy = true;
                    if context.defer_mode == DeferMode::Fresh {
                        self.stats.group_deferral();
                        deferred.push(group.resume_at(visited));
                        worker.record_successor_group(
                            successor_count,
                            had_new_claim,
                            had_busy,
                            revisit_busy,
                        );
                        return;
                    } else {
                        revisit_busy = true;
                        match context.policy {
                            EvaluationPolicy::Serial => worker.pending.push(successor),
                            EvaluationPolicy::CooperativeAssist => {
                                self.stats.deferred_descent();
                                if let Some(nimber) = self.cached_nimber_of_components(&successor) {
                                    self.stats.completed_cache_hit();
                                    self.stats.deferred_resolved();
                                    worker.reachable.insert(nimber);
                                } else {
                                    let Some(nimber) = self.evaluate_duplicate_game_with_policy(
                                        &successor,
                                        context.cancel,
                                        context.policy,
                                        context.depth + 1,
                                    ) else {
                                        worker.cancelled = true;
                                        worker.record_successor_group(
                                            successor_count,
                                            had_new_claim,
                                            had_busy,
                                            revisit_busy,
                                        );
                                        return;
                                    };
                                    worker.reachable.insert(nimber);
                                }
                            }
                        }
                    }
                }
                None => {
                    worker.cancelled = true;
                    worker.record_successor_group(
                        successor_count,
                        had_new_claim,
                        had_busy,
                        revisit_busy,
                    );
                    return;
                }
            }
        }
        worker.record_successor_group(successor_count, had_new_claim, had_busy, revisit_busy);
    }

    fn resolve_pending(&self, worker: &mut WorkerResult, cancel: Option<&AtomicBool>) {
        for game in worker.pending.drain(..) {
            if is_cancelled(cancel) {
                worker.cancelled = true;
                return;
            }
            let nimber = if let Some(nimber) = self.cached_nimber_of_components(&game) {
                self.stats.completed_cache_hit();
                self.stats.deferred_resolved();
                nimber
            } else {
                let Some(nimber) = self.evaluate_duplicate_game_with_policy(
                    &game,
                    cancel,
                    EvaluationPolicy::Serial,
                    0,
                ) else {
                    worker.cancelled = true;
                    return;
                };
                nimber
            };
            worker.reachable.insert(nimber);
        }
    }

    fn evaluate_duplicate_game_with_policy(
        &self,
        game: &CanonicalGame,
        cancel: Option<&AtomicBool>,
        policy: EvaluationPolicy,
        depth: usize,
    ) -> Option<usize> {
        if let [component] = game.components() {
            self.evaluate_duplicate_component_with_policy(component, cancel, policy, depth)
        } else {
            self.nimber_of_components(game, cancel, policy, depth)
        }
    }

    fn cache_result(&self, component: BitMatrix, nimber: usize) {
        if self.cache.insert_if_absent(
            component,
            nimber,
            self.config.max_cache_entries,
            self.config.cache_low_watermark,
        ) {
            self.stats.completed_position();
        } else {
            self.stats.duplicate_publish_race();
        }
    }
}

#[derive(Clone, Copy)]
enum Evaluation {
    Ready { nimber: usize, claimed: bool },
    Busy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvaluationPolicy {
    Serial,
    CooperativeAssist,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeferMode {
    Fresh,
    Revisit,
}

#[derive(Clone, Copy)]
struct GroupEvalContext<'a> {
    policy: EvaluationPolicy,
    defer_mode: DeferMode,
    depth: usize,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> GroupEvalContext<'a> {
    fn new(
        policy: EvaluationPolicy,
        defer_mode: DeferMode,
        depth: usize,
        cancel: Option<&'a AtomicBool>,
    ) -> Self {
        Self {
            policy,
            defer_mode,
            depth,
            cancel,
        }
    }
}

#[derive(Clone, Copy)]
struct DeferredGroup {
    group_index: usize,
    len: usize,
    start: usize,
    stride: usize,
    visited: usize,
}

impl DeferredGroup {
    fn new(group_index: usize, group_len: usize, seed: usize) -> Self {
        if group_len == 0 {
            return Self {
                group_index,
                len: group_len,
                start: 0,
                stride: 1,
                visited: 0,
            };
        }
        let seed = mix_usize(seed, group_index);
        let start = seed % group_len;
        let stride = coprime_stride(seed, group_len);
        Self {
            group_index,
            len: group_len,
            start,
            stride,
            visited: 0,
        }
    }

    fn move_index(self, visited: usize) -> usize {
        (self.start + visited * self.stride) % self.len
    }

    fn resume_at(self, visited: usize) -> Self {
        Self { visited, ..self }
    }
}

struct StridedIndices {
    len: usize,
    start: usize,
    stride: usize,
    visited: usize,
}

impl StridedIndices {
    fn new(len: usize, seed: usize) -> Self {
        if len == 0 {
            return Self {
                len,
                start: 0,
                stride: 1,
                visited: 0,
            };
        }
        let start = seed % len;
        let stride = coprime_stride(seed, len);
        Self {
            len,
            start,
            stride,
            visited: 0,
        }
    }
}

impl Iterator for StridedIndices {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.visited == self.len {
            None
        } else {
            let index = (self.start + self.visited * self.stride) % self.len;
            self.visited += 1;
            Some(index)
        }
    }
}

fn mixed_stride(seed: usize, len: usize) -> usize {
    let mixed = seed
        .wrapping_mul(0x9e37_79b9_7f4a_7c15usize)
        .rotate_left(17)
        | 1;
    1 + mixed % len
}

fn coprime_stride(seed: usize, len: usize) -> usize {
    let mut stride = mixed_stride(seed, len);
    while gcd(stride, len) != 1 {
        stride = stride.saturating_add(1).max(1);
    }
    stride
}

fn mix_usize(seed: usize, value: usize) -> usize {
    seed ^ value
        .wrapping_mul(0x9e37_79b9_7f4a_7c15usize)
        .rotate_left(23)
}

fn gcd(mut first: usize, mut second: usize) -> usize {
    while second != 0 {
        let remainder = first % second;
        first = second;
        second = remainder;
    }
    first
}

struct WorkerResult {
    reachable: NimberSet,
    pending: Vec<CanonicalGame>,
    distribution: DistributionStats,
    cancelled: bool,
}

impl WorkerResult {
    fn new(bit_count: usize) -> Self {
        Self {
            reachable: NimberSet::new(bit_count),
            pending: Vec::new(),
            distribution: DistributionStats::default(),
            cancelled: false,
        }
    }

    fn record_successor_group(
        &mut self,
        successor_count: usize,
        had_new_claim: bool,
        had_busy: bool,
        revisit_busy: bool,
    ) {
        if !COLLECT_STATS {
            return;
        }
        self.distribution.successor_groups_started += 1;
        self.distribution.successor_group_successors += successor_count;
        self.distribution.successor_groups_with_new_claim += usize::from(had_new_claim);
        self.distribution.successor_groups_with_busy += usize::from(had_busy);
        self.distribution.successor_revisit_groups_with_busy += usize::from(revisit_busy);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DistributionStats {
    successor_groups_started: usize,
    successor_group_successors: usize,
    successor_groups_with_new_claim: usize,
    successor_groups_with_busy: usize,
    successor_revisit_groups_with_busy: usize,
}

impl DistributionStats {
    fn merge(&mut self, other: Self) {
        self.successor_groups_started += other.successor_groups_started;
        self.successor_group_successors += other.successor_group_successors;
        self.successor_groups_with_new_claim += other.successor_groups_with_new_claim;
        self.successor_groups_with_busy += other.successor_groups_with_busy;
        self.successor_revisit_groups_with_busy += other.successor_revisit_groups_with_busy;
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

pub type DfsSolver = Evaluator<CanonicalMoveGenerator<RankCanonicalizer>, ToggleableSymmetryFinder>;

impl Default for DfsSolver {
    fn default() -> Self {
        Self::new(
            CanonicalMoveGenerator::new(RankCanonicalizer::default()),
            ToggleableSymmetryFinder::default(),
        )
    }
}

impl DfsSolver {
    pub fn with_symmetry_preserving_cache(
        &self,
        enabled: bool,
    ) -> Result<Self, ThreadPoolBuildError> {
        let cache = self.cache.clone_done_into_shards(self.config.cache_shards);
        Self::with_config_and_cache(
            self.generator.clone(),
            ToggleableSymmetryFinder::new(enabled),
            self.config,
            cache,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize},
        },
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        board::Maze,
        solver::{Canonicalizer, RankCanonicalizer},
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
                CanonicalMoveGenerator::new(RankCanonicalizer::default()),
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
                CanonicalMoveGenerator::new(RankCanonicalizer::default()),
                NoSymmetryFinder,
                EvaluatorConfig::default(),
            )
            .unwrap();
            let start = Instant::now();
            let without_nimber = without.nimber(&grid);
            let without_elapsed = start.elapsed();
            print_ab_result(label, "off", without_elapsed, &without);

            let with = Evaluator::with_config(
                CanonicalMoveGenerator::new(RankCanonicalizer::default()),
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
        run_shared_cache_benchmark_suite(
            "current",
            Evaluator::with_config(
                CanonicalMoveGenerator::new(RankCanonicalizer::default()),
                InvolutionSymmetryFinder,
                EvaluatorConfig {
                    threads: Some(8),
                    ..EvaluatorConfig::default()
                },
            )
            .unwrap(),
        );
    }

    fn run_shared_cache_benchmark_suite<G>(
        label: &str,
        evaluator: Evaluator<G, InvolutionSymmetryFinder>,
    ) where
        G: SuccessorGenerator,
    {
        let games = [
            ("dense 5x5", dense_grid(5)),
            ("dense 3x7", dense_rectangle(3, 7)),
            ("spiral 5x5", spiral_maze_game(5, 5)),
            ("chambers 5x7", chambered_maze_game()),
        ];
        let suite_start = Instant::now();

        println!("\n{label} canonicalizer");
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
        let suite_elapsed = suite_start.elapsed();
        let time_weighted_active_workers =
            stats.active_worker_micros as f64 / suite_elapsed.as_micros().max(1) as f64;
        let groups = stats.successor_groups_started.max(1) as f64;
        let avg_group_size = stats.successor_group_successors as f64 / groups;
        let new_group_rate = stats.successor_groups_with_new_claim as f64 * 100.0 / groups;
        let busy_group_rate = stats.successor_groups_with_busy as f64 * 100.0 / groups;
        let revisit_busy_rate = if stats.group_revisits == 0 {
            0.0
        } else {
            stats.successor_revisit_groups_with_busy as f64 * 100.0 / stats.group_revisits as f64
        };
        println!("\ntotal seconds: {:.3}", suite_elapsed.as_secs_f64());
        println!("final cache entries: {}", evaluator.cache_len());
        println!(
            "distribution: active_workers {:.2}/{}  worker_entries {}  descents {}  groups {}  avg_group_size {:.2}  new_groups {:.1}%  busy_groups {:.1}%  revisit_busy {:.1}%",
            time_weighted_active_workers,
            stats.max_active_workers,
            stats.cooperative_worker_entries,
            stats.deferred_descents,
            stats.successor_groups_started,
            avg_group_size,
            new_group_rate,
            busy_group_rate,
            revisit_busy_rate,
        );
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
    fn cache_can_be_saved_loaded_and_reused() {
        let path = std::env::temp_dir().join(format!(
            "nim_light_cache_test_{}_{}.bin",
            std::process::id(),
            stable_matrix_hash(&heap(1))
        ));
        let _ = fs::remove_file(&path);

        let first = DfsSolver::default();
        assert_eq!(first.nimber(&heap(4)), 4);
        let saved = first.save_cache(&path).unwrap();
        assert!(saved.entries > 0);

        let second = DfsSolver::default();
        let loaded = second.load_cache(&path).unwrap();
        assert_eq!(loaded.entries, saved.entries);
        assert_eq!(
            second.cached_nimber_of_canonical(&CanonicalGame::from_matrix(&heap(4))),
            Some(4)
        );
        assert_eq!(second.nimber(&heap(4)), 4);
        assert!(second.stats().completed_cache_hits > 0);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn changing_threads_preserves_completed_cache_entries() {
        let solver = DfsSolver::default();
        assert_eq!(solver.nimber(&heap(5)), 5);
        let cache_len = solver.cache_len();
        assert!(cache_len > 0);

        let reconfigured = solver.with_threads_preserving_cache(2, 64).unwrap();
        assert_eq!(reconfigured.cache_len(), cache_len);
        assert_eq!(
            reconfigured.cached_nimber_of_canonical(&CanonicalGame::from_matrix(&heap(5))),
            Some(5)
        );
        assert_eq!(reconfigured.nimber(&heap(5)), 5);
        assert!(reconfigured.stats().completed_cache_hits > 0);
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
        let component = CanonicalGame::from_matrix(&heap(2)).components()[0].clone();

        assert_eq!(cache.probe(&component), CacheProbe::Claimed);
        assert_eq!(cache.probe(&component), CacheProbe::Busy);
        assert_eq!(cache.get_done(&component), None);
        assert!(cache.insert_if_absent(component.clone(), 2, None, 0.95));
        assert_eq!(cache.probe(&component), CacheProbe::Done(2));
        assert!(!cache.insert_if_absent(component, 2, None, 0.95));
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
            CanonicalMoveGenerator::new(RankCanonicalizer::default()),
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
                RankCanonicalizer::default().canonicalize(matrix)
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
        let single_threaded = Evaluator::with_config(
            CanonicalMoveGenerator::new(RankCanonicalizer::default()),
            NoSymmetryFinder,
            EvaluatorConfig {
                threads: Some(1),
                ..EvaluatorConfig::default()
            },
        )
        .unwrap();
        let parallel = Evaluator::with_config(
            CanonicalMoveGenerator::new(RankCanonicalizer::default()),
            NoSymmetryFinder,
            EvaluatorConfig {
                threads: Some(4),
                ..EvaluatorConfig::default()
            },
        )
        .unwrap();

        assert_eq!(parallel.nimber(&matrix), single_threaded.nimber(&matrix));
        assert!(parallel.stats().cooperative_regions > 0);
    }

    #[test]
    fn cooperative_evaluator_is_thread_count_independent() {
        for matrix in [
            heap(6),
            dense_rectangle(3, 4),
            dense_rectangle(2, 5),
            spiral_maze_game(3, 3),
        ] {
            let single_threaded = Evaluator::with_config(
                CanonicalMoveGenerator::new(RankCanonicalizer::default()),
                InvolutionSymmetryFinder,
                EvaluatorConfig {
                    threads: Some(1),
                    ..EvaluatorConfig::default()
                },
            )
            .unwrap();
            let parallel = DfsSolver::default();

            assert_eq!(parallel.nimber(&matrix), single_threaded.nimber(&matrix));
            assert!(parallel.stats().cooperative_regions > 0);
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

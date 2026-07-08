# Performance A/B notes

Fast diagnostic benchmark command:

```sh
cargo test --release shared_cache_benchmark_suite -- --ignored --nocapture --test-threads=1
```

Criterion benchmark command:

```sh
cargo bench --bench shared_cache
```

The benchmark itself pins the evaluator to 8 worker threads and evaluates the
suite sequentially on one shared cache:

1. dense `5x5`
2. dense `3x7`
3. compiled `5x5` spiral maze
4. compiled `5x7` chamber maze

The ignored test prints per-position diagnostics and is useful for fast A/B
iteration. Criterion gives the proper benchmark harness, but each full-suite
sample is around ten seconds, so it is slower to iterate on.

## Final retained result

Current code, 8 threads, release diagnostic mode:

```text
game          matrix     nodes  nimber  seconds  cache  attempts  hits     unique  forced  sym
dense 5x5       5x5       25       0    0.760   2992      3063   175112    2992      71    15
dense 3x7       3x7       21       6    0.157   3510      3635   217256    3510     125    18
spiral 5x5     11x15      25       8    2.604  44050     44316   704060   44050     266    18
chambers 5x7   13x19      35       5    6.639  93105     93417  2003228   93105     312    24

total seconds: 10.161
```

Criterion result for the full suite:

```text
shared_cache_suite_8_threads
time: [10.252 s 10.391 s 10.531 s]
```

Criterion reported no statistically significant change for the cap-16
canonicalizer variant relative to its previous baseline.

## Changes kept

### Adjacent duplicate row skip in move generation

`CanonicalSuccessors` previously used `HashSet<Vec<u64>>` to skip duplicate row
patterns per orientation. Because the components passed to the move generator
are already canonicalized and row/column ordered, equal row patterns should be
adjacent. The generator now stores only the previous row pattern.

Effect:

- removes one hash set from the hot move-generation path;
- keeps cheap local row-pattern pruning, while leaving final canonical
  duplicate collapse to the evaluator cache;
- passed the brute-force successor comparison tests.

This is the cleanest retained improvement.

### Adjacent duplicate row skip in move-count estimation

`representative_move_count` used a `HashSet<Vec<u64>>` to count one
representative per exact row bit-pattern. Since canonicalized components keep
equal rows adjacent for the same reason as the iterator path, this now uses the
previous row pattern instead.

This is a small simplification. The release diagnostic benchmark was roughly
neutral/slightly positive; the main value is removing another allocation-heavy
hash set from a hot estimate path.

### Evaluator-cache canonical successor deduplication

`CanonicalSuccessors` no longer keeps a `HashSet<CanonicalGame>` for final
iterator-level deduplication. Duplicate canonical successors are allowed to
flow into the evaluator.

This is sound because mex computation is idempotent over duplicate successor
nimbers: inserting the same reachable nimber bit twice has no effect. The
evaluator cache already collapses duplicate canonical games and records them as
completed-cache hits.

Tradeoff:

- simpler and faster move generator;
- more cache hits / duplicate successor visits;
- clearer ownership: move generation enumerates plausible canonical moves,
  evaluation/cache handles canonical state reuse.

In an earlier run this was the fastest raw variant, and after accepting the
contract change the latest run was:

```text
total seconds: 6.213
```

### Removed canonicalization cycle hash

`refine_and_order` previously hashed every candidate matrix to detect cycles.
The loop already has a deterministic round cap and a direct stable-order stop.
Removing the hash set/hash computation was approximately neutral in the suite,
but it simplifies the canonicalization loop.

This should be watched on future adversarial cases. If a small cycle causes many
extra rounds, restoring cycle detection may be worth it.

### Canonicalization round cap 16

The canonical row/column ordering loop previously capped at `64` rounds. In the
diagnostic benchmark, cap `16` was slightly better than `64`, `8`, and `4`,
while preserving the same number of unique positions.

Criterion did not show a statistically significant improvement, so this should
be treated primarily as a conservative cap/simplification rather than a proven
large win.

### Canonicalization compact-component fast returns

`remove_empty_dimensions` now returns the original compact matrix clone as soon
as it knows all rows and columns are used. It also tracks the used column count
while scanning, so it no longer builds the retained-column vector in the common
already-compact case.

`split_components` also returns the compact matrix directly when the first
graph search proves that the position is a single connected component. This
avoids sorting identity row/column lists and rebuilding the same matrix.

These changes are semantic no-ops and were retained. They mostly help the
canonicalization-only benchmark on connected/compact games and are neutral
elsewhere.

### `BitMatrix::reordered` identity and row-only paths

`BitMatrix::reordered` now handles two common cases before falling back to
bit-by-bit reconstruction:

- full identity reorder: clone the matrix;
- identity columns: copy selected packed row words directly.

This is useful because canonicalization frequently asks for identity or
row-only reorders while evaluating candidate orderings. The row-only path is
also generally useful outside canonicalization.

Retained. It gives a clear dense-grid win and was neutral on the larger maze
cases after avoiding repeated identity scans.

### Identity-column row packing

`packed_row` now copies the existing packed row words directly when the column
order is still the identity order. This avoids bit-by-bit repacking in the
first ordering round and in cases where the column order stabilizes at identity.

Retained as a small local optimization. The benchmark effect was mixed in
isolation but positive in the retained combined variant, especially on the
larger maze cases.

### Direct indexed WL signature list

`refine_colors` now builds the sortable `(Signature, Vertex)` list directly
instead of allocating separate row and column signature vectors and then
chaining them into a third vector. This keeps the same color-refinement
semantics but removes two intermediate vectors per WL round.

The signature list also uses `sort_unstable_by`, because equal signatures get
the same color and their relative order is irrelevant.

Retained. This was mixed on the smallest maze cases but improved most of the
canonicalization benchmark, especially larger maze-like positions.

Latest representative medians versus the original canonicalization benchmark:

```text
game             original     current      change
dense 5x5          9.89 us     8.48 us     -14%
dense 7x7         16.32 us    14.15 us     -13%
dense 10x10       36.25 us    32.09 us     -11%
dense 3x7          8.50 us     7.41 us     -13%
spiral 5x5        33.25 us    34.43 us      +4%
spiral 9x9       192.23 us   163.26 us     -15%
chambers 5x7      31.14 us    29.52 us      -5%
chambers 9x11    158.57 us   145.22 us      -8%
```

The small `spiral_5x5` regression is currently accepted because the larger
maze-like cases and all dense cases improved. This should be rechecked if the
interactive editor workload turns out to be dominated by tiny sparse mazes.

## Rejected / not retained variants

### Full column-bit-pattern grouping in move generation

I tried generalizing leaf compression: for each row, group columns by identical
full column bit-pattern and enumerate only "remove first k from this group".
This is mathematically reasonable and greatly reduces generated moves on dense
symmetric rows.

It was substantially slower on the benchmark:

```text
total seconds: 9.955-10.179 in the older fast baseline shape
```

The likely reason is that full column-pattern grouping adds expensive per-row
setup and changes the parallel/cache dynamics. Even with precomputed column
patterns per orientation, it remained much slower. Rejected.

### Cheap move-count estimate

Replacing exact `estimated_successors` with `component.count_ones()` was very
bad:

```text
total seconds: 39.277
```

It suppressed useful move-level parallelism and made the suite mostly
sequential. Exact move counting is currently worth its cost.

### Parallel move threshold changes

Threshold tuning was noisy and easy to misread because it interacts with thread
count, final successor deduplication policy, and the amount of cache contention.

Observed:

- threshold `8`: roughly neutral/slightly worse than `32`;
- threshold `64/128`: looked good in one intermediate branch, but was not
  stable enough to keep;
- threshold `128` caused a large slowdown in one final benchmark shape because
  move-level parallelism was delayed too much.

Conclusion: keep the default threshold at `32` for now.

### Canonicalization round cap 8 / 4

Reducing `refine_and_order` to `8` or `4` rounds was worse than the retained
`16` cap in the current diagnostic runs.

Conclusion: keep the `16` cap for now.

### Canonicalization lazy transpose view and scratch packing

I added a focused Criterion benchmark for canonicalization-only work:

```sh
cargo bench --bench shared_cache -- canonicalize
```

The suite includes dense grids, spiral mazes, and chamber mazes up to sizes
where computing the full nimber would be unnecessary for this microbenchmark.

Two canonicalizer variants were tested and rejected:

1. Lazy transpose view.
   This avoided allocating `component.transposed()` by routing every access
   through an orientation flag.

2. Scratch-packed permutation refinement.
   This kept row/column permutation maps during refinement and used reusable
   flat `u64` arenas instead of allocating one small `Vec<u64>` per packed row
   or column.

The lazy transpose view was clearly bad for sparse maze-like positions. It
saved one transposed matrix allocation, but replaced it with repeated indirect
access and, more importantly, scans over the wrong logical dimension. The
larger sparse benchmarks regressed substantially.

The scratch/permutation-only variant was mostly neutral and noisy. A
representative median comparison against the original implementation:

```text
game             original     scratch/permutation
dense 5x5          9.89 us        10.06 us
dense 7x7         16.32 us        16.86 us
dense 10x10       36.25 us        36.18 us
dense 3x7          8.50 us         8.60 us
spiral 5x5        33.25 us        32.96 us
spiral 9x9       192.23 us       197.63 us
chambers 5x7      31.14 us        30.70 us
chambers 9x11    158.57 us       174.58 us
```

Conclusion: fully reverted. Candidate matrix materialization and per-row
packed-vector allocation are not currently the dominant canonicalization costs.
The existing direct implementation is simpler and at least as fast in practice.

### WL color-histogram signatures

I tested replacing sorted neighbor-color lists with sorted `(color, count)`
histograms. This is semantically equivalent for WL refinement and looked
plausible for dense regular boards where many neighbor colors repeat.

It regressed across the benchmark:

```text
game             histogram result
dense 5x5        +15%
dense 7x7        +11%
dense 10x10       +8%
dense 3x7        +18%
spiral 5x5       +26%
spiral 9x9       +13%
chambers 5x7     +22%
chambers 9x11    noisy / no clear win
```

Rejected. The extra compression work and pair comparisons cost more than the
shorter signatures save at these board sizes.

Implications for future canonicalization work:

1. Target WL signature construction first.
   The remaining hot path is likely building, sorting, and compressing
   neighbor-color signatures, not final candidate materialization.

2. Measure before changing representation.
   Add direct counters/timers for `canonicalize`, `split_components`,
   `refine_colors`, and `refine_and_order`. Solver-level throughput is too
   indirect for canonicalizer work.

3. Consider reusable signature storage.
   If profiling confirms WL allocation cost, reuse neighbor/color buffers per
   refinement round instead of allocating nested `Vec<usize>` signatures.

4. Improve cache quality only inside unresolved WL classes.
   A bounded exact ordering search over small color classes may be more valuable
   than making the current heuristic loop marginally faster, because better
   canonical collapse can reduce the number of evaluated states.

### Parallel depth 1

Reducing parallel depth from `2` to `1` was slower in the tested benchmark.

Conclusion: keep depth `2`.

### Larger / effectively infinite parallel depth

After moving final successor deduplication to the evaluator cache, I tested
larger `parallel_depth` values with the 8-thread release benchmark:

```text
depth  total seconds  forced duplicates  parallel expansions
2      6.213          411                74
3      6.252          513                384
4      6.254          419                1374
64     6.314          316                92923
```

In the current evaluator, `parallel_depth` is not needed for correctness. It is
a task-spawn budget. The current search evaluates all successors before taking
the mex, so deeper parallelism does not create an early-return path; it mostly
creates more Rayon tasks and more cache traffic. A very high value behaves like
"parallelize almost everywhere" and did not improve this benchmark.

If we later add a true early-exit mex strategy, this should be retested. That
would change the economics because deeper parallel work could be stopped once
the reachable nimber set is sufficient.

## Current next candidates

1. Measure canonicalization call counts and time directly. The current stats
   show solver-level effects, but not where canonicalization time is spent.
2. Try bounded exact canonicalization inside small unresolved WL color classes.
   This is still the most plausible cache-quality improvement.
3. Add solver metrics for generated successor count versus unique claimed
   positions. This would make move-generator tradeoffs less indirect.

## Evaluator distribution baseline

Command:

```sh
cargo test --release shared_cache_benchmark_suite -- --ignored --nocapture --test-threads=1
```

Current 8-thread release diagnostic baseline:

```text
game          matrix     nodes  nimber  seconds  cache  attempts  hits     unique  forced  sym
dense 5x5       5x5       25       0    1.086   2992      3007   168779    2992      15    15
dense 3x7       3x7       21       6    0.474   3510      3525   203923    3510      15    18
spiral 5x5     11x15      25       8    1.576  44050     44617   716163   44050     567    18
chambers 5x7   13x19      35       5   22.495  93105     93672  2012328   93105     567    24

total seconds: 25.631
```

Final distribution metrics:

```text
avg_active_workers:          5.43 / 8  (~67.9% sampled utilization)
max_active_workers:          8
active_worker_samples:       21
cooperative_regions:         3
cooperative_group_pulls:     21

successor_groups_started:    428275
successor_group_successors:  1992789
avg_group_size:              4.65
groups_with_new_claim:       53374   (12.5%)
groups_with_busy:            4449    (1.0%)

processing_hits:             5064    (5.4% of unique positions)
group_deferrals:             3082
group_revisits:              3082
revisit_groups_still_busy:   1367    (44.4% of revisits)
deferred_resolved:           1415    (45.9% of deferrals)
forced_duplicate_evals:       567    (0.6% of unique positions)
duplicate_publish_races:      567
```

Interpretation:

- The cooperative root layer does fill all 8 workers at peak, but the sampled
  average is only about 5.4 active workers. The sample count is low because
  samples are taken per cooperative group pull, so this is a coarse signal, not
  a precise utilization integral.
- Only about 12.5% of successor groups claim new work. Most groups are cache
  probes / already-known nimber propagation.
- Busy groups are rare overall (1.0%), but when a deferred group is revisited,
  44.4% are still busy. Deferral often helps, but collision on the same branch
  remains substantial.
- Forced duplicate evaluation is low relative to unique positions (0.6%), so
  the current scheduler is not losing most time by fully recomputing busy
  states. The larger cost appears to be traversing many cache-heavy successor
  groups and maintaining enough useful frontier work.

Next evaluator/scheduler questions:

1. Replace coarse active-worker sampling with a time-weighted active-worker
   integral if we need accurate utilization.
2. Measure group productivity by depth. The current metrics say many groups do
   not claim new work, but not where in the tree this happens.
3. Try reducing low-productivity group traversal: if a group prefix is mostly
   cache hits and then a busy state, defer earlier or prioritize a different
   group shape.
4. Investigate why `chambers_5x7` dominates the suite. It has many small groups
   and a high cache-hit volume; this may be the best scheduler benchmark.

## Notes for very long computations

Computing something like the dense `7x7` square is likely a different operating
mode than the current interactive/editor use. The important support features are:

1. Cooperative cancellation.
   The evaluator now has a cancellable path and the editor can cancel a running
   nimber computation with `Esc` or `x`. This preserves completed cache entries
   and removes unfinished `Processing` sentinels.

2. Resumability.
   The next practical step is serializing completed canonical cache entries so
   a long run can be stopped and resumed. This matters more than squeezing a few
   percent out of one session.

3. Progress visibility.
   For long runs, report at least cache entries, evaluations/sec, cache-hit
   rate, forced duplicates, and current memory use. The current editor already
   exposes the core solver stats, but not rates or memory.

4. Better canonicalization instrumentation.
   Before adding expensive canonicalizer search, measure unresolved WL classes,
   canonicalizer calls, and time spent canonicalizing. Dense squares are exactly
   where small canonicalization improvements can have large cache effects.

5. Bounded exact canonicalization.
   The most promising algorithmic experiment remains exact permutation inside
   small unresolved WL color classes, with a strict global permutation budget and
   fallback to the current pseudo-canonicalizer.

6. Side-preserving zero certificates.
   Stronger symmetry/zero certificates could eliminate large subtrees, but they
   should remain side-preserving for this representation.

7. Memory policy.
   A `7x7` dense run may be memory-bound before it is CPU-bound. We will need a
   strategy for cache size limits, disk spill, or at least clear reporting when
   the cache is the bottleneck.

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

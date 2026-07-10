# Nim Light performance notes

This file keeps only current, actionable performance information.

## Current default solver

- Canonicalizer: `RankCanonicalizer`, 2 rounds.
- Symmetry finder: configurable in the editor with `y`.
- Default local threads: 6.
- Recommended large-machine starting point from testing: threads around 512,
  cache shards around 1024.

The old WL/order canonicalizer and the hybrid evaluator were removed. The
solver now relies on the simpler parallel DFS evaluator plus the rank
canonicalizer.

## Rank canonicalizer result

Same-build local shared-cache diagnostic:

```text
current rank canonicalizer, 8 threads:
  dense 5x5:      0.297s, cache 2976
  dense 3x7:      0.031s, cache 3494
  spiral 5x5:     0.928s, cache 44103
  chambers 5x7:  13.854s, cache 93758
  total:         15.110s, final cache 93758
```

Compared to the removed canonicalizer on the same suite:

```text
old canonicalizer:
  total: 24.033s
  final cache: 93105
```

Cache growth was about +0.7%; local runtime improved about 37%.

Large-machine dense `7x7` test reported by user:

```text
old canonicalizer:  ~240s
rank canonicalizer: ~175s
```

## Important retained optimizations

- Move generator groups equivalent nodes by identical column bit-pattern.
- Literal transpose-symmetric components skip duplicate transposed move
  generation.
- Successor generation uses indexed groups instead of materialized
  `Vec<Vec<CanonicalGame>>`.
- Cooperative workers recursively descend into busy deferred groups.
- Cache shards are configurable independently from worker threads.
- Cache save/load is available from the editor.

## Useful commands

Fast diagnostic benchmark:

```sh
cargo test --release shared_cache_benchmark_suite -- --ignored --nocapture --test-threads=1
```

Criterion benchmark:

```sh
cargo bench --bench shared_cache
```

Validation:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

# Nim Light

Nim Light is a lightweight terminal implementation of a maze-restricted
hypergraph Nim variant.

The UI uses a compact maze board: live nodes are cells, and walls split
horizontal or vertical corridors. A move removes one or more live nodes from a
single corridor. The last player to move wins.

The solver compiles the maze into a dense bipartite bit-matrix representation.
Rows and columns are the two corridor families, and set bits are remaining
nodes. This keeps move generation and cache keys compact while still allowing
the editor and game to use the clearer maze representation.

## Current features

- Human vs human play.
- Human vs solver CPU play.
- Level editor with live solver statistics.
- Random-access maze editing for nodes and walls.
- Double-space corridor sweep selection.
- Parallel depth-first Sprague-Grundy solver.
- Shared cache across editor and solver CPU gameplay.
- Manual cancellation for long CPU/evaluator runs.
- Configurable solver thread count.
- Cache save/load from the terminal UI.

## Running

```bash
cargo run --release
```

Use release mode for solver work. Debug builds are useful for development but
are much slower for larger positions.

## Editor controls

```text
arrows / hjkl   move cursor
Tab             switch edit target
Space           toggle selected node/wall
n               compute nimber
c               clear cache
S               save completed cache entries to nim_light.cache
L               load completed cache entries from nim_light.cache
[ / ]           decrease / increase solver threads without clearing cache
d / D           decrease / increase parallel depth
f / F           decrease / increase task queue multiplier
+ / -           resize rows
< / >           resize columns
r               reset to demo board
o               reset to open board
m / Esc         return to menu
q               quit
```

Thread changes preserve completed cache entries and rebuild the evaluator with
a cache shard count appropriate for the new thread count. In-flight
`Processing` sentinels are not preserved because they are only meaningful
during one active computation.

The parallel depth and queue multiplier affect both editor nimber computation
and solver CPU move search. The default settings are depth `2` and queue
multiplier `16`.

## Solver notes

The current solver intentionally keeps the core simple:

1. Compile the maze into a dense bit matrix.
2. Canonicalize by removing redundant structure, splitting components, sorting
   dimensions, and cancelling identical independent components.
3. Generate canonical successors in grouped row/column batches.
4. Evaluate with a parallel depth-first search and a shared cache.
5. Use conservative symmetry certificates only when they prove nimber zero.

The default parallel scheduler uses a cooperative root evaluation: all worker
threads pull root successor groups, evaluate their branches depth-first, and
use shared `Processing` cache entries to avoid piling onto already-owned work.
Within grouped evaluation, a busy successor defers the rest of that group on
the first pass; deferred groups are revisited after fresh groups are exhausted.

The parameterized scheduler still supports grouped permit parallelism with
depth 2 and queue multiplier 16 as the default settings. The plain DFS path
remains as an internal fallback and benchmark comparison.

## Reference result

Dense `7 × 7` grid result from a large run:

```text
nimber: 0
previous compiled matrix: 7 × 7 with 49 nodes
previous elapsed: 272.23s
evals 17006066  unique 16963533  done 16963533  hits 3690666965  cache 16963533+0 (~2.3 GiB, sampled 0s ago)
busy 934042  deferred 891509  forced 42533  symmetry 922  parallel 197
62344 eval/s  62188 unique/s  13530025 hit/s  uptime 272.78s
```

This is a useful regression/performance anchor: the position is large enough to
exercise the cache, duplicate-processing behavior, symmetry certificates, and
parallel scheduling without needing a more complicated benchmark harness.

## Benchmarks

Criterion benchmarks live in `benches/shared_cache.rs`.

Run the permit scheduler benchmarks on a large machine with:

```bash
NIM_LIGHT_BENCH_THREADS=128 cargo bench --bench shared_cache -- permit_parallel
```

The benchmark includes:

- `depth_0_dfs_fallback`
- `depth_2_default`
- `depth_3`

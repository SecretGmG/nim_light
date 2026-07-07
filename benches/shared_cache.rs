use std::{
    collections::HashSet,
    env,
    hint::black_box,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use criterion::{Criterion, criterion_group, criterion_main};
use nim_light::{
    board::{Axis, BitMatrix, Cell, Maze},
    evaluator::{DfsSolver, Evaluator, EvaluatorConfig},
    game::{Move, solver_move},
    hybrid_evaluator::HybridEvaluator,
    solver::{CanonicalGame, PseudoCanonicalizer, compile_maze},
    successor::CanonicalMoveGenerator,
    symmetry::InvolutionSymmetryFinder,
};

fn shared_cache_suite(c: &mut Criterion) {
    c.bench_function("shared_cache_suite_8_threads", |bencher| {
        bencher.iter(|| {
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

            for game in benchmark_games() {
                black_box(evaluator.nimber(&game));
            }
            black_box(evaluator.stats());
        });
    });
}

fn exact_nimber_ab(c: &mut Criterion) {
    for (label, game) in labelled_benchmark_games() {
        c.bench_function(&format!("exact_nimber/current/{label}"), |bencher| {
            bencher.iter(|| {
                let evaluator = new_evaluator();
                black_box(evaluator.nimber(&game));
            });
        });

        c.bench_function(&format!("exact_nimber/hybrid_depth_2/{label}"), |bencher| {
            bencher.iter(|| {
                let evaluator = new_evaluator();
                let hybrid = HybridEvaluator::new(2);
                let cancel = Arc::new(AtomicBool::new(false));
                black_box(hybrid.exact_nimber(&game, &evaluator, &cancel));
            });
        });
    }
}

fn zero_ruling_ab(c: &mut Criterion) {
    for (label, game) in labelled_benchmark_games() {
        c.bench_function(&format!("zero_ruling/current_exact/{label}"), |bencher| {
            bencher.iter(|| {
                let evaluator = new_evaluator();
                black_box(evaluator.nimber(&game) == 0);
            });
        });

        c.bench_function(&format!("zero_ruling/hybrid_depth_2/{label}"), |bencher| {
            bencher.iter(|| {
                let evaluator = new_evaluator();
                let hybrid = HybridEvaluator::new(2);
                let cancel = Arc::new(AtomicBool::new(false));
                let canonical = CanonicalGame::from_matrix(&game);
                black_box(hybrid.prove_zero_of_canonical(&canonical, &evaluator, &cancel));
            });
        });
    }
}

fn dense_five_by_five_hybrid_depths(c: &mut Criterion) {
    let game = dense_rectangle(5, 5);
    for depth in 0..=3 {
        c.bench_function(
            &format!("dense_5x5_exact/hybrid_depth_{depth}"),
            |bencher| {
                bencher.iter(|| {
                    let evaluator = new_evaluator();
                    let hybrid = HybridEvaluator::new(depth);
                    let cancel = Arc::new(AtomicBool::new(false));
                    black_box(hybrid.exact_nimber(&game, &evaluator, &cancel));
                });
            },
        );
    }
}

fn dense_three_by_seven_nonzero_proof_depths(c: &mut Criterion) {
    let game = CanonicalGame::from_matrix(&dense_rectangle(3, 7));
    for depth in 0..=3 {
        c.bench_function(
            &format!("dense_3x7_nonzero_proof/hybrid_depth_{depth}"),
            |bencher| {
                bencher.iter(|| {
                    let evaluator = new_evaluator();
                    let hybrid = HybridEvaluator::new(depth);
                    let cancel = Arc::new(AtomicBool::new(false));
                    black_box(hybrid.prove_zero_of_canonical(&game, &evaluator, &cancel));
                });
            },
        );
    }
}

fn cpu_style_find_zero_move(c: &mut Criterion) {
    let maze = Maze::open(3, 7);

    c.bench_function("cpu_find_zero_move/open_3x7/current_exact", |bencher| {
        bencher.iter(|| {
            let evaluator = new_evaluator();
            black_box(solver_move(&maze, &evaluator));
        });
    });

    for depth in 0..=3 {
        c.bench_function(
            &format!("cpu_find_zero_move/open_3x7/hybrid_depth_{depth}"),
            |bencher| {
                bencher.iter(|| {
                    let evaluator = new_evaluator();
                    let hybrid = HybridEvaluator::new(depth);
                    let cancel = Arc::new(AtomicBool::new(false));
                    black_box(hybrid_zero_move(&maze, &evaluator, &hybrid, &cancel));
                });
            },
        );
    }
}

fn root_parallel_ab(c: &mut Criterion) {
    let threads = bench_threads();
    for (label, game) in labelled_benchmark_games() {
        c.bench_function(
            &format!("root_parallel/{threads}_threads/current/{label}"),
            |bencher| {
                bencher.iter(|| {
                    let evaluator = new_evaluator_with_threads(threads);
                    black_box(evaluator.nimber(&game));
                });
            },
        );

        c.bench_function(
            &format!("root_parallel/{threads}_threads/forced_root/{label}"),
            |bencher| {
                bencher.iter(|| {
                    let evaluator = new_evaluator_with_threads(threads);
                    black_box(evaluator.nimber_root_parallel(&game));
                });
            },
        );
    }
}

fn new_evaluator() -> DfsSolver {
    new_evaluator_with_threads(8)
}

fn new_evaluator_with_threads(threads: usize) -> DfsSolver {
    Evaluator::with_config(
        CanonicalMoveGenerator::new(PseudoCanonicalizer),
        InvolutionSymmetryFinder,
        EvaluatorConfig {
            threads: Some(threads),
            cache_shards: cache_shards_for_threads(threads),
            parallel_move_threshold: 32,
            ..EvaluatorConfig::default()
        },
    )
    .unwrap()
}

fn bench_threads() -> usize {
    env::var("NIM_LIGHT_BENCH_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&threads| threads > 0)
        .unwrap_or(8)
}

fn cache_shards_for_threads(threads: usize) -> usize {
    threads.saturating_mul(8).next_power_of_two().max(64)
}

fn hybrid_zero_move(
    maze: &Maze,
    evaluator: &DfsSolver,
    hybrid: &HybridEvaluator,
    cancel: &Arc<AtomicBool>,
) -> Option<Move> {
    for movement in ordered_maze_moves(maze) {
        let mut next = maze.clone();
        next.apply_move(movement.axis, movement.anchor, &movement.cells)
            .expect("generated benchmark moves must be legal");
        let game = CanonicalGame::from_matrix(&compile_maze(&next));
        if matches!(
            hybrid.prove_zero_of_canonical(&game, evaluator, cancel),
            nim_light::hybrid_evaluator::ZeroProof::Zero
        ) {
            return Some(movement);
        }
    }
    None
}

fn ordered_maze_moves(maze: &Maze) -> Vec<Move> {
    let corridors = alive_corridors(maze);
    let max_take = corridors
        .iter()
        .map(|corridor| corridor.alive.len())
        .max()
        .unwrap_or(0);
    let mut moves = Vec::new();
    for take in (1..=max_take).rev() {
        for corridor in corridors
            .iter()
            .filter(|corridor| corridor.alive.len() >= take)
        {
            let mut selected = Vec::with_capacity(take);
            collect_combinations(corridor, take, 0, &mut selected, &mut moves);
        }
    }
    moves
}

#[derive(Clone)]
struct BenchCorridor {
    axis: Axis,
    anchor: Cell,
    alive: Vec<Cell>,
}

fn alive_corridors(maze: &Maze) -> Vec<BenchCorridor> {
    let mut result = Vec::new();
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let mut seen = HashSet::new();
        for row in 0..maze.rows() {
            for col in 0..maze.cols() {
                let corridor = maze.corridor(Cell::new(row, col), axis);
                if !seen.insert(corridor.clone()) {
                    continue;
                }
                let alive: Vec<_> = corridor
                    .iter()
                    .copied()
                    .filter(|&cell| maze.is_alive(cell))
                    .collect();
                if !alive.is_empty() {
                    result.push(BenchCorridor {
                        axis,
                        anchor: corridor[0],
                        alive,
                    });
                }
            }
        }
    }
    result
}

fn collect_combinations(
    corridor: &BenchCorridor,
    take: usize,
    start: usize,
    selected: &mut Vec<Cell>,
    moves: &mut Vec<Move>,
) {
    if selected.len() == take {
        moves.push(Move {
            axis: corridor.axis,
            anchor: corridor.anchor,
            cells: selected.clone(),
        });
        return;
    }

    let remaining = take - selected.len();
    let last_start = corridor.alive.len() - remaining;
    for index in start..=last_start {
        selected.push(corridor.alive[index]);
        collect_combinations(corridor, take, index + 1, selected, moves);
        selected.pop();
    }
}

fn labelled_benchmark_games() -> [(&'static str, BitMatrix); 4] {
    [
        ("dense_5x5", dense_rectangle(5, 5)),
        ("dense_3x7", dense_rectangle(3, 7)),
        ("spiral_5x5", spiral_maze_game(5, 5)),
        ("chambers_5x7", chambered_maze_game()),
    ]
}

fn benchmark_games() -> [BitMatrix; 4] {
    [
        dense_rectangle(5, 5),
        dense_rectangle(3, 7),
        spiral_maze_game(5, 5),
        chambered_maze_game(),
    ]
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

    compile_maze(&maze)
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

    compile_maze(&maze)
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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(30));
    targets = shared_cache_suite, exact_nimber_ab, zero_ruling_ab, dense_five_by_five_hybrid_depths, dense_three_by_seven_nonzero_proof_depths, cpu_style_find_zero_move, root_parallel_ab
}
criterion_main!(benches);

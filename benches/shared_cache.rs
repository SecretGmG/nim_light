use std::{hint::black_box, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use nim_light::{
    board::{BitMatrix, Maze},
    evaluator::{
        DfsSolver, Evaluator, EvaluatorConfig, ToggleableSymmetryFinder, recommended_cache_shards,
    },
    game::solver_move,
    solver::{CanonicalGame, compile_maze},
    successor::CanonicalMoveGenerator,
};

fn shared_cache_suite(c: &mut Criterion) {
    c.bench_function("shared_cache_suite_8_threads", |bencher| {
        bencher.iter(|| {
            let evaluator = new_evaluator();
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
}

fn canonicalization_suite(c: &mut Criterion) {
    for (label, game) in canonicalization_benchmark_games() {
        c.bench_function(&format!("canonicalize/{label}"), |bencher| {
            bencher.iter(|| {
                black_box(CanonicalGame::from_matrix(black_box(&game)));
            });
        });
    }
}

fn new_evaluator() -> DfsSolver {
    new_evaluator_with_threads(8)
}

fn new_evaluator_with_threads(threads: usize) -> DfsSolver {
    Evaluator::with_config(
        CanonicalMoveGenerator::default(),
        ToggleableSymmetryFinder::default(),
        EvaluatorConfig {
            threads: Some(threads),
            cache_shards: cache_shards_for_threads(threads),
            ..EvaluatorConfig::default()
        },
    )
    .unwrap()
}

fn cache_shards_for_threads(threads: usize) -> usize {
    recommended_cache_shards(threads)
}

fn labelled_benchmark_games() -> [(&'static str, BitMatrix); 4] {
    [
        ("dense_5x5", dense_rectangle(5, 5)),
        ("dense_3x7", dense_rectangle(3, 7)),
        ("spiral_5x5", spiral_maze_game(5, 5)),
        ("chambers_5x7", chambered_maze_game()),
    ]
}

fn canonicalization_benchmark_games() -> Vec<(&'static str, BitMatrix)> {
    vec![
        ("dense_5x5", dense_rectangle(5, 5)),
        ("dense_7x7", dense_rectangle(7, 7)),
        ("dense_10x10", dense_rectangle(10, 10)),
        ("dense_3x7", dense_rectangle(3, 7)),
        ("spiral_5x5", spiral_maze_game(5, 5)),
        ("spiral_9x9", spiral_maze_game(9, 9)),
        ("chambers_5x7", chambered_maze_game()),
        ("chambers_9x11", chambered_maze_game_sized(9, 11)),
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
    chambered_maze_game_sized(5, 7)
}

fn chambered_maze_game_sized(rows: usize, cols: usize) -> BitMatrix {
    let mut maze = Maze::open(rows, cols);

    for row in 0..rows {
        for col in (1..cols.saturating_sub(1)).step_by(3) {
            if row % 4 != 1 {
                maze.add_vertical_wall(row, col);
            }
        }
    }
    for row in (1..rows.saturating_sub(1)).step_by(3) {
        for col in 0..cols {
            if col % 4 != 2 {
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
    targets = shared_cache_suite, exact_nimber_ab, cpu_style_find_zero_move
}
criterion_group! {
    name = canonicalization_benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5));
    targets = canonicalization_suite
}
criterion_main!(benches, canonicalization_benches);

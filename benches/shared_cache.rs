use std::{hint::black_box, time::Duration};

use criterion::{Criterion, criterion_group, criterion_main};
use nim_light::{
    board::{BitMatrix, Maze},
    evaluator::{Evaluator, EvaluatorConfig},
    solver::{PseudoCanonicalizer, compile_maze},
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
    targets = shared_cache_suite
}
criterion_main!(benches);

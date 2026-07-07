use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    board::{Axis, Cell, Maze, MoveError},
    evaluator::{DEFAULT_PARALLEL_DEPTH, DEFAULT_PERMIT_FACTOR, DfsSolver},
    solver::compile_maze,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerKind {
    Human,
    SolverCpu,
}

#[derive(Clone, Debug)]
pub struct Player {
    pub name: String,
    pub kind: PlayerKind,
}

#[derive(Clone, Debug)]
pub struct Move {
    pub axis: Axis,
    pub anchor: Cell,
    pub cells: Vec<Cell>,
}

pub struct Game {
    pub maze: Maze,
    pub players: [Player; 2],
    pub turn: usize,
    pub winner: Option<usize>,
    pub last_move: Option<(usize, Move)>,
}

impl Game {
    pub fn human_vs_human() -> Self {
        Self::human_vs_human_on(Maze::demo())
    }

    pub fn human_vs_human_on(maze: Maze) -> Self {
        Self::new(
            maze,
            [
                Player {
                    name: "Player 1".into(),
                    kind: PlayerKind::Human,
                },
                Player {
                    name: "Player 2".into(),
                    kind: PlayerKind::Human,
                },
            ],
        )
    }

    pub fn human_vs_cpu() -> Self {
        Self::human_vs_cpu_on(Maze::demo())
    }

    pub fn human_vs_cpu_on(maze: Maze) -> Self {
        Self::new(
            maze,
            [
                Player {
                    name: "Human".into(),
                    kind: PlayerKind::Human,
                },
                Player {
                    name: "CPU".into(),
                    kind: PlayerKind::SolverCpu,
                },
            ],
        )
    }

    fn new(maze: Maze, players: [Player; 2]) -> Self {
        Self {
            maze,
            players,
            turn: 0,
            winner: None,
            last_move: None,
        }
    }

    pub fn current_player(&self) -> &Player {
        &self.players[self.turn]
    }

    pub fn play(&mut self, movement: Move) -> Result<(), MoveError> {
        self.maze
            .apply_move(movement.axis, movement.anchor, &movement.cells)?;

        let player = self.turn;
        self.last_move = Some((player, movement));
        if self.maze.alive_count() == 0 {
            self.winner = Some(player);
        } else {
            self.turn = 1 - self.turn;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Corridor {
    axis: Axis,
    anchor: Cell,
    alive: Vec<Cell>,
}

#[derive(Clone, Debug)]
pub enum SolverMoveResult {
    Move(Move),
    NoMove,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SolverSearchConfig {
    pub parallel_depth: usize,
    pub permit_factor: usize,
}

impl Default for SolverSearchConfig {
    fn default() -> Self {
        Self {
            parallel_depth: DEFAULT_PARALLEL_DEPTH,
            permit_factor: DEFAULT_PERMIT_FACTOR,
        }
    }
}

pub fn solver_move(maze: &Maze, solver: &DfsSolver) -> Option<Move> {
    let cancel = Arc::new(AtomicBool::new(false));
    match solver_move_cancellable(maze, solver, &cancel) {
        SolverMoveResult::Move(movement) => Some(movement),
        SolverMoveResult::NoMove | SolverMoveResult::Cancelled => None,
    }
}

pub fn solver_move_cancellable(
    maze: &Maze,
    solver: &DfsSolver,
    cancel: &Arc<AtomicBool>,
) -> SolverMoveResult {
    solver_move_cancellable_with_config(maze, solver, SolverSearchConfig::default(), cancel)
}

pub fn solver_move_cancellable_with_config(
    maze: &Maze,
    solver: &DfsSolver,
    config: SolverSearchConfig,
    cancel: &Arc<AtomicBool>,
) -> SolverMoveResult {
    let corridors = alive_corridors(maze);
    let max_take = corridors
        .iter()
        .map(|corridor| corridor.alive.len())
        .max()
        .unwrap_or(0);
    let mut fallback = None;

    for take in (1..=max_take).rev() {
        if cancel.load(Ordering::Relaxed) {
            return SolverMoveResult::Cancelled;
        }
        for corridor in corridors
            .iter()
            .filter(|corridor| corridor.alive.len() >= take)
        {
            if cancel.load(Ordering::Relaxed) {
                return SolverMoveResult::Cancelled;
            }
            let mut selected = Vec::with_capacity(take);
            let mut search = SolverSearch {
                maze,
                solver,
                config,
                corridor,
                cancel,
                fallback: &mut fallback,
            };
            match search.winning_combination(take, 0, &mut selected) {
                SolverMoveResult::Move(movement) => return SolverMoveResult::Move(movement),
                SolverMoveResult::Cancelled => return SolverMoveResult::Cancelled,
                SolverMoveResult::NoMove => {}
            }
        }
    }

    fallback.map_or(SolverMoveResult::NoMove, SolverMoveResult::Move)
}

fn alive_corridors(maze: &Maze) -> Vec<Corridor> {
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
                    result.push(Corridor {
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

struct SolverSearch<'a> {
    maze: &'a Maze,
    solver: &'a DfsSolver,
    config: SolverSearchConfig,
    corridor: &'a Corridor,
    cancel: &'a Arc<AtomicBool>,
    fallback: &'a mut Option<Move>,
}

impl SolverSearch<'_> {
    fn winning_combination(
        &mut self,
        take: usize,
        start: usize,
        selected: &mut Vec<Cell>,
    ) -> SolverMoveResult {
        if self.cancel.load(Ordering::Relaxed) {
            return SolverMoveResult::Cancelled;
        }

        if selected.len() == take {
            let movement = Move {
                axis: self.corridor.axis,
                anchor: self.corridor.anchor,
                cells: selected.clone(),
            };
            self.fallback.get_or_insert_with(|| movement.clone());

            let mut next = self.maze.clone();
            next.apply_move(movement.axis, movement.anchor, &movement.cells)
                .expect("generated solver moves must be legal");
            return match self
                .solver
                .nimber_cancellable_with_parallel_params(
                    &compile_maze(&next),
                    self.config.parallel_depth,
                    self.config.permit_factor,
                    self.cancel,
                )
            {
                Some(0) => SolverMoveResult::Move(movement),
                Some(_) => SolverMoveResult::NoMove,
                None => SolverMoveResult::Cancelled,
            };
        }

        let remaining = take - selected.len();
        let last_start = self.corridor.alive.len() - remaining;
        for index in start..=last_start {
            selected.push(self.corridor.alive[index]);
            match self.winning_combination(take, index + 1, selected) {
                SolverMoveResult::Move(movement) => return SolverMoveResult::Move(movement),
                SolverMoveResult::Cancelled => return SolverMoveResult::Cancelled,
                SolverMoveResult::NoMove => {}
            }
            selected.pop();
        }
        SolverMoveResult::NoMove
    }
}

/// A tiny PRNG is enough for an intentionally non-strategic CPU.
pub struct Random {
    state: u64,
}

impl Random {
    pub fn seeded_from_clock() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        Self { state: seed.max(1) }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn index(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn coin_flip(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

pub fn random_move(maze: &Maze, random: &mut Random) -> Option<Move> {
    if maze.alive_count() == 0 {
        return None;
    }

    let mut alive = Vec::with_capacity(maze.alive_count());
    for row in 0..maze.rows() {
        for col in 0..maze.cols() {
            let cell = Cell::new(row, col);
            if maze.is_alive(cell) {
                alive.push(cell);
            }
        }
    }

    let anchor = alive[random.index(alive.len())];
    let axis = if random.coin_flip() {
        Axis::Horizontal
    } else {
        Axis::Vertical
    };
    let candidates: Vec<_> = maze
        .corridor(anchor, axis)
        .into_iter()
        .filter(|&cell| maze.is_alive(cell))
        .collect();
    let mut cells: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|_| random.coin_flip())
        .collect();

    if cells.is_empty() {
        cells.push(candidates[random.index(candidates.len())]);
    }

    Some(Move {
        axis,
        anchor,
        cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_cpu_always_produces_a_legal_nonempty_move() {
        let mut maze = Maze::demo();
        let mut random = Random { state: 42 };

        while maze.alive_count() > 0 {
            let before = maze.alive_count();
            let movement = random_move(&maze, &mut random).unwrap();
            maze.apply_move(movement.axis, movement.anchor, &movement.cells)
                .unwrap();
            assert!(maze.alive_count() < before);
        }
    }

    #[test]
    fn solver_cpu_prefers_largest_winning_move() {
        let mut maze = Maze::open(1, 3);
        let solver = DfsSolver::default();

        let movement = solver_move(&maze, &solver).unwrap();
        assert_eq!(movement.cells.len(), 3);
        maze.apply_move(movement.axis, movement.anchor, &movement.cells)
            .unwrap();
        assert_eq!(maze.alive_count(), 0);
    }

    #[test]
    fn solver_cpu_generates_legal_moves_until_game_end() {
        let mut maze = Maze::open(2, 3);
        let solver = DfsSolver::default();

        while maze.alive_count() > 0 {
            let before = maze.alive_count();
            let movement = solver_move(&maze, &solver).unwrap();
            maze.apply_move(movement.axis, movement.anchor, &movement.cells)
                .unwrap();
            assert!(maze.alive_count() < before);
        }
    }
}

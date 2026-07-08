use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    board::{Axis, Cell, Maze, MoveError},
    evaluator::{DfsSolver, SymmetryFinder},
    solver::{CanonicalGame, compile_maze},
    symmetry::InvolutionSymmetryFinder,
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
    let mut candidates = unique_solver_candidates(maze, cancel);
    if candidates.is_empty() {
        return if cancel.load(Ordering::Relaxed) {
            SolverMoveResult::Cancelled
        } else {
            SolverMoveResult::NoMove
        };
    }

    candidates.sort_by_key(|candidate| Reverse(candidate.removed));
    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| immediately_zero(&candidate.game))
    {
        return SolverMoveResult::Move(candidate.movement.clone());
    }

    candidates.sort_by_key(|candidate| {
        (
            candidate.nodes,
            candidate.game.components().len(),
            Reverse(candidate.removed),
        )
    });

    for candidate in &candidates {
        if cancel.load(Ordering::Relaxed) {
            return SolverMoveResult::Cancelled;
        }
        if let Some(nimber) = solver.cached_nimber_of_canonical(&candidate.game) {
            if nimber == 0 {
                return SolverMoveResult::Move(candidate.movement.clone());
            }
            continue;
        }
        match solver.nimber_of_canonical_cancellable(&candidate.game, cancel) {
            Some(0) => return SolverMoveResult::Move(candidate.movement.clone()),
            Some(_) => {}
            None => return SolverMoveResult::Cancelled,
        }
    }

    candidates
        .into_iter()
        .max_by_key(|candidate| candidate.removed)
        .map_or(SolverMoveResult::NoMove, |candidate| {
            SolverMoveResult::Move(candidate.movement)
        })
}

#[derive(Clone)]
struct SolverCandidate {
    movement: Move,
    game: CanonicalGame,
    removed: usize,
    nodes: usize,
}

fn unique_solver_candidates(maze: &Maze, cancel: &Arc<AtomicBool>) -> Vec<SolverCandidate> {
    let mut candidates: HashMap<CanonicalGame, SolverCandidate> = HashMap::new();
    let corridors = alive_corridors(maze);
    let max_take = corridors
        .iter()
        .map(|corridor| corridor.alive.len())
        .max()
        .unwrap_or(0);

    for take in (1..=max_take).rev() {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        for corridor in corridors
            .iter()
            .filter(|corridor| corridor.alive.len() >= take)
        {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let mut selected = Vec::with_capacity(take);
            let mut search = CandidateSearch {
                maze,
                corridor,
                cancel,
                candidates: &mut candidates,
            };
            search.collect(take, 0, &mut selected);
        }
    }

    candidates.into_values().collect()
}

fn immediately_zero(game: &CanonicalGame) -> bool {
    game.is_empty()
        || game
            .components()
            .iter()
            .all(|component| InvolutionSymmetryFinder.proves_zero(component))
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

struct CandidateSearch<'a> {
    maze: &'a Maze,
    corridor: &'a Corridor,
    cancel: &'a Arc<AtomicBool>,
    candidates: &'a mut HashMap<CanonicalGame, SolverCandidate>,
}

impl CandidateSearch<'_> {
    fn collect(&mut self, take: usize, start: usize, selected: &mut Vec<Cell>) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }

        if selected.len() == take {
            let movement = Move {
                axis: self.corridor.axis,
                anchor: self.corridor.anchor,
                cells: selected.clone(),
            };

            let mut next = self.maze.clone();
            next.apply_move(movement.axis, movement.anchor, &movement.cells)
                .expect("generated solver moves must be legal");
            let game = CanonicalGame::from_matrix(&compile_maze(&next));
            let candidate = SolverCandidate {
                removed: movement.cells.len(),
                nodes: game
                    .components()
                    .iter()
                    .map(|component| component.count_ones())
                    .sum(),
                movement,
                game: game.clone(),
            };
            self.candidates
                .entry(game)
                .and_modify(|old| {
                    if candidate.removed > old.removed {
                        *old = candidate.clone();
                    }
                })
                .or_insert(candidate);
            return;
        }

        let remaining = take - selected.len();
        let last_start = self.corridor.alive.len() - remaining;
        for index in start..=last_start {
            selected.push(self.corridor.alive[index]);
            self.collect(take, index + 1, selected);
            selected.pop();
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
        }
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

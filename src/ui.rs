use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    board::{Axis, Cell, Maze},
    evaluator::{DfsSolver, Evaluator, EvaluatorConfig, EvaluatorProgress},
    game::{Game, Move, PlayerKind, SolverMoveResult, solver_move_cancellable},
    solver::{PseudoCanonicalizer, compile_maze},
    successor::CanonicalMoveGenerator,
    symmetry::InvolutionSymmetryFinder,
};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy)]
enum MenuChoice {
    HumanVsHuman,
    HumanVsCpu,
    Editor,
}

struct Selection {
    cursor: Cell,
    axis: Axis,
    cells: Vec<Cell>,
}

impl Selection {
    fn new() -> Self {
        Self {
            cursor: Cell::new(0, 0),
            axis: Axis::Horizontal,
            cells: Vec::new(),
        }
    }

    fn corridor(&self, maze: &Maze) -> Vec<Cell> {
        maze.corridor(self.cursor, self.axis)
    }

    fn move_cursor(&mut self, maze: &Maze, row_delta: isize, col_delta: isize) {
        let old_corridor = self.corridor(maze);
        self.cursor.row = self
            .cursor
            .row
            .saturating_add_signed(row_delta)
            .min(maze.rows() - 1);
        self.cursor.col = self
            .cursor
            .col
            .saturating_add_signed(col_delta)
            .min(maze.cols() - 1);
        if self.corridor(maze) != old_corridor {
            self.cells.clear();
        }
    }

    fn toggle_axis(&mut self) {
        self.axis = self.axis.toggled();
        self.cells.clear();
    }

    fn toggle_current(&mut self, maze: &Maze) {
        if !maze.is_alive(self.cursor) {
            return;
        }
        if let Some(index) = self.cells.iter().position(|&cell| cell == self.cursor) {
            self.cells.swap_remove(index);
        } else {
            self.cells.push(self.cursor);
        }
    }

    fn alive_corridor(&self, maze: &Maze) -> Vec<Cell> {
        self.corridor(maze)
            .into_iter()
            .filter(|&cell| maze.is_alive(cell))
            .collect()
    }

    fn corridor_is_fully_selected(&self, maze: &Maze) -> bool {
        let alive = self.alive_corridor(maze);
        !alive.is_empty() && alive.iter().all(|cell| self.cells.contains(cell))
    }

    fn select_cell(&mut self, cell: Cell) {
        if !self.cells.contains(&cell) {
            self.cells.push(cell);
        }
    }

    fn deselect_cell(&mut self, cell: Cell) {
        if let Some(index) = self.cells.iter().position(|&selected| selected == cell) {
            self.cells.swap_remove(index);
        }
    }

    fn take_move(&mut self) -> Option<Move> {
        if self.cells.is_empty() {
            return None;
        }
        Some(Move {
            axis: self.axis,
            anchor: self.cursor,
            cells: std::mem::take(&mut self.cells),
        })
    }
}

struct LastSpace {
    at: Instant,
    corridor: Vec<Cell>,
    sweep_select: bool,
}

const DOUBLE_SPACE_WINDOW: Duration = Duration::from_millis(350);
const SWEEP_FRAME: Duration = Duration::from_millis(28);

pub fn run() -> io::Result<()> {
    let _terminal = TerminalGuard::enter()?;
    let mut stdout = io::stdout();
    let mut editor = Editor::new();

    loop {
        let Some(choice) = menu(&mut stdout)? else {
            return Ok(());
        };
        match choice {
            MenuChoice::HumanVsHuman => {
                let mut game = Game::human_vs_human_on(editor.maze.clone());
                match play_game(
                    &mut stdout,
                    &mut game,
                    Arc::clone(&editor.evaluator),
                    editor.solver_threads,
                )? {
                    PostGame::Menu => {}
                    PostGame::Quit => return Ok(()),
                }
            }
            MenuChoice::HumanVsCpu => {
                let mut game = Game::human_vs_cpu_on(editor.maze.clone());
                match play_game(
                    &mut stdout,
                    &mut game,
                    Arc::clone(&editor.evaluator),
                    editor.solver_threads,
                )? {
                    PostGame::Menu => {}
                    PostGame::Quit => return Ok(()),
                }
            }
            MenuChoice::Editor => match edit_board(&mut stdout, &mut editor)? {
                PostGame::Menu => continue,
                PostGame::Quit => return Ok(()),
            },
        }
    }
}

fn menu(stdout: &mut impl Write) -> io::Result<Option<MenuChoice>> {
    loop {
        queue!(
            stdout,
            MoveTo(0, 0),
            Clear(ClearType::All),
            SetForegroundColor(Color::Cyan),
            Print("NIM LIGHT\r\n\r\n"),
            ResetColor,
            Print("Take one or more nodes from one corridor. Last move wins.\r\n\r\n"),
            Print("  1  Human vs human\r\n"),
            Print("  2  Human vs solver CPU\r\n"),
            Print("  3  Level editor / solver\r\n\r\n"),
            Print("  q  Quit\r\n")
        )?;
        stdout.flush()?;

        if let Event::Key(key) = event::read()?
            && is_press(key)
        {
            match key.code {
                KeyCode::Char('1') => return Ok(Some(MenuChoice::HumanVsHuman)),
                KeyCode::Char('2') => return Ok(Some(MenuChoice::HumanVsCpu)),
                KeyCode::Char('3') => return Ok(Some(MenuChoice::Editor)),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditTarget {
    Node,
    VerticalWall,
    HorizontalWall,
}

impl EditTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::VerticalWall => "vertical wall",
            Self::HorizontalWall => "horizontal wall",
        }
    }
}

struct Editor {
    maze: Maze,
    row: usize,
    col: usize,
    target: EditTarget,
    evaluator: Arc<DfsSolver>,
    solver_threads: usize,
    last_evaluation: Option<EvaluationReport>,
    nimber_is_current: bool,
    last_cancelled: bool,
}

impl Editor {
    fn new() -> Self {
        Self {
            maze: Maze::demo(),
            row: 0,
            col: 0,
            target: EditTarget::Node,
            evaluator: new_evaluator(DEFAULT_SOLVER_THREADS),
            solver_threads: DEFAULT_SOLVER_THREADS,
            last_evaluation: None,
            nimber_is_current: false,
            last_cancelled: false,
        }
    }

    fn move_cursor(&mut self, row_delta: isize, col_delta: isize) {
        let (rows, cols) = self.target_dimensions();
        self.row = self.row.saturating_add_signed(row_delta).min(rows - 1);
        self.col = self.col.saturating_add_signed(col_delta).min(cols - 1);
    }

    fn cycle_target(&mut self) {
        self.target = match self.target {
            EditTarget::Node if self.maze.cols() > 1 => EditTarget::VerticalWall,
            EditTarget::Node if self.maze.rows() > 1 => EditTarget::HorizontalWall,
            EditTarget::Node => EditTarget::Node,
            EditTarget::VerticalWall if self.maze.rows() > 1 => EditTarget::HorizontalWall,
            EditTarget::VerticalWall => EditTarget::Node,
            EditTarget::HorizontalWall => EditTarget::Node,
        };
        self.clamp_cursor();
    }

    fn toggle_current(&mut self) {
        match self.target {
            EditTarget::Node => self.maze.toggle_alive(Cell::new(self.row, self.col)),
            EditTarget::VerticalWall => self.maze.toggle_vertical_wall(self.row, self.col),
            EditTarget::HorizontalWall => self.maze.toggle_horizontal_wall(self.row, self.col),
        }
        self.nimber_is_current = false;
        self.last_cancelled = false;
    }

    fn clear_cache(&mut self) {
        self.evaluator = new_evaluator(self.solver_threads);
        self.last_evaluation = None;
        self.nimber_is_current = false;
        self.last_cancelled = false;
    }

    fn adjust_solver_threads(&mut self, delta: isize) {
        let threads = self
            .solver_threads
            .saturating_add_signed(delta)
            .clamp(1, MAX_SOLVER_THREADS);
        if threads == self.solver_threads {
            return;
        }
        self.solver_threads = threads;
        self.evaluator = new_evaluator(threads);
        self.last_cancelled = false;
    }

    fn reset_demo(&mut self) {
        self.maze = Maze::demo();
        self.row = 0;
        self.col = 0;
        self.target = EditTarget::Node;
        self.nimber_is_current = false;
        self.last_cancelled = false;
    }

    fn reset_open(&mut self) {
        let rows = self.maze.rows();
        let cols = self.maze.cols();
        self.maze = Maze::open(rows, cols);
        self.row = 0;
        self.col = 0;
        self.target = EditTarget::Node;
        self.nimber_is_current = false;
        self.last_cancelled = false;
    }

    fn resize(&mut self, row_delta: isize, col_delta: isize) {
        let rows = self.maze.rows().saturating_add_signed(row_delta).max(1);
        let cols = self.maze.cols().saturating_add_signed(col_delta).max(1);
        self.maze = self.maze.resized(rows, cols);
        self.target = EditTarget::Node;
        self.clamp_cursor();
        self.nimber_is_current = false;
        self.last_cancelled = false;
    }

    fn clamp_cursor(&mut self) {
        let (rows, cols) = self.target_dimensions();
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
    }

    fn target_dimensions(&self) -> (usize, usize) {
        match self.target {
            EditTarget::Node => (self.maze.rows(), self.maze.cols()),
            EditTarget::VerticalWall => (self.maze.rows(), self.maze.cols() - 1),
            EditTarget::HorizontalWall => (self.maze.rows() - 1, self.maze.cols()),
        }
    }
}

const DEFAULT_SOLVER_THREADS: usize = 6;
const MAX_SOLVER_THREADS: usize = 64;

fn new_evaluator(threads: usize) -> Arc<DfsSolver> {
    Arc::new(
        Evaluator::with_config(
            CanonicalMoveGenerator::new(PseudoCanonicalizer),
            InvolutionSymmetryFinder,
            EvaluatorConfig {
                threads: Some(threads),
                ..EvaluatorConfig::default()
            },
        )
        .expect("failed to create evaluator worker pool"),
    )
}

#[derive(Clone, Copy, Debug)]
struct EvaluationReport {
    nimber: usize,
    elapsed: Duration,
    matrix_rows: usize,
    matrix_cols: usize,
    matrix_nodes: usize,
}

fn edit_board(stdout: &mut impl Write, editor: &mut Editor) -> io::Result<PostGame> {
    loop {
        render_editor(stdout, editor, None)?;

        if let Event::Key(key) = event::read()?
            && is_press(key)
        {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => editor.move_cursor(-1, 0),
                KeyCode::Down | KeyCode::Char('j') => editor.move_cursor(1, 0),
                KeyCode::Left | KeyCode::Char('h') => editor.move_cursor(0, -1),
                KeyCode::Right | KeyCode::Char('l') => editor.move_cursor(0, 1),
                KeyCode::Tab => editor.cycle_target(),
                KeyCode::Char(' ') => editor.toggle_current(),
                KeyCode::Char('n') => evaluate_editor(stdout, editor)?,
                KeyCode::Char('c') => editor.clear_cache(),
                KeyCode::Char('[') => editor.adjust_solver_threads(-1),
                KeyCode::Char(']') => editor.adjust_solver_threads(1),
                KeyCode::Char('r') => editor.reset_demo(),
                KeyCode::Char('o') => editor.reset_open(),
                KeyCode::Char('+') | KeyCode::Char('=') => editor.resize(1, 0),
                KeyCode::Char('-') => editor.resize(-1, 0),
                KeyCode::Char('>') | KeyCode::Char('.') => editor.resize(0, 1),
                KeyCode::Char('<') | KeyCode::Char(',') => editor.resize(0, -1),
                KeyCode::Char('m') | KeyCode::Esc => return Ok(PostGame::Menu),
                KeyCode::Char('q') => return Ok(PostGame::Quit),
                _ => {}
            }
        }
    }
}

fn evaluate_editor(stdout: &mut impl Write, editor: &mut Editor) -> io::Result<()> {
    let matrix = compile_maze(&editor.maze);
    let report_shape = (matrix.rows(), matrix.cols(), matrix.count_ones());
    let evaluator = Arc::clone(&editor.evaluator);
    let worker_evaluator = Arc::clone(&evaluator);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let started = Instant::now();

    let handle = thread::spawn(move || {
        let nimber = worker_evaluator.nimber_cancellable(&matrix, &worker_cancel);
        let _ = sender.send(nimber);
    });

    let nimber = loop {
        match receiver.try_recv() {
            Ok(nimber) => break nimber,
            Err(mpsc::TryRecvError::Disconnected) => {
                handle.join().expect("evaluator worker panicked");
                return Ok(());
            }
            Err(mpsc::TryRecvError::Empty) => {
                render_editor(
                    stdout,
                    editor,
                    Some((evaluator.progress(), started.elapsed())),
                )?;
                if event::poll(Duration::from_millis(100))?
                    && let Event::Key(key) = event::read()?
                    && is_press(key)
                    && matches!(key.code, KeyCode::Esc | KeyCode::Char('x'))
                {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    };

    handle.join().expect("evaluator worker panicked");
    if let Some(nimber) = nimber {
        editor.last_evaluation = Some(EvaluationReport {
            nimber,
            elapsed: started.elapsed(),
            matrix_rows: report_shape.0,
            matrix_cols: report_shape.1,
            matrix_nodes: report_shape.2,
        });
        editor.nimber_is_current = true;
        editor.last_cancelled = false;
    } else {
        editor.nimber_is_current = false;
        editor.last_cancelled = true;
    }
    Ok(())
}

enum PostGame {
    Menu,
    Quit,
}

fn play_game(
    stdout: &mut impl Write,
    game: &mut Game,
    solver: Arc<DfsSolver>,
    solver_threads: usize,
) -> io::Result<PostGame> {
    let mut selection = Selection::new();
    let mut last_space = None;
    let initial_maze = game.maze.clone();
    let shows_solver_panel = game
        .players
        .iter()
        .any(|player| player.kind == PlayerKind::SolverCpu);

    loop {
        let panel = shows_solver_panel.then(|| SolverPanel {
            status: "Solver cache",
            progress: solver.progress(),
            threads: solver_threads,
        });
        render(stdout, game, &selection, panel)?;

        if game.winner.is_some() {
            if let Event::Key(key) = event::read()?
                && is_press(key)
            {
                match key.code {
                    KeyCode::Char('r') => {
                        let cpu = game.players[1].kind == PlayerKind::SolverCpu;
                        *game = if cpu {
                            Game::human_vs_cpu_on(initial_maze.clone())
                        } else {
                            Game::human_vs_human_on(initial_maze.clone())
                        };
                        selection = Selection::new();
                        last_space = None;
                    }
                    KeyCode::Char('m') | KeyCode::Esc => return Ok(PostGame::Menu),
                    KeyCode::Char('q') => return Ok(PostGame::Quit),
                    _ => {}
                }
            }
            continue;
        }

        if game.current_player().kind == PlayerKind::SolverCpu {
            match run_cpu_turn(
                stdout,
                game,
                &selection,
                Arc::clone(&solver),
                solver_threads,
            )? {
                CpuTurn::Played => {}
                CpuTurn::Cancelled => return Ok(PostGame::Menu),
                CpuTurn::Quit => return Ok(PostGame::Quit),
            }
            continue;
        }

        if let Event::Key(key) = event::read()?
            && is_press(key)
        {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selection.move_cursor(&game.maze, -1, 0);
                    last_space = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selection.move_cursor(&game.maze, 1, 0);
                    last_space = None;
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    selection.move_cursor(&game.maze, 0, -1);
                    last_space = None;
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    selection.move_cursor(&game.maze, 0, 1);
                    last_space = None;
                }
                KeyCode::Tab => {
                    selection.toggle_axis();
                    last_space = None;
                }
                KeyCode::Char(' ') => {
                    let corridor = selection.corridor(&game.maze);
                    let is_double_space = last_space.as_ref().is_some_and(|last: &LastSpace| {
                        last.at.elapsed() <= DOUBLE_SPACE_WINDOW && last.corridor == corridor
                    });
                    if is_double_space {
                        let panel = shows_solver_panel.then(|| SolverPanel {
                            status: "Solver cache",
                            progress: solver.progress(),
                            threads: solver_threads,
                        });
                        let select = last_space
                            .as_ref()
                            .map(|last| last.sweep_select)
                            .unwrap_or_else(|| !selection.corridor_is_fully_selected(&game.maze));
                        animate_corridor_sweep(stdout, game, &mut selection, panel, select)?;
                        last_space = None;
                    } else {
                        let sweep_select = !selection.corridor_is_fully_selected(&game.maze);
                        selection.toggle_current(&game.maze);
                        last_space = Some(LastSpace {
                            at: Instant::now(),
                            corridor,
                            sweep_select,
                        });
                    }
                }
                KeyCode::Enter => {
                    if let Some(movement) = selection.take_move() {
                        game.play(movement)
                            .expect("the UI must only construct legal moves");
                    }
                    last_space = None;
                }
                KeyCode::Esc => {
                    selection.cells.clear();
                    last_space = None;
                }
                KeyCode::Char('m') => return Ok(PostGame::Menu),
                KeyCode::Char('q') => return Ok(PostGame::Quit),
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy)]
struct SolverPanel {
    status: &'static str,
    progress: EvaluatorProgress,
    threads: usize,
}

enum CpuTurn {
    Played,
    Cancelled,
    Quit,
}

fn run_cpu_turn(
    stdout: &mut impl Write,
    game: &mut Game,
    selection: &Selection,
    solver: Arc<DfsSolver>,
    solver_threads: usize,
) -> io::Result<CpuTurn> {
    let maze = game.maze.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker_solver = Arc::clone(&solver);
    let (sender, receiver) = mpsc::channel();
    let started = Instant::now();

    let handle = thread::spawn(move || {
        let result = solver_move_cancellable(&maze, &worker_solver, &worker_cancel);
        let _ = sender.send(result);
    });

    let result = loop {
        match receiver.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                handle.join().expect("CPU solver worker panicked");
                return Ok(CpuTurn::Cancelled);
            }
            Err(mpsc::TryRecvError::Empty) => {
                let status = if cancel.load(Ordering::Relaxed) {
                    "CPU cancelling..."
                } else {
                    "CPU thinking... Esc/x: cancel"
                };
                render(
                    stdout,
                    game,
                    selection,
                    Some(SolverPanel {
                        status,
                        progress: solver.progress(),
                        threads: solver_threads,
                    }),
                )?;
                queue!(
                    stdout,
                    Print(format!("CPU turn elapsed: {:.2?}\r\n", started.elapsed()))
                )?;
                stdout.flush()?;

                if event::poll(Duration::from_millis(100))?
                    && let Event::Key(key) = event::read()?
                    && is_press(key)
                {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('x') => {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        KeyCode::Char('q') => {
                            cancel.store(true, Ordering::Relaxed);
                            handle.join().expect("CPU solver worker panicked");
                            return Ok(CpuTurn::Quit);
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    handle.join().expect("CPU solver worker panicked");
    match result {
        SolverMoveResult::Move(movement) => {
            game.play(movement)
                .expect("the solver CPU must generate a legal move");
            Ok(CpuTurn::Played)
        }
        SolverMoveResult::NoMove => Ok(CpuTurn::Played),
        SolverMoveResult::Cancelled => Ok(CpuTurn::Cancelled),
    }
}

fn animate_corridor_sweep(
    stdout: &mut impl Write,
    game: &Game,
    selection: &mut Selection,
    solver_panel: Option<SolverPanel>,
    select: bool,
) -> io::Result<()> {
    let cells = selection.alive_corridor(&game.maze);
    if cells.is_empty() {
        return Ok(());
    }

    for cell in cells {
        if select {
            selection.select_cell(cell);
        } else {
            selection.deselect_cell(cell);
        }
        render(stdout, game, selection, solver_panel)?;
        thread::sleep(SWEEP_FRAME);
    }
    Ok(())
}

fn is_press(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn render(
    stdout: &mut impl Write,
    game: &Game,
    selection: &Selection,
    solver_panel: Option<SolverPanel>,
) -> io::Result<()> {
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

    if let Some(winner) = game.winner {
        queue!(
            stdout,
            SetForegroundColor(Color::Green),
            Print(format!("{} wins!\r\n", game.players[winner].name)),
            ResetColor,
            Print("r: rematch   m/Esc: menu   q: quit\r\n\r\n")
        )?;
    } else {
        queue!(
            stdout,
            Print("Turn: "),
            SetForegroundColor(if game.turn == 0 {
                Color::Cyan
            } else {
                Color::Magenta
            }),
            Print(&game.current_player().name),
            ResetColor,
            Print(format!(
                "    nodes: {}    corridor: {}\r\n",
                game.maze.alive_count(),
                selection.axis.name()
            )),
            Print("Move: arrows/hjkl  Axis: Tab  Select: Space  Sweep: Space×2  Take: Enter\r\n"),
            Print("Esc clears selection  ·  m menu  ·  q quit\r\n\r\n")
        )?;
    }

    render_maze(stdout, &game.maze, selection)?;

    if let Some((player, movement)) = &game.last_move {
        queue!(
            stdout,
            Print("\r\n"),
            SetForegroundColor(if *player == 0 {
                Color::Cyan
            } else {
                Color::Magenta
            }),
            Print(&game.players[*player].name),
            ResetColor,
            Print(format!(
                " took {} node{} from a {} corridor.",
                movement.cells.len(),
                if movement.cells.len() == 1 { "" } else { "s" },
                movement.axis.name()
            ))
        )?;
    }

    if let Some(panel) = solver_panel {
        queue!(
            stdout,
            Print("\r\n\r\n"),
            Print(panel.status),
            Print(format!("    threads: {}", panel.threads)),
            Print("\r\n")
        )?;
        render_progress(stdout, panel.progress)?;
    }

    stdout.flush()
}

fn render_editor(
    stdout: &mut impl Write,
    editor: &Editor,
    computing: Option<(EvaluatorProgress, Duration)>,
) -> io::Result<()> {
    queue!(
        stdout,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetForegroundColor(Color::Cyan),
        Print("NIM LIGHT — editor\r\n"),
        ResetColor,
        Print(format!(
            "{}×{}  target: {}  nodes: {}  threads: {}  cache: {}\r\n",
            editor.maze.rows(),
            editor.maze.cols(),
            editor.target.name(),
            editor.maze.alive_count(),
            editor.solver_threads,
            editor.evaluator.cache_len()
        )),
        Print("Move arrows/hjkl · Tab target · Space toggle · n nimber · c cache\r\n"),
        Print("+/- rows · </> cols · [/] threads · r demo · o open · m/Esc menu · q quit\r\n\r\n")
    )?;

    render_editor_maze(stdout, editor)?;
    queue!(stdout, Print("\r\n"))?;

    let progress = computing
        .map(|(progress, _)| progress)
        .unwrap_or_else(|| editor.evaluator.progress());

    if let Some((_, elapsed)) = computing {
        queue!(
            stdout,
            SetForegroundColor(Color::Yellow),
            Print("Computing nimber...\r\n"),
            ResetColor,
            Print(format!("elapsed: {:.2?}    Esc/x: cancel\r\n", elapsed))
        )?;
    } else if editor.last_cancelled {
        queue!(
            stdout,
            SetForegroundColor(Color::Yellow),
            Print("nimber: cancelled; completed cache entries were kept\r\n"),
            ResetColor
        )?;
    } else if let Some(report) = editor.last_evaluation {
        if editor.nimber_is_current {
            queue!(
                stdout,
                SetForegroundColor(if report.nimber == 0 {
                    Color::Green
                } else {
                    Color::Magenta
                }),
                Print(format!("nimber: {}\r\n", report.nimber)),
                ResetColor
            )?;
        } else {
            queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(format!(
                    "nimber: stale after edits; previous value was {}\r\n",
                    report.nimber
                )),
                ResetColor
            )?;
        }
        queue!(
            stdout,
            Print(format!(
                "previous compiled matrix: {} × {} with {} nodes\r\n",
                report.matrix_rows, report.matrix_cols, report.matrix_nodes
            )),
            Print(format!("previous elapsed: {:.2?}\r\n", report.elapsed))
        )?;
    } else {
        queue!(
            stdout,
            Print("nimber: not computed for this edit state\r\n")
        )?;
    }
    render_progress(stdout, progress)?;

    stdout.flush()
}

fn render_progress(stdout: &mut impl Write, progress: EvaluatorProgress) -> io::Result<()> {
    let stats = progress.stats;
    queue!(
        stdout,
        Print(format!(
            "evals {}  unique {}  done {}  hits {}  cache {}+{} (~{})\r\n",
            stats.evaluation_attempts,
            stats.unique_positions_claimed,
            stats.completed_positions,
            stats.completed_cache_hits,
            progress.cache_done_entries,
            progress.cache_processing_entries,
            format_bytes(progress.estimated_cache_bytes)
        )),
        Print(format!(
            "busy {}  deferred {}  forced {}  symmetry {}  parallel {}\r\n",
            stats.processing_hits,
            stats.deferred_resolved,
            stats.forced_duplicate_evaluations,
            stats.symmetry_zero_certificates,
            stats.parallel_expansions
        )),
        Print(format!(
            "{:.0} eval/s  {:.0} unique/s  {:.0} hit/s  uptime {:.2?}\r\n",
            progress.evaluations_per_second,
            progress.unique_positions_per_second,
            progress.cache_hits_per_second,
            progress.elapsed
        ))
    )
}

fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as usize)
    }
}

fn render_editor_maze(stdout: &mut impl Write, editor: &Editor) -> io::Result<()> {
    render_editor_horizontal_boundary(stdout, editor, None)?;
    for row in 0..editor.maze.rows() {
        queue!(stdout, Print("│"))?;
        for col in 0..editor.maze.cols() {
            let node_cursor =
                editor.target == EditTarget::Node && editor.row == row && editor.col == col;
            let cell = Cell::new(row, col);

            queue!(stdout, Print(" "))?;
            if node_cursor {
                queue!(
                    stdout,
                    SetForegroundColor(Color::White),
                    Print(if editor.maze.is_alive(cell) {
                        "◉"
                    } else {
                        "⊗"
                    }),
                    ResetColor
                )?;
            } else if editor.maze.is_alive(cell) {
                queue!(
                    stdout,
                    SetForegroundColor(Color::Green),
                    Print("•"),
                    ResetColor
                )?;
            } else {
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print("×"),
                    ResetColor
                )?;
            }
            queue!(stdout, Print(" "))?;

            let wall_cursor = editor.target == EditTarget::VerticalWall
                && editor.row == row
                && editor.col == col
                && col + 1 < editor.maze.cols();
            let wall = col + 1 == editor.maze.cols() || !editor.maze.connected_right(row, col);
            if wall_cursor {
                queue!(
                    stdout,
                    SetForegroundColor(Color::Yellow),
                    Print(if wall { "┃" } else { "╎" }),
                    ResetColor
                )?;
            } else if wall {
                queue!(stdout, Print("│"))?;
            } else {
                queue!(stdout, Print(" "))?;
            }
        }
        queue!(stdout, Print("\r\n"))?;

        if row + 1 < editor.maze.rows() {
            render_editor_horizontal_boundary(stdout, editor, Some(row))?;
        }
    }
    render_editor_horizontal_boundary(stdout, editor, Some(editor.maze.rows() - 1))
}

fn render_editor_horizontal_boundary(
    stdout: &mut impl Write,
    editor: &Editor,
    above: Option<usize>,
) -> io::Result<()> {
    queue!(stdout, Print("+"))?;
    for col in 0..editor.maze.cols() {
        let wall = match above {
            None => true,
            Some(row) if row + 1 == editor.maze.rows() => true,
            Some(row) => !editor.maze.connected_down(row, col),
        };
        let wall_cursor = matches!(above, Some(row) if row + 1 < editor.maze.rows()
            && editor.target == EditTarget::HorizontalWall
            && editor.row == row
            && editor.col == col);

        if wall_cursor {
            queue!(
                stdout,
                SetForegroundColor(Color::Yellow),
                Print(if wall { "━━━" } else { "┄┄┄" }),
                ResetColor,
                Print("+")
            )?;
        } else {
            queue!(stdout, Print(if wall { "───" } else { "   " }), Print("+"))?;
        }
    }
    queue!(stdout, Print("\r\n"))
}

fn render_maze(stdout: &mut impl Write, maze: &Maze, selection: &Selection) -> io::Result<()> {
    let active_corridor = selection.corridor(maze);

    render_horizontal_boundary(stdout, maze, None)?;
    for row in 0..maze.rows() {
        queue!(stdout, Print("│"))?;
        for col in 0..maze.cols() {
            let cell = Cell::new(row, col);
            let selected = selection.cells.contains(&cell);
            let cursor = selection.cursor == cell;
            let active = active_corridor.contains(&cell);

            queue!(stdout, Print(" "))?;
            if selected {
                queue!(
                    stdout,
                    SetForegroundColor(Color::Yellow),
                    Print("◆"),
                    ResetColor
                )?;
            } else if cursor {
                queue!(
                    stdout,
                    SetForegroundColor(Color::White),
                    Print(if maze.is_alive(cell) { "◉" } else { "⊗" }),
                    ResetColor
                )?;
            } else if maze.is_alive(cell) {
                queue!(
                    stdout,
                    SetForegroundColor(if active {
                        Color::DarkCyan
                    } else {
                        Color::Green
                    }),
                    Print(if active { "●" } else { "•" }),
                    ResetColor
                )?;
            } else {
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print("×"),
                    ResetColor
                )?;
            }
            queue!(stdout, Print(" "))?;

            if col + 1 == maze.cols() || !maze.connected_right(row, col) {
                queue!(stdout, Print("│"))?;
            } else {
                queue!(stdout, Print(" "))?;
            }
        }
        queue!(stdout, Print("\r\n"))?;

        if row + 1 < maze.rows() {
            render_horizontal_boundary(stdout, maze, Some(row))?;
        }
    }
    render_horizontal_boundary(stdout, maze, Some(maze.rows() - 1))
}

/// `above = None` is the outer top border; the final row is the outer bottom
/// border. Other values describe the boundary below that logical row.
fn render_horizontal_boundary(
    stdout: &mut impl Write,
    maze: &Maze,
    above: Option<usize>,
) -> io::Result<()> {
    queue!(stdout, Print("+"))?;
    for col in 0..maze.cols() {
        let wall = match above {
            None => true,
            Some(row) if row + 1 == maze.rows() => true,
            Some(row) => !maze.connected_down(row, col),
        };
        queue!(stdout, Print(if wall { "───" } else { "   " }), Print("+"))?;
    }
    queue!(stdout, Print("\r\n"))
}

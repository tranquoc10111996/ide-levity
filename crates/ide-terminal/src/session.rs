//! PTY-backed terminal sessions.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use parking_lot::{Mutex, RwLock};

use crate::keys::{encode_key, KeyInput, TermKey, TermModeFlags};
use crate::palette::{ColorTable, TerminalPalette};
use crate::snapshot::{build_snapshot, mode_flags, TerminalSnapshot};

/// Minimum grid width accepted by [`TerminalSession::resize`].
const MIN_COLS: u16 = 2;
/// Minimum grid height accepted by [`TerminalSession::resize`].
const MIN_ROWS: u16 = 1;
/// How long [`TerminalSession::shutdown`] waits after SIGHUP before escalating to SIGKILL.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1000);

/// Source of unique ids handed to alacritty's PTY layer (exported as `WINDOWID`).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Errors produced when starting a terminal session.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The PTY could not be created or the shell could not be started.
    #[error("failed to spawn shell `{program}`: {source}")]
    Spawn {
        /// Program that was being started.
        program: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The I/O event loop or a helper thread could not be created.
    #[error("failed to start terminal I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for this crate.
pub type Result<T, E = TerminalError> = std::result::Result<T, E>;

/// How to start a terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Program to run. `None` uses the user's shell (see [`default_shell`]).
    pub shell: Option<String>,
    /// Arguments for the program. When `shell` is `None` and this is empty, platform defaults
    /// are used (`-l` on macOS so the login profile is read, like VS Code; none elsewhere).
    pub args: Vec<String>,
    /// Working directory; `None` inherits the IDE's current directory.
    pub working_dir: Option<PathBuf>,
    /// Extra environment variables (override the defaults below).
    ///
    /// `TERM=xterm-256color`, `COLORTERM=truecolor` and `TERM_PROGRAM=ide-levity` are always
    /// set unless overridden here.
    pub env: HashMap<String, String>,
    /// Initial number of columns.
    pub cols: u16,
    /// Initial number of rows.
    pub rows: u16,
    /// Maximum number of scrollback lines kept in memory.
    pub scrollback_lines: usize,
    /// Colors used for snapshots; can be changed later with [`TerminalSession::set_palette`].
    pub palette: TerminalPalette,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            working_dir: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            palette: TerminalPalette::default(),
        }
    }
}

/// Asynchronous notifications from a terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The terminal content changed; take a new [`TerminalSession::snapshot`] and redraw.
    ///
    /// Wakeups are coalesced: after one is sent, no further wakeup is sent until
    /// [`TerminalSession::snapshot`] has been called.
    Wakeup,
    /// The title changed (OSC 0/2), or was reset to the shell name.
    Title(String),
    /// The application rang the bell (BEL).
    Bell,
    /// The shell process exited. Carries the exit code when the process exited normally,
    /// `None` when it was killed by a signal or the session was shut down.
    Exited(Option<i32>),
    /// The application asked to copy text to the system clipboard (OSC 52).
    ClipboardStore(String),
}

/// Kind of text selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionKind {
    /// Character-wise selection (single click + drag).
    #[default]
    Simple,
    /// Rectangular selection (Alt + drag).
    Block,
    /// Word selection that expands to semantic boundaries (double click).
    Semantic,
    /// Whole-line selection (triple click).
    Lines,
}

impl From<SelectionKind> for SelectionType {
    fn from(kind: SelectionKind) -> Self {
        match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Block => SelectionType::Block,
            SelectionKind::Semantic => SelectionType::Semantic,
            SelectionKind::Lines => SelectionType::Lines,
        }
    }
}

/// Which half of a cell the pointer is over; determines whether that cell is included at a
/// selection boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellSide {
    /// Left half.
    #[default]
    Left,
    /// Right half.
    Right,
}

impl From<CellSide> for Side {
    fn from(side: CellSide) -> Self {
        match side {
            CellSide::Left => Side::Left,
            CellSide::Right => Side::Right,
        }
    }
}

/// A cell position in the visible viewport (row 0 is the top visible row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ViewportPoint {
    /// Viewport row.
    pub row: usize,
    /// Column.
    pub col: usize,
}

impl ViewportPoint {
    /// Creates a point.
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Returns the shell used when [`TerminalOptions::shell`] is `None`.
///
/// Unix: `$SHELL` if it points to an existing file, else `/bin/bash`, else `/bin/sh`.
/// Windows: `%COMSPEC%`, else `powershell.exe`.
pub fn default_shell() -> String {
    #[cfg(unix)]
    {
        if let Ok(shell) = std::env::var("SHELL") {
            if !shell.is_empty() && std::path::Path::new(&shell).is_file() {
                return shell;
            }
        }
        if std::path::Path::new("/bin/bash").is_file() {
            return "/bin/bash".to_owned();
        }
        "/bin/sh".to_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "powershell.exe".to_owned())
    }
}

/// Grid dimensions passed to alacritty.
#[derive(Debug, Clone, Copy)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Messages delivered to the dispatcher thread.
enum Internal {
    /// An event raised by the terminal emulator.
    Term(Event),
    /// The PTY event loop terminated and the PTY was closed.
    LoopEnded,
}

/// Forwards alacritty events to the dispatcher thread.
///
/// `send_event` is invoked while the terminal lock is held, so it must never block or lock.
#[derive(Clone)]
struct Listener(Sender<Internal>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(Internal::Term(event));
    }
}

/// State shared between the session handle and its helper threads.
struct Shared {
    title: Mutex<String>,
    default_title: String,
    palette: RwLock<TerminalPalette>,
    /// `Some(code)` once the loop ended.
    exit: Mutex<Option<Option<i32>>>,
    wakeup_pending: AtomicBool,
    window: Mutex<WindowSize>,
}

/// Kind of the active selection (the selection itself lives in the terminal so it follows
/// scrolling output).
#[derive(Debug, Clone, Copy)]
struct SelectionAnchor {
    kind: SelectionType,
}

/// A running shell attached to a pseudo terminal.
///
/// All methods are cheap and non-blocking (they lock the terminal state briefly) except
/// [`TerminalSession::shutdown`]. The session is `Send + Sync`, so it can live in the GUI
/// state or behind an `Arc`.
///
/// Threads: alacritty's PTY event loop (reads/parses output, writes input), a watcher that
/// closes the PTY when the loop ends, and a dispatcher that answers terminal queries (DSR,
/// DA, OSC color queries) and forwards [`TerminalEvent`]s.
pub struct TerminalSession {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: Mutex<EventLoopSender>,
    shared: Arc<Shared>,
    events: Mutex<Option<Receiver<TerminalEvent>>>,
    selection: Mutex<Option<SelectionAnchor>>,
    pid: Option<u32>,
    shutdown: Mutex<Option<ShutdownJob>>,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("pid", &self.pid)
            .field("title", &*self.shared.title.lock())
            .field("exit", &*self.shared.exit.lock())
            .finish_non_exhaustive()
    }
}

impl TerminalSession {
    /// Starts `options.shell` (or the user's shell) on a new PTY.
    ///
    /// Returns once the process is spawned; output arrives asynchronously and is announced
    /// with [`TerminalEvent::Wakeup`].
    pub fn spawn(options: TerminalOptions) -> Result<Self> {
        let (program, args) = match options.shell {
            Some(shell) => (shell, options.args),
            None => {
                let shell = default_shell();
                let args = if options.args.is_empty() && cfg!(target_os = "macos") {
                    vec!["-l".to_owned()]
                } else {
                    options.args
                };
                (shell, args)
            }
        };
        let default_title = std::path::Path::new(&program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone());

        let mut env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "ide-levity".to_owned()),
        ]);
        env.extend(options.env);

        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(program.clone(), args)),
            working_directory: options.working_dir,
            // Keep reading after the child exits so its final output is not lost; the watcher
            // thread reports the exit once the loop ends.
            hold: true,
            env,
        };
        let cols = options.cols.max(MIN_COLS);
        let rows = options.rows.max(MIN_ROWS);
        let window = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 0,
            cell_height: 0,
        };

        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let pty = tty::new(&pty_options, window, id).map_err(|source| TerminalError::Spawn {
            program: program.clone(),
            source,
        })?;

        #[cfg(unix)]
        let pid = Some(pty.child().id());
        #[cfg(windows)]
        let pid = pty.child_watcher().pid().map(|p| p.get());
        #[cfg(unix)]
        let master = pty.file().try_clone().ok();

        let (internal_tx, internal_rx) = mpsc::channel();
        let listener = Listener(internal_tx.clone());
        let config = Config {
            scrolling_history: options.scrollback_lines,
            ..Config::default()
        };
        let size = GridSize {
            cols: usize::from(cols),
            rows: usize::from(rows),
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));

        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let sender = event_loop.channel();

        let shared = Arc::new(Shared {
            title: Mutex::new(default_title.clone()),
            default_title,
            palette: RwLock::new(options.palette),
            exit: Mutex::new(None),
            wakeup_pending: AtomicBool::new(false),
            window: Mutex::new(window),
        });

        let (events_tx, events_rx) = mpsc::channel();
        let dispatcher = {
            let term = term.clone();
            let sender = sender.clone();
            let shared = shared.clone();
            thread::Builder::new()
                .name("ide-terminal-dispatch".to_owned())
                .spawn(move || dispatch(internal_rx, events_tx, term, sender, shared))?
        };

        let loop_handle = event_loop.spawn();
        let watcher = thread::Builder::new()
            .name("ide-terminal-watch".to_owned())
            .spawn(move || {
                // Dropping the returned event loop drops the PTY, which hangs up and reaps the child.
                if let Ok((event_loop, _state)) = loop_handle.join() {
                    drop(event_loop);
                }
                let _ = internal_tx.send(Internal::LoopEnded);
            })?;

        Ok(Self {
            term,
            sender: Mutex::new(sender.clone()),
            shared,
            events: Mutex::new(Some(events_rx)),
            selection: Mutex::new(None),
            pid,
            shutdown: Mutex::new(Some(ShutdownJob {
                sender,
                watcher: Some(watcher),
                dispatcher: Some(dispatcher),
                pid,
                #[cfg(unix)]
                master,
            })),
        })
    }

    /// Process id of the shell, when known.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether the shell has exited (or the session was shut down).
    pub fn has_exited(&self) -> bool {
        self.shared.exit.lock().is_some()
    }

    /// Exit code of the shell, if it exited normally.
    pub fn exit_code(&self) -> Option<i32> {
        self.shared.exit.lock().flatten()
    }

    /// Takes the event receiver so it can be moved to another thread (e.g. a forwarding
    /// thread feeding an `iced` subscription). Returns `None` if already taken.
    pub fn take_events(&mut self) -> Option<Receiver<TerminalEvent>> {
        self.events.lock().take()
    }

    /// Drains pending events without blocking. Empty once [`Self::take_events`] was called.
    pub fn poll_events(&self) -> Vec<TerminalEvent> {
        self.events
            .lock()
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    /// Sends raw bytes to the shell. Scrolls the viewport back to the bottom like a real
    /// terminal does on input.
    pub fn write_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
        }
        let _ = self
            .sender
            .lock()
            .send(Msg::Input(Cow::Owned(bytes.to_vec())));
    }

    /// Sends UTF-8 text to the shell as if typed.
    pub fn write_str(&self, text: &str) {
        self.write_bytes(text.as_bytes());
    }

    /// Encodes and sends a key press using the current terminal modes. Returns `false` when
    /// the key produces no input.
    pub fn send_key(&self, input: KeyInput) -> bool {
        match encode_key(input, self.mode()) {
            Some(bytes) => {
                self.write_bytes(&bytes);
                true
            }
            None => false,
        }
    }

    /// Pastes text. Uses bracketed paste when the application enabled it (escape characters
    /// are stripped so pasted text cannot terminate the bracket); otherwise newlines are sent
    /// as carriage returns.
    pub fn paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.term.lock().mode().contains(TermMode::BRACKETED_PASTE) {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend(text.bytes().filter(|&b| b != 0x1b));
            bytes.extend_from_slice(b"\x1b[201~");
            self.write_bytes(&bytes);
        } else {
            self.write_str(&text.replace("\r\n", "\r").replace('\n', "\r"));
        }
    }

    /// Resizes the grid and the PTY (`TIOCSWINSZ`, which sends `SIGWINCH` to the foreground
    /// job). Cell sizes in pixels are reported to applications that ask for them; pass 0 if
    /// unknown.
    pub fn resize(&self, cols: u16, rows: u16, cell_width_px: u16, cell_height_px: u16) {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        let window = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: cell_width_px,
            cell_height: cell_height_px,
        };
        {
            let mut current = self.shared.window.lock();
            if current.num_cols == cols
                && current.num_lines == rows
                && current.cell_width == cell_width_px
                && current.cell_height == cell_height_px
            {
                return;
            }
            *current = window;
        }
        self.term.lock().resize(GridSize {
            cols: usize::from(cols),
            rows: usize::from(rows),
        });
        let _ = self.sender.lock().send(Msg::Resize(window));
    }

    /// Scrolls the viewport through the scrollback. Positive values scroll up (towards older
    /// output), negative values scroll down.
    pub fn scroll(&self, delta_lines: i32) {
        if delta_lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(delta_lines));
        }
    }

    /// Scrolls the viewport by one page. Positive pages scroll up.
    pub fn scroll_pages(&self, pages: i32) {
        let mut term = self.term.lock();
        let scroll = if pages > 0 {
            Scroll::PageUp
        } else {
            Scroll::PageDown
        };
        for _ in 0..pages.unsigned_abs() {
            term.scroll_display(scroll);
        }
    }

    /// Jumps back to the live output.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// Jumps to the oldest scrollback line.
    pub fn scroll_to_top(&self) {
        self.term.lock().scroll_display(Scroll::Top);
    }

    /// Handles a mouse-wheel movement over the terminal, like VS Code / xterm do:
    /// reported to the application when it enabled mouse reporting, translated to arrow keys
    /// in the alternate screen (e.g. `less`, `man`), and otherwise scrolls the scrollback.
    /// Positive `lines` means wheel up.
    pub fn wheel(&self, lines: i32, at: ViewportPoint) {
        if lines == 0 {
            return;
        }
        let mode = self.mode();
        let count = lines.unsigned_abs() as usize;
        if mode.mouse_reporting {
            let button = if lines > 0 { 64 } else { 65 };
            let (col, row) = (at.col + 1, at.row + 1);
            let report = if mode.sgr_mouse {
                format!("\x1b[<{button};{col};{row}M").into_bytes()
            } else if col + 32 <= 255 && row + 32 <= 255 {
                vec![
                    0x1b,
                    b'[',
                    b'M',
                    32 + button,
                    (32 + col) as u8,
                    (32 + row) as u8,
                ]
            } else {
                return;
            };
            let _ = self
                .sender
                .lock()
                .send(Msg::Input(Cow::Owned(report.repeat(count))));
        } else if mode.alt_screen && mode.alternate_scroll {
            let key = KeyInput::plain(if lines > 0 {
                TermKey::Up
            } else {
                TermKey::Down
            });
            if let Some(bytes) = encode_key(key, mode) {
                let _ = self
                    .sender
                    .lock()
                    .send(Msg::Input(Cow::Owned(bytes.repeat(count))));
            }
        } else {
            self.scroll(lines);
        }
    }

    /// Informs the terminal about focus changes; sends focus reports when requested.
    pub fn set_focused(&self, focused: bool) {
        let report = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            term.mode().contains(TermMode::FOCUS_IN_OUT)
        };
        if report {
            let seq: &'static [u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
            let _ = self.sender.lock().send(Msg::Input(Cow::Borrowed(seq)));
        }
    }

    /// Current terminal modes.
    pub fn mode(&self) -> TermModeFlags {
        mode_flags(*self.term.lock().mode())
    }

    /// Current title.
    pub fn title(&self) -> String {
        self.shared.title.lock().clone()
    }

    /// Replaces the color palette used by future snapshots.
    pub fn set_palette(&self, palette: TerminalPalette) {
        *self.shared.palette.write() = palette;
    }

    /// Captures the visible grid with fully resolved colors. Cheap enough to call every frame
    /// (a 120x40 grid takes well under a millisecond). Also re-arms [`TerminalEvent::Wakeup`].
    pub fn snapshot(&self) -> TerminalSnapshot {
        self.shared.wakeup_pending.store(false, Ordering::Release);
        let title = self.shared.title.lock().clone();
        let palette = self.shared.palette.read().clone();
        let term = self.term.lock();
        build_snapshot(&term, &palette, title)
    }

    /// Starts a selection at `point` (mouse down). `side` is the half of the cell under the
    /// pointer.
    pub fn start_selection(&self, point: ViewportPoint, side: CellSide, kind: SelectionKind) {
        let mut term = self.term.lock();
        let grid_point = viewport_to_grid(&term, point);
        let kind = SelectionType::from(kind);
        term.selection = Some(Selection::new(kind, grid_point, side.into()));
        *self.selection.lock() = Some(SelectionAnchor { kind });
    }

    /// Extends the current selection to `point` (mouse drag). No-op without a selection.
    pub fn update_selection(&self, point: ViewportPoint, side: CellSide) {
        let mut term = self.term.lock();
        let grid_point = viewport_to_grid(&term, point);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(grid_point, side.into());
        }
    }

    /// Selects the inclusive range between two viewport cells.
    pub fn select(&self, start: ViewportPoint, end: ViewportPoint, kind: SelectionKind) {
        let (first, last) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.start_selection(first, CellSide::Left, kind);
        self.update_selection(last, CellSide::Right);
    }

    /// Selects the whole scrollback and screen.
    pub fn select_all(&self) {
        let mut term = self.term.lock();
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        *self.selection.lock() = Some(SelectionAnchor {
            kind: SelectionType::Simple,
        });
    }

    /// Removes the selection.
    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
        *self.selection.lock() = None;
    }

    /// Whether a non-empty selection exists.
    pub fn has_selection(&self) -> bool {
        self.term
            .lock()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// Kind of the current selection, if any.
    pub fn selection_kind(&self) -> Option<SelectionKind> {
        self.selection.lock().map(|anchor| match anchor.kind {
            SelectionType::Simple => SelectionKind::Simple,
            SelectionType::Block => SelectionKind::Block,
            SelectionType::Semantic => SelectionKind::Semantic,
            SelectionType::Lines => SelectionKind::Lines,
        })
    }

    /// Text of the current selection (for copy), if any.
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    /// Stops the shell and releases the PTY.
    ///
    /// Sends SIGHUP to the shell's process group and the foreground job, waits up to one
    /// second, then escalates to SIGKILL, and finally reaps the child so no zombie remains.
    /// Blocks until done (normally a few milliseconds). Idempotent; also performed (on a
    /// background thread) when the session is dropped.
    pub fn shutdown(&mut self) {
        if let Some(job) = self.shutdown.lock().take() {
            job.run();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Some(job) = self.shutdown.lock().take() {
            // Never block the (GUI) thread dropping the session; fall back to running inline
            // only if no thread can be created.
            let slot = Arc::new(Mutex::new(Some(job)));
            let thread_slot = slot.clone();
            let spawned = thread::Builder::new()
                .name("ide-terminal-reaper".to_owned())
                .spawn(move || {
                    if let Some(job) = thread_slot.lock().take() {
                        job.run();
                    }
                });
            if spawned.is_err() {
                if let Some(job) = slot.lock().take() {
                    job.run();
                }
            }
        }
    }
}

/// Converts a viewport cell to a grid point, clamped to the grid.
fn viewport_to_grid<T>(term: &Term<T>, point: ViewportPoint) -> Point {
    let rows = term.screen_lines();
    let cols = term.columns();
    let row = point.row.min(rows.saturating_sub(1)) as i32;
    let col = point.col.min(cols.saturating_sub(1));
    Point::new(Line(row - term.grid().display_offset() as i32), Column(col))
}

/// Everything needed to stop a session, detached from the handle so it can run on another
/// thread.
struct ShutdownJob {
    sender: EventLoopSender,
    watcher: Option<JoinHandle<()>>,
    dispatcher: Option<JoinHandle<()>>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: Option<u32>,
    #[cfg(unix)]
    master: Option<std::fs::File>,
}

impl ShutdownJob {
    fn run(mut self) {
        let _ = self.sender.send(Msg::Shutdown);
        let loop_running = self.watcher.as_ref().is_some_and(|w| !w.is_finished());

        #[cfg(unix)]
        if loop_running {
            self.signal_children(rustix::process::Signal::Hup);
        }

        if loop_running {
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while self.watcher.as_ref().is_some_and(|w| !w.is_finished())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
            #[cfg(unix)]
            if self.watcher.as_ref().is_some_and(|w| !w.is_finished()) {
                self.signal_children(rustix::process::Signal::Kill);
            }
        }

        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        if let Some(dispatcher) = self.dispatcher.take() {
            let _ = dispatcher.join();
        }
    }

    /// Signals the foreground job and the shell's process group (the shell is a session
    /// leader, so its process group id equals its pid).
    #[cfg(unix)]
    fn signal_children(&self, signal: rustix::process::Signal) {
        use rustix::process::{kill_process, kill_process_group, Pid};

        let Some(shell) = self.pid.and_then(|p| Pid::from_raw(p as i32)) else {
            return;
        };
        if let Some(master) = &self.master {
            if let Ok(foreground) = rustix::termios::tcgetpgrp(master) {
                if foreground != shell {
                    let _ = kill_process_group(foreground, signal);
                }
            }
        }
        let _ = kill_process_group(shell, signal);
        let _ = kill_process(shell, signal);
    }
}

/// Dispatcher thread: answers terminal queries and forwards public events.
fn dispatch(
    rx: Receiver<Internal>,
    events: Sender<TerminalEvent>,
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    shared: Arc<Shared>,
) {
    let write = |text: String| {
        if !text.is_empty() {
            let _ = sender.send(Msg::Input(Cow::Owned(text.into_bytes())));
        }
    };
    let mut child_code = None;

    while let Ok(message) = rx.recv() {
        let event = match message {
            Internal::LoopEnded => {
                *shared.exit.lock() = Some(child_code);
                shared.wakeup_pending.store(true, Ordering::Release);
                let _ = events.send(TerminalEvent::Exited(child_code));
                let _ = events.send(TerminalEvent::Wakeup);
                break;
            }
            Internal::Term(event) => event,
        };
        match event {
            Event::Wakeup => {
                if !shared.wakeup_pending.swap(true, Ordering::AcqRel) {
                    let _ = events.send(TerminalEvent::Wakeup);
                }
            }
            Event::Title(title) => {
                shared.title.lock().clone_from(&title);
                let _ = events.send(TerminalEvent::Title(title));
            }
            Event::ResetTitle => {
                let title = shared.default_title.clone();
                shared.title.lock().clone_from(&title);
                let _ = events.send(TerminalEvent::Title(title));
            }
            Event::Bell => {
                let _ = events.send(TerminalEvent::Bell);
            }
            Event::ClipboardStore(_, text) => {
                let _ = events.send(TerminalEvent::ClipboardStore(text));
            }
            Event::PtyWrite(text) => write(text),
            Event::ColorRequest(index, format) => {
                let palette = shared.palette.read().clone();
                let color = ColorTable::new(&palette, term.lock().colors()).get(index);
                write(format(color.into()));
            }
            Event::TextAreaSizeRequest(format) => {
                let window = *shared.window.lock();
                write(format(window));
            }
            Event::ChildExit(code) => child_code = Some(code),
            // OSC 52 paste is disabled (`Osc52::OnlyCopy`), so clipboard loads never arrive;
            // the remaining events only matter to Alacritty's own windowing.
            Event::ClipboardLoad(..)
            | Event::MouseCursorDirty
            | Event::CursorBlinkingChange
            | Event::Exit => {}
        }
    }
}

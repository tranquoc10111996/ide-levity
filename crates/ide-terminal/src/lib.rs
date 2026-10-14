//! Real integrated terminal for IDE Levity.
//!
//! [`TerminalSession`] runs the user's shell on a pseudo terminal (Unix PTY, Windows ConPTY)
//! using `alacritty_terminal` for PTY I/O and VT emulation. The GUI interacts with it through
//! a small, toolkit-independent API:
//!
//! - input: [`TerminalSession::write_str`], [`TerminalSession::send_key`] /
//!   [`encode_key`], [`TerminalSession::paste`];
//! - output: [`TerminalSession::snapshot`] returns a [`TerminalSnapshot`] with fully
//!   resolved RGB colors, ready to paint;
//! - notifications: a channel of [`TerminalEvent`]s (redraw, title, bell, exit, clipboard).
//!
//! # Integration
//!
//! ```no_run
//! use ide_terminal::{TerminalEvent, TerminalOptions, TerminalSession};
//!
//! let mut session = TerminalSession::spawn(TerminalOptions::default())?;
//! let events = session.take_events().expect("receiver");
//! // Forward events to the GUI from a helper thread (blocking recv is fine there).
//! std::thread::spawn(move || {
//!     for event in events {
//!         if let TerminalEvent::Exited(code) = event {
//!             println!("shell exited with {code:?}");
//!         }
//!     }
//! });
//! session.resize(120, 40, 8, 16);
//! session.write_str("echo hello\r");
//! let frame = session.snapshot();
//! assert_eq!(frame.cols, 120);
//! # Ok::<(), ide_terminal::TerminalError>(())
//! ```

mod keys;
mod palette;
mod session;
mod snapshot;

pub use keys::{encode_key, KeyInput, TermKey, TermModeFlags};
pub use palette::{Rgb, TerminalPalette};
pub use session::{
    default_shell, CellSide, Result, SelectionKind, TerminalError, TerminalEvent, TerminalOptions,
    TerminalSession, ViewportPoint,
};
pub use snapshot::{CellView, CursorShape, CursorView, TerminalSnapshot};

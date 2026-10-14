//! Render-ready snapshots of the terminal grid.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::CursorShape as VteCursorShape;

use crate::keys::TermModeFlags;
use crate::palette::{ColorTable, Rgb, TerminalPalette};

/// Shape of the terminal cursor as requested by the application (DECSCUSR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    /// Filled block.
    Block,
    /// Underscore.
    Underline,
    /// Vertical bar.
    Beam,
    /// Outlined block (typically used when the terminal is unfocused).
    HollowBlock,
}

/// Cursor position within the visible viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorView {
    /// Viewport row (0 = top visible row).
    pub row: usize,
    /// Column.
    pub col: usize,
    /// Shape.
    pub shape: CursorShape,
}

/// A single resolved grid cell.
///
/// Colors already account for inverse video, hidden text, bold-as-bright, dim, selection and
/// runtime palette overrides, so a renderer can paint `bg` and draw `ch` in `fg` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    /// Base character (a space for empty cells and wide-char spacers).
    pub ch: char,
    /// Combining / zero-width characters attached to `ch`, if any.
    pub zerowidth: Option<String>,
    /// Resolved foreground.
    pub fg: Rgb,
    /// Resolved background.
    pub bg: Rgb,
    /// Bold weight.
    pub bold: bool,
    /// Italic style.
    pub italic: bool,
    /// Any underline style (single, double, curly, dotted, dashed).
    pub underline: bool,
    /// Strike-through.
    pub strikeout: bool,
    /// This cell holds a double-width character that also covers the next column.
    pub wide: bool,
    /// Placeholder cell after a wide character (or at a line end before a wrapped wide
    /// character); renderers should skip drawing its glyph.
    pub wide_spacer: bool,
    /// The cell is inside the current selection (`bg` is already the selection color).
    pub selected: bool,
}

impl CellView {
    /// A blank cell with the given colors.
    pub fn blank(fg: Rgb, bg: Rgb) -> Self {
        Self {
            ch: ' ',
            zerowidth: None,
            fg,
            bg,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide: false,
            wide_spacer: false,
            selected: false,
        }
    }
}

/// Everything needed to render one frame of a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    /// Number of columns.
    pub cols: usize,
    /// Number of visible rows.
    pub rows: usize,
    /// `rows` rows of `cols` cells each, top to bottom.
    pub cells: Vec<Vec<CellView>>,
    /// Cursor, or `None` when hidden or scrolled out of view.
    pub cursor: Option<CursorView>,
    /// Resolved cursor color.
    pub cursor_color: Rgb,
    /// How many lines the viewport is scrolled up into the history (0 = following output).
    pub display_offset: usize,
    /// Number of lines in the scrollback history (for scrollbars).
    pub history_size: usize,
    /// Current title (set by the application via OSC 0/2, else the shell name).
    pub title: String,
    /// Resolved default foreground.
    pub foreground: Rgb,
    /// Resolved default background (paint this behind the grid).
    pub background: Rgb,
    /// Terminal modes.
    pub mode: TermModeFlags,
}

impl TerminalSnapshot {
    /// Text of a visible row with trailing whitespace removed.
    pub fn row_text(&self, row: usize) -> String {
        let Some(cells) = self.cells.get(row) else {
            return String::new();
        };
        let mut text = String::with_capacity(cells.len());
        for cell in cells.iter().filter(|c| !c.wide_spacer) {
            text.push(cell.ch);
            if let Some(zw) = &cell.zerowidth {
                text.push_str(zw);
            }
        }
        text.truncate(text.trim_end().len());
        text
    }

    /// All visible rows joined with `\n`.
    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|row| self.row_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Converts alacritty's mode bits into [`TermModeFlags`].
pub(crate) fn mode_flags(mode: TermMode) -> TermModeFlags {
    TermModeFlags {
        show_cursor: mode.contains(TermMode::SHOW_CURSOR),
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        app_keypad: mode.contains(TermMode::APP_KEYPAD),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
        sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
        focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
        alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        line_feed_new_line: mode.contains(TermMode::LINE_FEED_NEW_LINE),
    }
}

/// Builds a snapshot from a locked terminal.
pub(crate) fn build_snapshot<T: alacritty_terminal::event::EventListener>(
    term: &Term<T>,
    palette: &TerminalPalette,
    title: String,
) -> TerminalSnapshot {
    let colors = ColorTable::new(palette, term.colors());
    let cols = term.columns();
    let rows = term.screen_lines();
    let foreground = colors.foreground();
    let background = colors.background();

    let content = term.renderable_content();
    let offset = content.display_offset;
    let cursor_point = content.cursor.point;
    let cursor_shape = content.cursor.shape;
    let selection = content.selection;

    let mut cells = vec![vec![CellView::blank(foreground, background); cols]; rows];
    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset as i32;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }
        let cell = indexed.cell;
        let flags = cell.flags;

        let mut fg = colors.resolve_fg(cell.fg, flags);
        let mut bg = colors.resolve_bg(cell.bg);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        let selected = selection
            .is_some_and(|range| range.contains_cell(&indexed, cursor_point, cursor_shape));
        if selected {
            bg = palette.selection_background;
            if let Some(sel_fg) = palette.selection_foreground {
                fg = sel_fg;
            }
        }

        let wide_spacer =
            flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
        cells[row as usize][col] = CellView {
            ch: if wide_spacer || cell.c == '\t' {
                ' '
            } else {
                cell.c
            },
            zerowidth: cell
                .zerowidth()
                .filter(|zw| !zw.is_empty())
                .map(|zw| zw.iter().collect()),
            fg,
            bg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: flags.contains(Flags::STRIKEOUT),
            wide: flags.contains(Flags::WIDE_CHAR),
            wide_spacer,
            selected,
        };
    }

    let cursor = match cursor_shape {
        VteCursorShape::Hidden => None,
        shape => {
            let row = cursor_point.line.0 + offset as i32;
            (row >= 0 && (row as usize) < rows && cursor_point.column.0 < cols).then_some(
                CursorView {
                    row: row as usize,
                    col: cursor_point.column.0,
                    shape: match shape {
                        VteCursorShape::Underline => CursorShape::Underline,
                        VteCursorShape::Beam => CursorShape::Beam,
                        VteCursorShape::HollowBlock => CursorShape::HollowBlock,
                        _ => CursorShape::Block,
                    },
                },
            )
        }
    };

    TerminalSnapshot {
        cols,
        rows,
        cells,
        cursor,
        cursor_color: colors.cursor(),
        display_offset: offset,
        history_size: term.history_size(),
        title,
        foreground,
        background,
        mode: mode_flags(content.mode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn term_with(bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(20, 4), VoidListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for b in bytes {
            parser.advance(&mut term, *b);
        }
        term
    }

    #[test]
    fn resolves_sgr_colors_and_attributes() {
        let p = TerminalPalette::default();
        let term = term_with(
            b"\x1b[31mR\x1b[0m\x1b[38;5;196mI\x1b[38;2;1;2;3mT\x1b[0m\x1b[7mV\x1b[0m\x1b[1;4;3mB",
        );
        let snap = build_snapshot(&term, &p, "t".into());
        let row = &snap.cells[0];
        assert_eq!(row[0].ch, 'R');
        assert_eq!(row[0].fg, p.ansi[1]);
        assert_eq!(row[0].bg, p.background);
        assert_eq!(row[1].fg, Rgb::new(255, 0, 0));
        assert_eq!(row[2].fg, Rgb::new(1, 2, 3));
        assert_eq!(row[3].fg, p.background);
        assert_eq!(row[3].bg, p.foreground);
        assert!(row[4].bold && row[4].underline && row[4].italic);
        assert_eq!(snap.row_text(0), "RITVB");
        assert_eq!(
            snap.cursor,
            Some(CursorView {
                row: 0,
                col: 5,
                shape: CursorShape::Block
            })
        );
    }

    #[test]
    fn wide_characters_and_combining_marks() {
        let term = term_with("中a\u{0301}".as_bytes());
        let snap = build_snapshot(&term, &TerminalPalette::default(), String::new());
        let row = &snap.cells[0];
        assert!(row[0].wide);
        assert!(row[1].wide_spacer);
        assert_eq!(row[2].ch, 'a');
        assert_eq!(row[2].zerowidth.as_deref(), Some("\u{0301}"));
        assert_eq!(snap.row_text(0), "中a\u{0301}");
    }

    #[test]
    fn hidden_cursor_and_modes() {
        let term = term_with(b"\x1b[?25l\x1b[?1h\x1b[?2004h");
        let snap = build_snapshot(&term, &TerminalPalette::default(), String::new());
        assert_eq!(snap.cursor, None);
        assert!(snap.mode.app_cursor);
        assert!(snap.mode.bracketed_paste);
        assert!(!snap.mode.show_cursor);
    }

    #[test]
    fn osc_background_override_is_resolved() {
        let term = term_with(b"\x1b]11;#010203\x07");
        let snap = build_snapshot(&term, &TerminalPalette::default(), String::new());
        assert_eq!(snap.background, Rgb::new(1, 2, 3));
        assert_eq!(snap.cells[3][19].bg, Rgb::new(1, 2, 3));
    }
}

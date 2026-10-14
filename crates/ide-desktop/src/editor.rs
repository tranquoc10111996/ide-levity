//! The code editor surface: a custom `iced` canvas that renders the `ViewportViewModel`
//! (syntax-colored text, gutter with line numbers and Git markers, selections, search matches,
//! diagnostics squiggles, multi-cursors, scrollbar with an overview ruler) and turns mouse
//! input into [`EditorEvent`]s.
//!
//! Text is laid out on a monospace cell grid: tabs expand to tab stops and East Asian wide
//! characters / emoji take two cells (`unicode-width`), so cursor, selection and click hit
//! testing always agree with what is drawn.

use crate::theme;
use cosmic_text::{Attrs, Buffer as CosmicBuffer, Family, FontSystem, Metrics, Shaping};
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{alignment, Color, Point, Rectangle, Renderer, Size, Theme};
use ide_core::{DiagnosticSeverityLevel, DiffMarkerKind, SearchMatch, ViewportViewModel};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// Width of the vertical scrollbar / overview ruler in pixels.
pub const SCROLLBAR_WIDTH: f32 = 14.0;
/// Number of columns a tab character advances to.
pub const TAB_SIZE: u32 = 4;
const TEXT_PADDING_LEFT: f32 = 6.0;

/// Font metrics of the editor grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorMetrics {
    pub font_size: f32,
    pub line_height: f32,
    pub cell_width: f32,
}

impl EditorMetrics {
    /// Measures the advance width of the bundled monospace font at `font_size` with
    /// `cosmic-text`, so the grid matches exactly what the renderer draws.
    pub fn measure(font_size: f32) -> Self {
        let line_height = (font_size * 1.5).round();
        let cell_width = measure_cell_width(font_size).unwrap_or(font_size * 0.6);
        Self {
            font_size,
            line_height,
            cell_width,
        }
    }

    /// Width of the gutter (line numbers + Git marker) for a document of `total_lines`.
    pub fn gutter_width(&self, total_lines: u32) -> f32 {
        let digits = total_lines.max(1).to_string().len().max(3) as f32;
        (digits + 2.0) * self.cell_width + 12.0
    }

    /// Number of fully or partially visible text rows for a canvas of `height` pixels.
    pub fn rows_for_height(&self, height: f32) -> u32 {
        (height / self.line_height).ceil().max(1.0) as u32
    }
}

fn measure_cell_width(font_size: f32) -> Option<f32> {
    let mut fs = FontSystem::new_with_fonts(
        theme::FONT_BYTES
            .iter()
            .map(|b| cosmic_text::fontdb::Source::Binary(Arc::new(b.to_vec()))),
    );
    let mut buffer = CosmicBuffer::new(&mut fs, Metrics::new(font_size, font_size * 1.5));
    let sample = "0123456789abcdefghij";
    buffer.set_text(
        &mut fs,
        sample,
        Attrs::new().family(Family::Name(theme::MONO_FAMILY)),
        Shaping::Advanced,
    );
    buffer.shape_until_scroll(&mut fs, false);
    let width = buffer.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);
    (width > 0.0).then(|| width / sample.chars().count() as f32)
}

/// Display-cell layout of one line of text.
#[derive(Debug, Clone, PartialEq)]
pub struct LineLayout {
    /// `cells[i]` is the starting cell of char `i`; `cells[len]` is the total width.
    pub cells: Vec<u32>,
    /// The line with tabs expanded to spaces, ready to draw.
    pub chars: Vec<char>,
}

impl LineLayout {
    pub fn new(text: &str) -> Self {
        let mut cells = Vec::with_capacity(text.len() + 1);
        let mut chars = Vec::with_capacity(text.len());
        let mut cell = 0u32;
        for ch in text.chars() {
            cells.push(cell);
            if ch == '\t' {
                let w = TAB_SIZE - (cell % TAB_SIZE);
                cell += w;
                chars.push('\t');
            } else {
                let w = ch.width().unwrap_or(0) as u32;
                cell += w;
                chars.push(ch);
            }
        }
        cells.push(cell);
        Self { cells, chars }
    }

    /// Cell index where char column `col` starts (clamped to the line end).
    pub fn cell_of(&self, col: u32) -> u32 {
        let idx = (col as usize).min(self.cells.len() - 1);
        self.cells[idx]
    }

    /// Char column whose boundary is nearest to the fractional cell position `cell`.
    pub fn col_at_cell(&self, cell: f32) -> u32 {
        if cell <= 0.0 {
            return 0;
        }
        for i in 0..self.chars.len() {
            let start = self.cells[i] as f32;
            let end = self.cells[i + 1] as f32;
            if cell < end {
                return if cell - start < end - cell {
                    i as u32
                } else {
                    i as u32 + 1
                };
            }
        }
        self.chars.len() as u32
    }

    /// Text of chars `[from, to)` with tabs expanded, for drawing.
    pub fn display_slice(&self, from: usize, to: usize) -> String {
        let mut out = String::new();
        for i in from..to.min(self.chars.len()) {
            if self.chars[i] == '\t' {
                let w = self.cells[i + 1] - self.cells[i];
                out.extend(std::iter::repeat_n(' ', w as usize));
            } else {
                out.push(self.chars[i]);
            }
        }
        out
    }
}

/// Mouse interaction produced by the editor canvas.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorEvent {
    /// Mouse press at a document position. `clicks` is 1, 2 (word) or 3 (line).
    Press {
        line: u32,
        col: u32,
        shift: bool,
        alt: bool,
        command: bool,
        clicks: u8,
    },
    /// Dragging with the left button held.
    Drag { line: u32, col: u32 },
    /// Wheel scrolling by whole lines (vertical) and pixels (horizontal).
    Scroll { lines: i32, pixels_x: f32 },
    /// Scrollbar thumb dragged to a fraction of the document (0.0 - 1.0).
    ScrollTo { fraction: f32 },
    /// The canvas size changed: number of visible rows and text area width in pixels.
    Resized { rows: u32, text_width: f32 },
    /// The mouse rests over a document position (for hover tooltips); `None` when it leaves.
    Hover(Option<(u32, u32)>),
}

/// Data needed to render the editor.
pub struct EditorCanvas<'a> {
    pub vm: &'a ViewportViewModel,
    pub metrics: EditorMetrics,
    pub scroll_x: f32,
    pub search_matches: &'a [SearchMatch],
    pub current_match: Option<usize>,
    pub focused: bool,
    pub cursor_visible: bool,
    /// Highlight range (line, start col, end col) e.g. for the hovered symbol.
    pub highlight: Option<(u32, u32, u32)>,
}

/// Interaction state kept by iced between frames.
#[derive(Debug, Default)]
pub struct EditorState {
    dragging: bool,
    dragging_scrollbar: bool,
    last_click: Option<(Instant, u32, u32, u8)>,
    reported: Option<(u32, u32)>,
    hover_pos: Option<(u32, u32)>,
    modifiers: iced::keyboard::Modifiers,
}

impl EditorCanvas<'_> {
    fn gutter(&self) -> f32 {
        self.metrics.gutter_width(self.vm.total_lines)
    }

    fn text_origin(&self) -> f32 {
        self.gutter() + TEXT_PADDING_LEFT
    }

    fn line_layout(&self, line: u32) -> Option<LineLayout> {
        let row = line.checked_sub(self.vm.first_visible_line)? as usize;
        self.vm.visible_lines.get(row).map(|l| LineLayout::new(&l.text))
    }

    /// Converts a canvas-local point to a (line, char column) document position.
    fn hit_test(&self, p: Point) -> (u32, u32) {
        let row = (p.y / self.metrics.line_height).floor().max(0.0) as u32;
        let last_line = self.vm.total_lines.saturating_sub(1);
        let line = (self.vm.first_visible_line + row).min(last_line);
        let cell = (p.x - self.text_origin() + self.scroll_x) / self.metrics.cell_width;
        let col = self
            .line_layout(line)
            .map(|l| l.col_at_cell(cell))
            .unwrap_or(0);
        (line, col)
    }

    fn x_of(&self, layout: &LineLayout, col: u32) -> f32 {
        self.text_origin() + layout.cell_of(col) as f32 * self.metrics.cell_width - self.scroll_x
    }

    fn row_y(&self, line: u32) -> Option<f32> {
        let row = line.checked_sub(self.vm.first_visible_line)?;
        Some(row as f32 * self.metrics.line_height)
    }

    fn scrollbar_thumb(&self, height: f32) -> (f32, f32) {
        let total = self.vm.total_lines.max(1) as f32;
        let visible_rows = (height / self.metrics.line_height).max(1.0);
        let virtual_total = total + visible_rows - 1.0;
        let thumb_h = (height * visible_rows / virtual_total).clamp(24.0, height);
        let y = (height - thumb_h) * (self.vm.first_visible_line as f32 / (total - 1.0).max(1.0));
        (y, thumb_h)
    }
}

impl canvas::Program<EditorEvent> for EditorCanvas<'_> {
    type State = EditorState;

    fn update(
        &self,
        state: &mut Self::State,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<EditorEvent>) {
        if let canvas::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) = event {
            state.modifiers = m;
        }
        // Report size changes on any event so the core viewport matches the canvas.
        let rows = self.metrics.rows_for_height(bounds.height);
        let text_width = (bounds.width - self.text_origin() - SCROLLBAR_WIDTH).max(0.0);
        let size_key = (rows, text_width as u32);
        if state.reported != Some(size_key) {
            state.reported = Some(size_key);
            return (
                canvas::event::Status::Ignored,
                Some(EditorEvent::Resized { rows, text_width }),
            );
        }

        let scrollbar_x = bounds.width - SCROLLBAR_WIDTH;
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(p) = cursor.position_in(bounds) else {
                    return (canvas::event::Status::Ignored, None);
                };
                if p.x >= scrollbar_x {
                    state.dragging_scrollbar = true;
                    let (_, thumb_h) = self.scrollbar_thumb(bounds.height);
                    let fraction = ((p.y - thumb_h / 2.0) / (bounds.height - thumb_h).max(1.0))
                        .clamp(0.0, 1.0);
                    return (
                        canvas::event::Status::Captured,
                        Some(EditorEvent::ScrollTo { fraction }),
                    );
                }
                let (line, col) = self.hit_test(p);
                let now = Instant::now();
                let clicks = match state.last_click {
                    Some((t, l, c, n))
                        if now.duration_since(t) < Duration::from_millis(450)
                            && l == line
                            && c.abs_diff(col) <= 1 =>
                    {
                        if n >= 3 {
                            1
                        } else {
                            n + 1
                        }
                    }
                    _ => 1,
                };
                state.last_click = Some((now, line, col, clicks));
                state.dragging = true;
                (
                    canvas::event::Status::Captured,
                    Some(EditorEvent::Press {
                        line,
                        col,
                        shift: state.modifiers.shift(),
                        alt: state.modifiers.alt(),
                        command: state.modifiers.command(),
                        clicks,
                    }),
                )
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.dragging_scrollbar {
                    if let Some(p) = cursor.position_from(bounds.position()) {
                        let (_, thumb_h) = self.scrollbar_thumb(bounds.height);
                        let fraction = ((p.y - thumb_h / 2.0)
                            / (bounds.height - thumb_h).max(1.0))
                        .clamp(0.0, 1.0);
                        return (
                            canvas::event::Status::Captured,
                            Some(EditorEvent::ScrollTo { fraction }),
                        );
                    }
                }
                if state.dragging {
                    if let Some(p) = cursor.position_from(bounds.position()) {
                        let clamped = Point::new(p.x, p.y.clamp(-1.0, bounds.height + 1.0));
                        let (line, col) = if p.y < 0.0 {
                            let l = self.vm.first_visible_line.saturating_sub(1);
                            (l, self.hit_test(Point::new(clamped.x, 0.0)).1)
                        } else if p.y > bounds.height {
                            let l = (self.vm.first_visible_line
                                + self.metrics.rows_for_height(bounds.height))
                            .min(self.vm.total_lines.saturating_sub(1));
                            (l, self.hit_test(Point::new(clamped.x, bounds.height - 1.0)).1)
                        } else {
                            self.hit_test(clamped)
                        };
                        return (
                            canvas::event::Status::Captured,
                            Some(EditorEvent::Drag { line, col }),
                        );
                    }
                }
                let hover = cursor
                    .position_in(bounds)
                    .filter(|p| p.x >= self.text_origin() && p.x < scrollbar_x)
                    .map(|p| self.hit_test(p));
                if hover != state.hover_pos {
                    state.hover_pos = hover;
                    return (canvas::event::Status::Ignored, Some(EditorEvent::Hover(hover)));
                }
                (canvas::event::Status::Ignored, None)
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.dragging = false;
                state.dragging_scrollbar = false;
                (canvas::event::Status::Ignored, None)
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if cursor.position_in(bounds).is_none() {
                    return (canvas::event::Status::Ignored, None);
                }
                let (dx, dy) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (
                        x * self.metrics.cell_width * 4.0,
                        y * 3.0,
                    ),
                    mouse::ScrollDelta::Pixels { x, y } => (x, y / self.metrics.line_height),
                };
                let (dx, dy) = if state.modifiers.shift() {
                    (dy * self.metrics.cell_width * 4.0 / 3.0, 0.0)
                } else {
                    (dx, dy)
                };
                (
                    canvas::event::Status::Captured,
                    Some(EditorEvent::Scroll {
                        lines: -dy.round() as i32,
                        pixels_x: -dx,
                    }),
                )
            }
            _ => (canvas::event::Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let m = self.metrics;
        let gutter = self.gutter();
        let text_left = gutter;
        let text_right = bounds.width - SCROLLBAR_WIDTH;
        let lh = m.line_height;

        frame.fill_rectangle(Point::ORIGIN, bounds.size(), theme::BG_EDITOR);

        let layouts: Vec<LineLayout> = self
            .vm
            .visible_lines
            .iter()
            .map(|l| LineLayout::new(&l.text))
            .collect();
        let primary = self.vm.cursors.iter().find(|c| c.is_primary);

        let text_clip = Rectangle::new(
            Point::new(text_left, 0.0),
            Size::new((text_right - text_left).max(0.0), bounds.height),
        );
        frame.with_clip(text_clip, |f| {
            // Coordinates inside the clip are relative to the clip origin.
            let shift = |x: f32| x - text_left;

            // Current line highlight (only when the primary cursor has no selection).
            if let Some(p) = primary {
                let has_sel = self
                    .vm
                    .selections
                    .iter()
                    .any(|s| s.start_line <= p.line && p.line <= s.end_line);
                if let (false, Some(y)) = (has_sel, self.row_y(p.line)) {
                    f.fill_rectangle(
                        Point::new(0.0, y),
                        Size::new(text_clip.width, lh),
                        theme::CURRENT_LINE,
                    );
                }
            }

            // Selections.
            let sel_color = if self.focused {
                theme::SELECTION
            } else {
                theme::SELECTION_INACTIVE
            };
            for sel in &self.vm.selections {
                for (row, layout) in layouts.iter().enumerate() {
                    let line = self.vm.first_visible_line + row as u32;
                    if line < sel.start_line || line > sel.end_line {
                        continue;
                    }
                    let start_col = if line == sel.start_line { sel.start_col } else { 0 };
                    let end_col = if line == sel.end_line {
                        sel.end_col
                    } else {
                        layout.chars.len() as u32
                    };
                    let x0 = self.x_of(layout, start_col);
                    let mut x1 = self.x_of(layout, end_col);
                    if line != sel.end_line {
                        x1 += m.cell_width * 0.6; // newline marker
                    }
                    if x1 > x0 {
                        f.fill_rectangle(
                            Point::new(shift(x0), row as f32 * lh),
                            Size::new(x1 - x0, lh),
                            sel_color,
                        );
                    }
                }
            }

            // Search matches.
            for (idx, sm) in self.search_matches.iter().enumerate() {
                let Some(y) = self.row_y(sm.line) else {
                    continue;
                };
                let row = (sm.line - self.vm.first_visible_line) as usize;
                let Some(layout) = layouts.get(row) else {
                    continue;
                };
                let len = (sm.end_char - sm.start_char) as u32;
                let x0 = self.x_of(layout, sm.column);
                let x1 = self.x_of(layout, sm.column + len);
                let color = if Some(idx) == self.current_match {
                    theme::SEARCH_MATCH_CURRENT
                } else {
                    theme::SEARCH_MATCH
                };
                f.fill_rectangle(
                    Point::new(shift(x0), y + 1.0),
                    Size::new((x1 - x0).max(2.0), lh - 2.0),
                    color,
                );
            }

            // Extra highlight (hovered symbol).
            if let Some((line, s, e)) = self.highlight {
                if let (Some(y), Some(layout)) = (self.row_y(line), self.line_layout(line)) {
                    let x0 = self.x_of(&layout, s);
                    let x1 = self.x_of(&layout, e);
                    f.fill_rectangle(
                        Point::new(shift(x0), y + lh - 2.0),
                        Size::new((x1 - x0).max(1.0), 1.0),
                        theme::ACCENT_BRIGHT,
                    );
                }
            }

            // Text with syntax spans.
            for (row, (rendered, layout)) in self
                .vm
                .visible_lines
                .iter()
                .zip(layouts.iter())
                .enumerate()
            {
                let y = row as f32 * lh;
                if y + lh > bounds.height {
                    break;
                }
                let len = layout.chars.len();
                let mut segments: Vec<(usize, usize, Color, bool)> = Vec::new();
                let mut pos = 0usize;
                for span in &rendered.spans {
                    let s = (span.start_col as usize).min(len);
                    let e = (span.end_col as usize).min(len);
                    if s < pos || s >= e {
                        continue;
                    }
                    if pos < s {
                        segments.push((pos, s, theme::TEXT, false));
                    }
                    segments.push((s, e, rgba_to_color(span.color_rgba), span.font_style & 1 == 1));
                    pos = e;
                }
                if pos < len {
                    segments.push((pos, len, theme::TEXT, false));
                }
                for (s, e, color, bold) in segments {
                    let content = layout.display_slice(s, e);
                    if content.trim().is_empty() {
                        continue;
                    }
                    let x = self.x_of(layout, s as u32);
                    let w = (layout.cells[e] - layout.cells[s]) as f32 * m.cell_width;
                    if x + w < text_left || x > text_right {
                        continue;
                    }
                    f.fill_text(Text {
                        content,
                        position: Point::new(shift(x), y),
                        color,
                        size: m.font_size.into(),
                        line_height: iced::widget::text::LineHeight::Absolute(lh.into()),
                        font: if bold { theme::MONO_BOLD } else { theme::MONO },
                        horizontal_alignment: alignment::Horizontal::Left,
                        vertical_alignment: alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Advanced,
                    });
                }
            }

            // Diagnostics squiggles.
            for d in &self.vm.diagnostics {
                let (Some(y), Some(layout)) = (self.row_y(d.line), self.line_layout(d.line)) else {
                    continue;
                };
                if y + lh > bounds.height {
                    continue;
                }
                let x0 = self.x_of(&layout, d.start_col);
                let x1 = self.x_of(&layout, d.end_col).max(x0 + m.cell_width);
                let color = severity_color(d.severity);
                let base = y + lh - 3.0;
                let path = Path::new(|b| {
                    let mut x = shift(x0);
                    let end = shift(x1);
                    let mut up = true;
                    b.move_to(Point::new(x, base));
                    while x < end {
                        x = (x + 2.0).min(end);
                        b.line_to(Point::new(x, if up { base - 1.5 } else { base + 1.0 }));
                        up = !up;
                    }
                });
                f.stroke(&path, Stroke::default().with_width(1.0).with_color(color));
            }

            // Cursors.
            if self.cursor_visible || !self.focused {
                for c in &self.vm.cursors {
                    let (Some(y), Some(layout)) = (self.row_y(c.line), self.line_layout(c.line))
                    else {
                        continue;
                    };
                    if y + lh > bounds.height {
                        continue;
                    }
                    let x = shift(self.x_of(&layout, c.column));
                    if self.focused {
                        f.fill_rectangle(
                            Point::new(x, y + 1.0),
                            Size::new(2.0, lh - 2.0),
                            if c.is_primary {
                                theme::ACCENT_BRIGHT
                            } else {
                                Color::from_rgb8(0xD2, 0xA8, 0xFF)
                            },
                        );
                    } else {
                        f.stroke(
                            &Path::rectangle(Point::new(x, y + 1.0), Size::new(1.0, lh - 2.0)),
                            Stroke::default()
                                .with_width(1.0)
                                .with_color(theme::TEXT_FAINT),
                        );
                    }
                }
            }
        });

        // Gutter (drawn after text so horizontally scrolled text never shows under it).
        frame.fill_rectangle(Point::ORIGIN, Size::new(gutter, bounds.height), theme::BG_EDITOR);
        let digits_right = gutter - m.cell_width - 8.0;
        for (row, rendered) in self.vm.visible_lines.iter().enumerate() {
            let y = row as f32 * lh;
            if y + lh > bounds.height {
                break;
            }
            let is_current = primary.is_some_and(|p| p.line == rendered.line_number);
            frame.fill_text(Text {
                content: (rendered.line_number + 1).to_string(),
                position: Point::new(digits_right, y),
                color: if is_current {
                    theme::TEXT_SECONDARY
                } else {
                    theme::TEXT_FAINT
                },
                size: (m.font_size - 1.0).into(),
                line_height: iced::widget::text::LineHeight::Absolute(lh.into()),
                font: theme::MONO,
                horizontal_alignment: alignment::Horizontal::Right,
                vertical_alignment: alignment::Vertical::Top,
                shaping: iced::widget::text::Shaping::Basic,
            });
        }
        for marker in &self.vm.git_diff_markers {
            let Some(y) = self.row_y(marker.line_number) else {
                continue;
            };
            if y + lh > bounds.height {
                continue;
            }
            let x = gutter - 7.0;
            match marker.status {
                DiffMarkerKind::Added => {
                    frame.fill_rectangle(Point::new(x, y), Size::new(3.0, lh), theme::GUTTER_ADDED)
                }
                DiffMarkerKind::Modified => frame.fill_rectangle(
                    Point::new(x, y),
                    Size::new(3.0, lh),
                    theme::GUTTER_MODIFIED,
                ),
                DiffMarkerKind::Deleted => {
                    let tri = Path::new(|b| {
                        b.move_to(Point::new(x - 1.0, y + lh - 4.0));
                        b.line_to(Point::new(x + 5.0, y + lh));
                        b.line_to(Point::new(x - 1.0, y + lh + 4.0));
                        b.close();
                    });
                    frame.fill(&tri, theme::GUTTER_DELETED);
                }
                DiffMarkerKind::Unchanged => {}
            }
        }

        // Scrollbar with overview ruler.
        let sb_x = bounds.width - SCROLLBAR_WIDTH;
        frame.fill_rectangle(
            Point::new(sb_x, 0.0),
            Size::new(SCROLLBAR_WIDTH, bounds.height),
            theme::BG_EDITOR,
        );
        frame.fill_rectangle(
            Point::new(sb_x, 0.0),
            Size::new(1.0, bounds.height),
            theme::BORDER_MUTED,
        );
        let total = self.vm.total_lines.max(1) as f32;
        for d in &self.vm.diagnostics {
            let y = bounds.height * d.line as f32 / total;
            frame.fill_rectangle(
                Point::new(sb_x + SCROLLBAR_WIDTH - 5.0, y),
                Size::new(4.0, 2.0),
                severity_color(d.severity),
            );
        }
        for (idx, sm) in self.search_matches.iter().enumerate() {
            let y = bounds.height * sm.line as f32 / total;
            frame.fill_rectangle(
                Point::new(sb_x + 3.0, y),
                Size::new(4.0, 2.0),
                if Some(idx) == self.current_match {
                    theme::WARNING
                } else {
                    Color { a: 0.7, ..theme::WARNING }
                },
            );
        }
        if let Some(p) = primary {
            let y = bounds.height * p.line as f32 / total;
            frame.fill_rectangle(
                Point::new(sb_x + 1.0, y),
                Size::new(SCROLLBAR_WIDTH - 1.0, 1.5),
                theme::TEXT_MUTED,
            );
        }
        let visible_rows = (bounds.height / lh).floor() as u32;
        if self.vm.total_lines > visible_rows || self.vm.first_visible_line > 0 {
            let (thumb_y, thumb_h) = self.scrollbar_thumb(bounds.height);
            frame.fill_rectangle(
                Point::new(sb_x + 2.0, thumb_y),
                Size::new(SCROLLBAR_WIDTH - 3.0, thumb_h),
                theme::SCROLLBAR_THUMB,
            );
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match cursor.position_in(bounds) {
            Some(p) if p.x >= bounds.width - SCROLLBAR_WIDTH => mouse::Interaction::Idle,
            Some(p) if p.x >= self.gutter() || state.dragging => mouse::Interaction::Text,
            _ => mouse::Interaction::Idle,
        }
    }
}

/// Converts the `0xRRGGBBAA` color used by highlight spans to an iced color.
pub fn rgba_to_color(rgba: u32) -> Color {
    let r = ((rgba >> 24) & 0xFF) as u8;
    let g = ((rgba >> 16) & 0xFF) as u8;
    let b = ((rgba >> 8) & 0xFF) as u8;
    let a = (rgba & 0xFF) as f32 / 255.0;
    Color::from_rgba8(r, g, b, a)
}

pub fn severity_color(s: DiagnosticSeverityLevel) -> Color {
    match s {
        DiagnosticSeverityLevel::Error => theme::DANGER,
        DiagnosticSeverityLevel::Warning => theme::WARNING,
        DiagnosticSeverityLevel::Information => theme::INFO,
        DiagnosticSeverityLevel::Hint => theme::TEXT_MUTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_expands_tabs_and_wide_chars() {
        let l = LineLayout::new("\tab界c");
        assert_eq!(l.cells, vec![0, 4, 5, 6, 8, 9]);
        assert_eq!(l.display_slice(0, 3), "    ab");
        assert_eq!(l.col_at_cell(4.4), 1);
        assert_eq!(l.col_at_cell(6.9), 3);
        assert_eq!(l.col_at_cell(7.1), 4);
        assert_eq!(l.col_at_cell(100.0), 5);
        let v = LineLayout::new("Chào mừng");
        assert_eq!(v.cells.last().copied(), Some(9));
    }

    #[test]
    fn metrics_measure_bundled_font() {
        let m = EditorMetrics::measure(14.0);
        assert!((m.cell_width - 8.4).abs() < 0.2, "cell width {}", m.cell_width);
        assert_eq!(m.line_height, 21.0);
        assert!(m.gutter_width(10) < m.gutter_width(100_000));
    }
}

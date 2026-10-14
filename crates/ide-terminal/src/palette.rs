//! Color palette and SGR color resolution.
//!
//! Terminal cells store colors symbolically (named ANSI colors, 256-color indices or 24-bit
//! "truecolor" values). [`TerminalPalette`] maps those to concrete [`Rgb`] values, and
//! [`ColorTable`] combines the palette with any runtime overrides applied by programs through
//! OSC 4/10/11/12 so that snapshots can hand fully resolved colors to the renderer.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::{Colors, COUNT};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb as VteRgb};

/// Factor applied to colors rendered with the SGR "dim/faint" attribute.
const DIM_FACTOR: f32 = 0.66;

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Creates a color from its channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Creates a color from a `0xRRGGBB` value.
    pub const fn from_hex(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xff) as u8,
            g: ((hex >> 8) & 0xff) as u8,
            b: (hex & 0xff) as u8,
        }
    }

    /// Returns the color packed as `0xRRGGBBAA` with full opacity.
    pub const fn to_rgba_u32(self) -> u32 {
        ((self.r as u32) << 24) | ((self.g as u32) << 16) | ((self.b as u32) << 8) | 0xff
    }

    /// Returns the color scaled by `factor` (clamped to the valid range).
    #[must_use]
    pub fn scaled(self, factor: f32) -> Self {
        let scale = |c: u8| (f32::from(c) * factor).round().clamp(0.0, 255.0) as u8;
        Self {
            r: scale(self.r),
            g: scale(self.g),
            b: scale(self.b),
        }
    }
}

impl From<VteRgb> for Rgb {
    fn from(c: VteRgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<Rgb> for VteRgb {
    fn from(c: Rgb) -> Self {
        VteRgb {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// Colors used to render a terminal.
///
/// The default is a GitHub-dark style theme (background `#0D1117`, foreground `#E6EDF3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPalette {
    /// Default foreground.
    pub foreground: Rgb,
    /// Default background.
    pub background: Rgb,
    /// Cursor color.
    pub cursor: Rgb,
    /// Background of selected cells.
    pub selection_background: Rgb,
    /// Foreground of selected cells; `None` keeps the cell's own foreground.
    pub selection_foreground: Option<Rgb>,
    /// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan, white, followed by
    /// their bright variants in the same order.
    pub ansi: [Rgb; 16],
    /// Render bold text that uses one of the 8 normal ANSI colors with its bright variant
    /// (xterm / VS Code `drawBoldTextInBrightColors`).
    pub bold_is_bright: bool,
}

impl Default for TerminalPalette {
    fn default() -> Self {
        Self::github_dark()
    }
}

impl TerminalPalette {
    /// GitHub-dark style palette matching the IDE chrome.
    pub fn github_dark() -> Self {
        Self {
            foreground: Rgb::from_hex(0xE6EDF3),
            background: Rgb::from_hex(0x0D1117),
            cursor: Rgb::from_hex(0x2F81F7),
            selection_background: Rgb::from_hex(0x264F78),
            selection_foreground: None,
            ansi: [
                Rgb::from_hex(0x484F58),
                Rgb::from_hex(0xFF7B72),
                Rgb::from_hex(0x3FB950),
                Rgb::from_hex(0xD29922),
                Rgb::from_hex(0x58A6FF),
                Rgb::from_hex(0xBC8CFF),
                Rgb::from_hex(0x39C5CF),
                Rgb::from_hex(0xB1BAC4),
                Rgb::from_hex(0x6E7681),
                Rgb::from_hex(0xFFA198),
                Rgb::from_hex(0x56D364),
                Rgb::from_hex(0xE3B341),
                Rgb::from_hex(0x79C0FF),
                Rgb::from_hex(0xD2A8FF),
                Rgb::from_hex(0x56D4DD),
                Rgb::from_hex(0xFFFFFF),
            ],
            bold_is_bright: true,
        }
    }

    /// Returns the color for an xterm 256-color index (0..=255).
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = 8 + (index - 232) * 10;
                Rgb::new(v, v, v)
            }
        }
    }

    /// Returns the palette color for an alacritty color-table index (0..269), see
    /// [`alacritty_terminal::term::color::Colors`] for the layout.
    pub(crate) fn table_entry(&self, index: usize) -> Rgb {
        match index {
            0..=255 => self.indexed(index as u8),
            i if i == NamedColor::Foreground as usize => self.foreground,
            i if i == NamedColor::Background as usize => self.background,
            i if i == NamedColor::Cursor as usize => self.cursor,
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                self.ansi[i - NamedColor::DimBlack as usize].scaled(DIM_FACTOR)
            }
            i if i == NamedColor::BrightForeground as usize => self.foreground,
            i if i == NamedColor::DimForeground as usize => self.foreground.scaled(DIM_FACTOR),
            _ => self.foreground,
        }
    }
}

/// Fully resolved color table: palette plus runtime OSC overrides.
pub(crate) struct ColorTable {
    entries: [Rgb; COUNT],
    bold_is_bright: bool,
}

impl ColorTable {
    /// Builds the table from the palette and the terminal's color overrides.
    pub(crate) fn new(palette: &TerminalPalette, overrides: &Colors) -> Self {
        let mut entries = [Rgb::default(); COUNT];
        for (i, entry) in entries.iter_mut().enumerate() {
            *entry = overrides[i]
                .map(Rgb::from)
                .unwrap_or_else(|| palette.table_entry(i));
        }
        // Derived colors follow overridden base colors unless they were overridden themselves.
        for i in 0..8 {
            let dim = NamedColor::DimBlack as usize + i;
            if overrides[dim].is_none() {
                entries[dim] = entries[i].scaled(DIM_FACTOR);
            }
        }
        let fg = NamedColor::Foreground as usize;
        if overrides[NamedColor::BrightForeground as usize].is_none() {
            entries[NamedColor::BrightForeground as usize] = entries[fg];
        }
        if overrides[NamedColor::DimForeground as usize].is_none() {
            entries[NamedColor::DimForeground as usize] = entries[fg].scaled(DIM_FACTOR);
        }
        Self {
            entries,
            bold_is_bright: palette.bold_is_bright,
        }
    }

    /// Color at a raw table index.
    pub(crate) fn get(&self, index: usize) -> Rgb {
        self.entries
            .get(index)
            .copied()
            .unwrap_or(self.entries[NamedColor::Foreground as usize])
    }

    /// Default foreground.
    pub(crate) fn foreground(&self) -> Rgb {
        self.get(NamedColor::Foreground as usize)
    }

    /// Default background.
    pub(crate) fn background(&self) -> Rgb {
        self.get(NamedColor::Background as usize)
    }

    /// Cursor color.
    pub(crate) fn cursor(&self) -> Rgb {
        self.get(NamedColor::Cursor as usize)
    }

    /// Resolves a foreground color, applying bold-as-bright and dim handling the same way
    /// Alacritty's renderer does.
    pub(crate) fn resolve_fg(&self, color: Color, flags: Flags) -> Rgb {
        let dim_bold = flags & Flags::DIM_BOLD;
        match color {
            Color::Spec(rgb) => {
                let rgb = Rgb::from(rgb);
                if dim_bold == Flags::DIM {
                    rgb.scaled(DIM_FACTOR)
                } else {
                    rgb
                }
            }
            Color::Named(named) => {
                let named = if dim_bold == Flags::DIM_BOLD && named == NamedColor::Foreground {
                    NamedColor::DimForeground
                } else if self.bold_is_bright && dim_bold == Flags::BOLD {
                    named.to_bright()
                } else if dim_bold == Flags::DIM
                    || (!self.bold_is_bright && dim_bold == Flags::DIM_BOLD)
                {
                    named.to_dim()
                } else {
                    named
                };
                self.get(named as usize)
            }
            Color::Indexed(idx) => {
                let index = match (self.bold_is_bright, dim_bold, idx) {
                    (true, Flags::BOLD, 0..=7) => usize::from(idx) + 8,
                    (false, Flags::DIM, 8..=15) => usize::from(idx) - 8,
                    (false, Flags::DIM, 0..=7) => NamedColor::DimBlack as usize + usize::from(idx),
                    _ => usize::from(idx),
                };
                self.get(index)
            }
        }
    }

    /// Resolves a background color.
    pub(crate) fn resolve_bg(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb.into(),
            Color::Named(named) => self.get(named as usize),
            Color::Indexed(idx) => self.get(usize::from(idx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ColorTable {
        ColorTable::new(&TerminalPalette::default(), &Colors::default())
    }

    #[test]
    fn indexed_cube_and_grayscale() {
        let p = TerminalPalette::default();
        assert_eq!(p.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(p.indexed(196), Rgb::new(255, 0, 0));
        assert_eq!(p.indexed(21), Rgb::new(0, 0, 255));
        assert_eq!(p.indexed(231), Rgb::new(255, 255, 255));
        assert_eq!(p.indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(p.indexed(255), Rgb::new(238, 238, 238));
        assert_eq!(p.indexed(1), p.ansi[1]);
    }

    #[test]
    fn named_and_default_colors() {
        let t = table();
        let p = TerminalPalette::default();
        assert_eq!(
            t.resolve_fg(Color::Named(NamedColor::Red), Flags::empty()),
            p.ansi[1]
        );
        assert_eq!(
            t.resolve_fg(Color::Named(NamedColor::Foreground), Flags::empty()),
            p.foreground
        );
        assert_eq!(
            t.resolve_bg(Color::Named(NamedColor::Background)),
            p.background
        );
        assert_eq!(t.background(), Rgb::from_hex(0x0D1117));
        assert_eq!(t.foreground(), Rgb::from_hex(0xE6EDF3));
    }

    #[test]
    fn bold_uses_bright_and_dim_scales() {
        let t = table();
        let p = TerminalPalette::default();
        assert_eq!(
            t.resolve_fg(Color::Named(NamedColor::Red), Flags::BOLD),
            p.ansi[9]
        );
        assert_eq!(t.resolve_fg(Color::Indexed(2), Flags::BOLD), p.ansi[10]);
        assert_eq!(
            t.resolve_fg(Color::Named(NamedColor::Red), Flags::DIM),
            p.ansi[1].scaled(0.66)
        );
        let spec = VteRgb {
            r: 100,
            g: 200,
            b: 50,
        };
        assert_eq!(
            t.resolve_fg(Color::Spec(spec), Flags::empty()),
            Rgb::new(100, 200, 50)
        );
        assert_eq!(
            t.resolve_fg(Color::Spec(spec), Flags::DIM),
            Rgb::new(66, 132, 33)
        );
    }

    #[test]
    fn overrides_take_precedence() {
        let mut overrides = Colors::default();
        overrides[NamedColor::Background] = Some(VteRgb { r: 1, g: 2, b: 3 });
        overrides[1] = Some(VteRgb {
            r: 10,
            g: 20,
            b: 30,
        });
        let t = ColorTable::new(&TerminalPalette::default(), &overrides);
        assert_eq!(t.background(), Rgb::new(1, 2, 3));
        assert_eq!(
            t.resolve_fg(Color::Indexed(1), Flags::empty()),
            Rgb::new(10, 20, 30)
        );
        assert_eq!(
            t.get(NamedColor::DimRed as usize),
            Rgb::new(10, 20, 30).scaled(0.66)
        );
    }

    #[test]
    fn rgba_packing() {
        assert_eq!(Rgb::from_hex(0x0D1117).to_rgba_u32(), 0x0D1117FF);
    }
}

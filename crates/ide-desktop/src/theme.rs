//! Visual theme for the desktop IDE: a GitHub-dark inspired palette plus reusable widget
//! styles. Every color used by the UI lives here so the look stays consistent.

use iced::widget::{button, container, scrollable, text_input};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector};

/// Font family name of the bundled editor font.
pub const MONO_FAMILY: &str = "JetBrains Mono";

/// Bundled monospace font used by the editor, terminal and code views.
pub const MONO: Font = Font::with_name(MONO_FAMILY);

/// Bundled bold monospace font.
pub const MONO_BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::with_name(MONO_FAMILY)
};

/// Raw bytes of the bundled fonts (registered with the application at startup).
pub const FONT_BYTES: [&[u8]; 2] = [
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
];

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

pub const BG_EDITOR: Color = rgb(0x0D, 0x11, 0x17);
pub const BG_PANEL: Color = rgb(0x16, 0x1B, 0x22);
pub const BG_ELEVATED: Color = rgb(0x1C, 0x21, 0x28);
pub const BG_HOVER: Color = rgb(0x21, 0x26, 0x2D);
pub const BG_ACTIVE: Color = rgb(0x1F, 0x29, 0x37);
pub const BG_INPUT: Color = rgb(0x0D, 0x11, 0x17);
pub const BORDER: Color = rgb(0x30, 0x36, 0x3D);
pub const BORDER_MUTED: Color = rgb(0x21, 0x26, 0x2D);
pub const TEXT: Color = rgb(0xE6, 0xED, 0xF3);
pub const TEXT_SECONDARY: Color = rgb(0xC9, 0xD1, 0xD9);
pub const TEXT_MUTED: Color = rgb(0x8B, 0x94, 0x9E);
pub const TEXT_FAINT: Color = rgb(0x6E, 0x76, 0x81);
pub const ACCENT: Color = rgb(0x2F, 0x81, 0xF7);
pub const ACCENT_BRIGHT: Color = rgb(0x58, 0xA6, 0xFF);
pub const ACCENT_BG: Color = rgb(0x1F, 0x6F, 0xEB);
pub const SUCCESS: Color = rgb(0x3F, 0xB9, 0x50);
pub const SUCCESS_BG: Color = rgb(0x23, 0x86, 0x36);
pub const WARNING: Color = rgb(0xD2, 0x99, 0x22);
pub const DANGER: Color = rgb(0xF8, 0x51, 0x49);
pub const DANGER_BG: Color = rgb(0xDA, 0x36, 0x33);
pub const INFO: Color = rgb(0x58, 0xA6, 0xFF);
pub const SELECTION: Color = Color::from_rgba(0.149, 0.310, 0.471, 0.75);
pub const SELECTION_INACTIVE: Color = Color::from_rgba(0.149, 0.310, 0.471, 0.40);
pub const CURRENT_LINE: Color = Color::from_rgba(0.129, 0.149, 0.176, 0.70);
pub const SEARCH_MATCH: Color = Color::from_rgba(0.824, 0.600, 0.133, 0.35);
pub const SEARCH_MATCH_CURRENT: Color = Color::from_rgba(0.824, 0.600, 0.133, 0.70);
pub const GUTTER_ADDED: Color = rgb(0x2E, 0xA0, 0x43);
pub const GUTTER_MODIFIED: Color = rgb(0x1F, 0x6F, 0xEB);
pub const GUTTER_DELETED: Color = rgb(0xF8, 0x51, 0x49);
pub const SCROLLBAR_THUMB: Color = Color::from_rgba(0.431, 0.463, 0.506, 0.35);
pub const SCROLLBAR_THUMB_HOVER: Color = Color::from_rgba(0.431, 0.463, 0.506, 0.60);
pub const STATUS_BAR: Color = rgb(0x16, 0x1B, 0x22);

/// Plain panel background with a 1px border.
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_PANEL.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(TEXT_SECONDARY),
        ..Default::default()
    }
}

/// Panel background without border.
pub fn panel_flat(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_PANEL.into()),
        text_color: Some(TEXT_SECONDARY),
        ..Default::default()
    }
}

/// Editor background.
pub fn editor_bg(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_EDITOR.into()),
        text_color: Some(TEXT),
        ..Default::default()
    }
}

/// Floating overlay (command palette, dialogs, completion popup) with shadow.
pub fn overlay(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_ELEVATED.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.55),
            offset: Vector::new(0.0, 6.0),
            blur_radius: 18.0,
        },
        text_color: Some(TEXT),
    }
}

/// Semi-transparent backdrop behind modal dialogs.
pub fn backdrop(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
        ..Default::default()
    }
}

/// Status bar.
pub fn status_bar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(STATUS_BAR.into()),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(TEXT_MUTED),
        ..Default::default()
    }
}

/// Inline notification banner colored by severity.
pub fn banner(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Color { a: 0.15, ..color }.into()),
        border: Border {
            color: Color { a: 0.6, ..color },
            width: 1.0,
            radius: 4.0.into(),
        },
        text_color: Some(TEXT),
        ..Default::default()
    }
}

fn hovered(status: button::Status) -> bool {
    matches!(status, button::Status::Hovered | button::Status::Pressed)
}

/// Primary (filled accent) button.
pub fn btn_primary(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Disabled => BG_HOVER,
        s if hovered(s) => ACCENT,
        _ => ACCENT_BG,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: if status == button::Status::Disabled {
            TEXT_FAINT
        } else {
            Color::WHITE
        },
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Secondary (outlined) button.
pub fn btn_secondary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(if hovered(status) { BG_HOVER } else { BG_ELEVATED }.into()),
        text_color: if status == button::Status::Disabled {
            TEXT_FAINT
        } else {
            TEXT_SECONDARY
        },
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Destructive action button.
pub fn btn_danger(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(if hovered(status) { DANGER } else { DANGER_BG }.into()),
        text_color: Color::WHITE,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Confirming action button (Accept).
pub fn btn_success(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(if hovered(status) { SUCCESS } else { SUCCESS_BG }.into()),
        text_color: Color::WHITE,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Borderless text-like button used for list rows, tree items and icons.
pub fn btn_ghost(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let bg = if active {
            Some(Background::Color(BG_ACTIVE))
        } else if hovered(status) {
            Some(Background::Color(BG_HOVER))
        } else {
            None
        };
        button::Style {
            background: bg,
            text_color: if active { TEXT } else { TEXT_SECONDARY },
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 3.0.into(),
            },
            ..Default::default()
        }
    }
}

/// Editor tab button.
pub fn btn_tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| button::Style {
        background: Some(
            if active {
                BG_EDITOR
            } else if hovered(status) {
                BG_HOVER
            } else {
                BG_PANEL
            }
            .into(),
        ),
        text_color: if active { TEXT } else { TEXT_MUTED },
        border: Border {
            color: if active { ACCENT_BRIGHT } else { Color::TRANSPARENT },
            width: if active { 1.0 } else { 0.0 },
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// Segmented toggle button (sidebar/panel section switches).
pub fn btn_toggle(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| button::Style {
        background: Some(
            if active {
                BG_ACTIVE
            } else if hovered(status) {
                BG_HOVER
            } else {
                BG_PANEL
            }
            .into(),
        ),
        text_color: if active { TEXT } else { TEXT_MUTED },
        border: Border {
            color: if active { ACCENT_BRIGHT } else { Color::TRANSPARENT },
            width: if active { 1.0 } else { 0.0 },
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Text input style.
pub fn input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused);
    text_input::Style {
        background: BG_INPUT.into(),
        border: Border {
            color: if focused { ACCENT_BRIGHT } else { BORDER },
            width: 1.0,
            radius: 4.0.into(),
        },
        icon: TEXT_MUTED,
        placeholder: TEXT_FAINT,
        value: TEXT,
        selection: SELECTION,
    }
}

/// Thin dark scrollbars.
pub fn scrollbar(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let thumb_color = match status {
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. } => {
            SCROLLBAR_THUMB_HOVER
        }
        scrollable::Status::Active => SCROLLBAR_THUMB,
    };
    for rail in [&mut style.vertical_rail, &mut style.horizontal_rail] {
        rail.background = None;
        rail.border = Border::default();
        rail.scroller.color = thumb_color;
        rail.scroller.border = Border {
            radius: 4.0.into(),
            ..Border::default()
        };
    }
    style.container = container::Style::default();
    style
}

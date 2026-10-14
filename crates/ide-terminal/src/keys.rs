//! GUI-toolkit independent keyboard encoding (xterm conventions).

/// Terminal mode flags relevant to input handling and rendering.
///
/// Mirrors the subset of `alacritty_terminal::term::TermMode` a front-end needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermModeFlags {
    /// The cursor should be drawn (DECTCEM).
    pub show_cursor: bool,
    /// Application cursor keys (DECCKM): arrows send `ESC O A` instead of `ESC [ A`.
    pub app_cursor: bool,
    /// Application keypad mode (DECKPAM).
    pub app_keypad: bool,
    /// Bracketed paste mode (`ESC [ ? 2004 h`).
    pub bracketed_paste: bool,
    /// The alternate screen buffer is active (full-screen apps like vim, less, htop).
    pub alt_screen: bool,
    /// The application requested mouse reporting (click, drag or motion).
    pub mouse_reporting: bool,
    /// Mouse reports should use the SGR (1006) encoding.
    pub sgr_mouse: bool,
    /// The application wants focus in/out reports (`ESC [ I` / `ESC [ O`).
    pub focus_reporting: bool,
    /// Mouse wheel should be translated to arrow keys in the alternate screen (1007).
    pub alternate_scroll: bool,
    /// Line feed / new line mode (LNM): Enter sends `CR LF`.
    pub line_feed_new_line: bool,
}

/// A key independent of any GUI toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKey {
    /// A printable character, already shifted by the keyboard layout (e.g. `'A'` for Shift+a).
    Char(char),
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Escape.
    Escape,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete (forward delete).
    Delete,
    /// Function key F1..=F12. Other numbers are not encodable.
    F(u8),
}

/// A key press with modifier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    /// The key.
    pub key: TermKey,
    /// Control held.
    pub ctrl: bool,
    /// Alt / Option held (sent as an `ESC` prefix or xterm modifier parameter).
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
}

impl KeyInput {
    /// A key without modifiers.
    pub fn plain(key: TermKey) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// A key with Control held.
    pub fn ctrl(key: TermKey) -> Self {
        Self {
            key,
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    /// The xterm modifier parameter (`1 + shift + 2*alt + 4*ctrl`), or `None` without modifiers.
    fn modifier_param(&self) -> Option<u8> {
        let m = 1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        (m > 1).then_some(m)
    }
}

/// Encodes a key press into the bytes a terminal application expects.
///
/// Follows xterm conventions: Ctrl+letter produces C0 control codes, Alt prefixes `ESC`,
/// cursor keys honor application cursor mode (DECCKM), and modified special keys use the
/// `CSI 1 ; <mod> X` / `CSI <n> ; <mod> ~` forms. Returns `None` for keys that produce no
/// input (e.g. `F(13)`).
///
/// Front-ends should handle their own shortcuts (copy/paste, Shift+PageUp scrollback) before
/// calling this.
pub fn encode_key(input: KeyInput, mode: TermModeFlags) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(8);
    match input.key {
        TermKey::Char(c) => {
            if input.alt {
                out.push(0x1b);
            }
            match input.ctrl.then(|| ctrl_code(c)).flatten() {
                Some(code) => out.push(code),
                None => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        TermKey::Enter => {
            if input.alt {
                out.push(0x1b);
            }
            if mode.line_feed_new_line {
                out.extend_from_slice(b"\r\n");
            } else {
                out.push(b'\r');
            }
        }
        TermKey::Backspace => {
            if input.alt {
                out.push(0x1b);
            }
            out.push(if input.ctrl { 0x08 } else { 0x7f });
        }
        TermKey::Tab => {
            if input.shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                if input.alt {
                    out.push(0x1b);
                }
                out.push(b'\t');
            }
        }
        TermKey::Escape => {
            if input.alt {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        TermKey::Up => cursor_key(&mut out, b'A', input, mode),
        TermKey::Down => cursor_key(&mut out, b'B', input, mode),
        TermKey::Right => cursor_key(&mut out, b'C', input, mode),
        TermKey::Left => cursor_key(&mut out, b'D', input, mode),
        TermKey::Home => cursor_key(&mut out, b'H', input, mode),
        TermKey::End => cursor_key(&mut out, b'F', input, mode),
        TermKey::Insert => tilde_key(&mut out, 2, input),
        TermKey::Delete => tilde_key(&mut out, 3, input),
        TermKey::PageUp => tilde_key(&mut out, 5, input),
        TermKey::PageDown => tilde_key(&mut out, 6, input),
        TermKey::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            match input.modifier_param() {
                Some(m) => out.extend_from_slice(format!("\x1b[1;{m}").as_bytes()),
                None => out.extend_from_slice(b"\x1bO"),
            }
            out.push(final_byte);
        }
        TermKey::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(&mut out, code, input);
        }
        TermKey::F(_) => return None,
    }
    Some(out)
}

/// Writes a cursor-movement style key (`CSI X`, `SS3 X` or `CSI 1;m X`).
fn cursor_key(out: &mut Vec<u8>, final_byte: u8, input: KeyInput, mode: TermModeFlags) {
    match input.modifier_param() {
        Some(m) => out.extend_from_slice(format!("\x1b[1;{m}").as_bytes()),
        None if mode.app_cursor => out.extend_from_slice(b"\x1bO"),
        None => out.extend_from_slice(b"\x1b["),
    }
    out.push(final_byte);
}

/// Writes a `CSI n ~` style key, with an optional modifier parameter.
fn tilde_key(out: &mut Vec<u8>, code: u8, input: KeyInput) {
    let seq = match input.modifier_param() {
        Some(m) => format!("\x1b[{code};{m}~"),
        None => format!("\x1b[{code}~"),
    };
    out.extend_from_slice(seq.as_bytes());
}

/// Maps Ctrl+character to its C0 control code, if one exists.
fn ctrl_code(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '~' | '6' => Some(0x1e),
        '_' | '-' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(input: KeyInput) -> Vec<u8> {
        encode_key(input, TermModeFlags::default()).expect("encodable")
    }

    fn with(key: TermKey, ctrl: bool, alt: bool, shift: bool) -> KeyInput {
        KeyInput {
            key,
            ctrl,
            alt,
            shift,
        }
    }

    #[test]
    fn printable_characters_are_utf8() {
        assert_eq!(enc(KeyInput::plain(TermKey::Char('a'))), b"a");
        assert_eq!(enc(KeyInput::plain(TermKey::Char('ệ'))), "ệ".as_bytes());
        assert_eq!(enc(KeyInput::plain(TermKey::Char('😀'))), "😀".as_bytes());
    }

    #[test]
    fn ctrl_letters_produce_control_codes() {
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('c'))), [0x03]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('C'))), [0x03]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('d'))), [0x04]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('z'))), [0x1a]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('a'))), [0x01]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char(' '))), [0x00]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('['))), [0x1b]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('\\'))), [0x1c]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('_'))), [0x1f]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('?'))), [0x7f]);
        // No control code: the character is sent unchanged.
        assert_eq!(enc(KeyInput::ctrl(TermKey::Char('é'))), "é".as_bytes());
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(with(TermKey::Char('b'), false, true, false)), b"\x1bb");
        assert_eq!(
            enc(with(TermKey::Char('c'), true, true, false)),
            b"\x1b\x03"
        );
        assert_eq!(
            enc(with(TermKey::Backspace, false, true, false)),
            b"\x1b\x7f"
        );
        assert_eq!(enc(with(TermKey::Enter, false, true, false)), b"\x1b\r");
    }

    #[test]
    fn basic_keys() {
        assert_eq!(enc(KeyInput::plain(TermKey::Enter)), b"\r");
        assert_eq!(enc(KeyInput::plain(TermKey::Backspace)), [0x7f]);
        assert_eq!(enc(KeyInput::ctrl(TermKey::Backspace)), [0x08]);
        assert_eq!(enc(KeyInput::plain(TermKey::Tab)), b"\t");
        assert_eq!(enc(with(TermKey::Tab, false, false, true)), b"\x1b[Z");
        assert_eq!(enc(KeyInput::plain(TermKey::Escape)), b"\x1b");
        let lnm = TermModeFlags {
            line_feed_new_line: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(KeyInput::plain(TermKey::Enter), lnm).unwrap(),
            b"\r\n"
        );
    }

    #[test]
    fn cursor_keys_respect_decckm() {
        assert_eq!(enc(KeyInput::plain(TermKey::Up)), b"\x1b[A");
        assert_eq!(enc(KeyInput::plain(TermKey::Down)), b"\x1b[B");
        assert_eq!(enc(KeyInput::plain(TermKey::Right)), b"\x1b[C");
        assert_eq!(enc(KeyInput::plain(TermKey::Left)), b"\x1b[D");
        assert_eq!(enc(KeyInput::plain(TermKey::Home)), b"\x1b[H");
        assert_eq!(enc(KeyInput::plain(TermKey::End)), b"\x1b[F");
        let app = TermModeFlags {
            app_cursor: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(KeyInput::plain(TermKey::Up), app).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(KeyInput::plain(TermKey::Home), app).unwrap(),
            b"\x1bOH"
        );
        // Modified cursor keys always use the CSI form.
        assert_eq!(
            encode_key(KeyInput::ctrl(TermKey::Left), app).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(enc(with(TermKey::Right, false, true, true)), b"\x1b[1;4C");
        assert_eq!(enc(with(TermKey::Up, false, false, true)), b"\x1b[1;2A");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(enc(KeyInput::plain(TermKey::Insert)), b"\x1b[2~");
        assert_eq!(enc(KeyInput::plain(TermKey::Delete)), b"\x1b[3~");
        assert_eq!(enc(KeyInput::plain(TermKey::PageUp)), b"\x1b[5~");
        assert_eq!(enc(KeyInput::plain(TermKey::PageDown)), b"\x1b[6~");
        assert_eq!(enc(KeyInput::ctrl(TermKey::Delete)), b"\x1b[3;5~");
        assert_eq!(enc(KeyInput::plain(TermKey::F(1))), b"\x1bOP");
        assert_eq!(enc(KeyInput::plain(TermKey::F(4))), b"\x1bOS");
        assert_eq!(enc(with(TermKey::F(2), false, false, true)), b"\x1b[1;2Q");
        assert_eq!(enc(KeyInput::plain(TermKey::F(5))), b"\x1b[15~");
        assert_eq!(enc(KeyInput::plain(TermKey::F(6))), b"\x1b[17~");
        assert_eq!(enc(KeyInput::plain(TermKey::F(10))), b"\x1b[21~");
        assert_eq!(enc(KeyInput::plain(TermKey::F(11))), b"\x1b[23~");
        assert_eq!(enc(KeyInput::plain(TermKey::F(12))), b"\x1b[24~");
        assert_eq!(enc(KeyInput::ctrl(TermKey::F(12))), b"\x1b[24;5~");
        assert_eq!(
            encode_key(KeyInput::plain(TermKey::F(13)), TermModeFlags::default()),
            None
        );
        assert_eq!(
            encode_key(KeyInput::plain(TermKey::F(0)), TermModeFlags::default()),
            None
        );
    }
}

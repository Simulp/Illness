//! xterm-compatible keyboard, paste, and mouse encoding.

const ESC: u8 = 0x1b;

/// Modifier state for a key or mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Layout-independent key identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
}

impl Key {
    /// Map a character produced by the platform to the matching logical key.
    pub fn from_char(c: char) -> Self {
        match c {
            '\r' | '\n' => Self::Enter,
            '\t' => Self::Tab,
            '\u{8}' | '\u{7f}' => Self::Backspace,
            '\u{1b}' => Self::Escape,
            ' ' => Self::Space,
            c => Self::Char(c),
        }
    }
}

/// A single key press as reported by the window system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    /// Text committed for this press, if any. Used verbatim without Ctrl/Alt.
    pub text: Option<String>,
    pub mods: Mods,
    /// Reserved for the kitty keyboard protocol; legacy encoding ignores it.
    pub repeat: bool,
}

impl KeyInput {
    pub fn new(key: Key, mods: Mods, text: Option<String>) -> Self {
        Self {
            key,
            text,
            mods,
            repeat: false,
        }
    }
}

/// Terminal modes that change how input is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyModes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Send ESC + CR for Shift+Enter instead of a plain CR.
    pub shift_enter_newline: bool,
}

impl Default for KeyModes {
    fn default() -> Self {
        Self {
            app_cursor: false,
            bracketed_paste: false,
            shift_enter_newline: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MousePhase {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// No button is held; used for hover motion and button-less releases.
    None,
}

/// Encode a key press for the PTY.
pub fn encode_key(input: KeyInput, modes: KeyModes) -> Vec<u8> {
    let KeyInput {
        key,
        text,
        mods,
        repeat: _,
    } = input;

    match key {
        // Shift+Enter sends ESC + CR, the sequence terminal-setup helpers register for
        // a newline in TUI clients. This is configurable because a plain shell prompt
        // reads a leading ESC as RevertLine and clears the line. Ctrl+Shift+Enter
        // always stays CR.
        Key::Enter if mods.shift && !mods.ctrl && modes.shift_enter_newline => vec![ESC, 0x0d],
        Key::Enter => single(0x0d, mods.alt),
        Key::Tab if mods.shift => vec![ESC, b'[', b'Z'],
        Key::Tab => single(0x09, mods.alt),
        Key::Backspace => single(0x7f, mods.alt),
        Key::Escape => single(ESC, mods.alt),
        Key::Up => arrow(b'A', mods, modes.app_cursor),
        Key::Down => arrow(b'B', mods, modes.app_cursor),
        Key::Right => arrow(b'C', mods, modes.app_cursor),
        Key::Left => arrow(b'D', mods, modes.app_cursor),
        Key::Home => csi_simple(b'H', mods),
        Key::End => csi_simple(b'F', mods),
        Key::Insert => tilde(2, mods),
        Key::Delete => tilde(3, mods),
        Key::PageUp => tilde(5, mods),
        Key::PageDown => tilde(6, mods),
        Key::F1 => function_ss3(b'P', mods),
        Key::F2 => function_ss3(b'Q', mods),
        Key::F3 => function_ss3(b'R', mods),
        Key::F4 => function_ss3(b'S', mods),
        Key::F5 => tilde(15, mods),
        Key::F6 => tilde(17, mods),
        Key::F7 => tilde(18, mods),
        Key::F8 => tilde(19, mods),
        Key::F9 => tilde(20, mods),
        Key::F10 => tilde(21, mods),
        Key::F11 => tilde(23, mods),
        Key::F12 => tilde(24, mods),
        Key::Space => encode_char(' ', text.as_deref(), mods),
        Key::Char(c) => encode_char(c, text.as_deref(), mods),
    }
}

/// Encode pasted text, optionally wrapped in bracketed-paste markers.
///
/// Bracketed paste strips ESC and ETX so clipboard contents cannot terminate
/// the paste block or inject key sequences. Unbracketed paste normalizes
/// CRLF and lone LF line endings to CR, matching what pressing Enter sends.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
        for ch in text.chars().filter(|ch| !matches!(ch, '\x1b' | '\x03')) {
            push_char(&mut out, ch);
        }
        out.extend_from_slice(b"\x1b[201~");
        return out;
    }

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                out.push(b'\r');
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\n' => out.push(b'\r'),
            ch => push_char(&mut out, ch),
        }
    }
    out
}

fn push_char(out: &mut Vec<u8>, ch: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

/// Encode a mouse report, either SGR or legacy X10.
pub fn encode_mouse(
    phase: MousePhase,
    button: MouseButton,
    col: usize,
    row: usize,
    mods: Mods,
    sgr: bool,
) -> Vec<u8> {
    let bits = mouse_mod_bits(mods);
    let base = mouse_button_code(button);
    let motion = if phase == MousePhase::Motion { 32 } else { 0 };

    if sgr {
        let code = if phase == MousePhase::Release {
            base + bits
        } else {
            base + bits + motion
        };
        let suffix = if phase == MousePhase::Release {
            'm'
        } else {
            'M'
        };
        format!(
            "\x1b[<{code};{};{}{suffix}",
            col.saturating_add(1),
            row.saturating_add(1)
        )
        .into_bytes()
    } else {
        // X10 has no separate button-less motion code: a real release is button code 3,
        // while motion reports carry the motion bit on top of the base button code.
        let code = if phase == MousePhase::Release {
            3 + bits
        } else {
            base + bits + motion
        };
        vec![ESC, b'[', b'M', 32 + code, coord(col), coord(row)]
    }
}

fn single(byte: u8, alt: bool) -> Vec<u8> {
    if alt { vec![ESC, byte] } else { vec![byte] }
}

fn encode_char(c: char, text: Option<&str>, mods: Mods) -> Vec<u8> {
    if mods.ctrl
        && let Some(byte) = ctrl_byte(c)
    {
        return single(byte, mods.alt);
    }

    let payload = match text.filter(|text| !text.is_empty()) {
        Some(text) => text.as_bytes().to_vec(),
        None => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
    };
    if mods.alt {
        let mut out = Vec::with_capacity(payload.len() + 1);
        out.push(ESC);
        out.extend_from_slice(&payload);
        out
    } else {
        payload
    }
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        '@' | ' ' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '2' => Some(0x00),
        '6' => Some(0x1e),
        '-' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn modifier_param(mods: Mods) -> Option<u8> {
    let any = mods.shift || mods.ctrl || mods.alt;
    any.then(|| 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl))
}

fn csi_modified(final_byte: u8, param: u8) -> Vec<u8> {
    format!("\x1b[1;{param}{}", char::from(final_byte)).into_bytes()
}

fn csi_simple(final_byte: u8, mods: Mods) -> Vec<u8> {
    match modifier_param(mods) {
        Some(param) => csi_modified(final_byte, param),
        None => vec![ESC, b'[', final_byte],
    }
}

fn arrow(final_byte: u8, mods: Mods, app_cursor: bool) -> Vec<u8> {
    match modifier_param(mods) {
        Some(param) => csi_modified(final_byte, param),
        None if app_cursor => vec![ESC, b'O', final_byte],
        None => vec![ESC, b'[', final_byte],
    }
}

fn tilde(code: u8, mods: Mods) -> Vec<u8> {
    match modifier_param(mods) {
        Some(param) => format!("\x1b[{code};{param}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

fn function_ss3(final_byte: u8, mods: Mods) -> Vec<u8> {
    match modifier_param(mods) {
        Some(param) => csi_modified(final_byte, param),
        None => vec![ESC, b'O', final_byte],
    }
}

fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::None => 3,
    }
}

fn mouse_mod_bits(mods: Mods) -> u8 {
    u8::from(mods.shift) * 4 + u8::from(mods.alt) * 8 + u8::from(mods.ctrl) * 16
}

fn coord(value: usize) -> u8 {
    u8::try_from(value.saturating_add(33)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
    };
    const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
    };
    const ALT: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: true,
    };
    const CTRL_SHIFT: Mods = Mods {
        shift: true,
        ctrl: true,
        alt: false,
    };
    const CTRL_ALT: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: true,
    };
    const SHIFT_ALT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: true,
    };

    fn key(key: Key, mods: Mods) -> KeyInput {
        KeyInput::new(key, mods, None)
    }

    fn typed(key: Key, text: &str, mods: Mods) -> KeyInput {
        KeyInput::new(key, mods, Some(text.to_owned()))
    }

    fn plain() -> KeyModes {
        KeyModes::default()
    }

    fn app_cursor() -> KeyModes {
        KeyModes {
            app_cursor: true,
            ..KeyModes::default()
        }
    }

    type MouseCase = (MousePhase, MouseButton, usize, usize, Mods, &'static [u8]);

    #[test]
    fn test_ctrl_letters() {
        let letters: [(char, u8); 26] = [
            ('a', 0x01),
            ('b', 0x02),
            ('c', 0x03),
            ('d', 0x04),
            ('e', 0x05),
            ('f', 0x06),
            ('g', 0x07),
            ('h', 0x08),
            ('i', 0x09),
            ('j', 0x0a),
            ('k', 0x0b),
            ('l', 0x0c),
            ('m', 0x0d),
            ('n', 0x0e),
            ('o', 0x0f),
            ('p', 0x10),
            ('q', 0x11),
            ('r', 0x12),
            ('s', 0x13),
            ('t', 0x14),
            ('u', 0x15),
            ('v', 0x16),
            ('w', 0x17),
            ('x', 0x18),
            ('y', 0x19),
            ('z', 0x1a),
        ];

        for (c, byte) in letters {
            assert_eq!(
                encode_key(key(Key::Char(c), CTRL), plain()),
                vec![byte],
                "Ctrl+{c}"
            );
            assert_eq!(
                encode_key(key(Key::Char(c.to_ascii_uppercase()), CTRL), plain()),
                vec![byte],
                "Ctrl+{}",
                c.to_ascii_uppercase()
            );
        }

        assert_eq!(
            encode_key(key(Key::Char('c'), CTRL_SHIFT), plain()),
            vec![0x03],
            "Ctrl+Shift+letter behaves like Ctrl+letter"
        );
    }

    #[test]
    fn test_ctrl_symbols() {
        let cases: [(Key, u8); 11] = [
            (Key::Char('@'), 0x00),
            (Key::Space, 0x00),
            (Key::Char('2'), 0x00),
            (Key::Char('6'), 0x1e),
            (Key::Char('-'), 0x1f),
            (Key::Char('['), 0x1b),
            (Key::Char('\\'), 0x1c),
            (Key::Char(']'), 0x1d),
            (Key::Char('^'), 0x1e),
            (Key::Char('_'), 0x1f),
            (Key::Char('?'), 0x7f),
        ];

        for (k, byte) in cases {
            assert_eq!(encode_key(key(k, CTRL), plain()), vec![byte], "Ctrl+{k:?}");
        }
    }

    #[test]
    fn test_text_passthrough() {
        let cases: Vec<(KeyInput, &[u8])> = vec![
            (typed(Key::Char('a'), "a", NONE), b"a"),
            (typed(Key::Char('A'), "A", SHIFT), b"A"),
            (typed(Key::Char('a'), "ab", NONE), b"ab"),
            (typed(Key::Char('é'), "é", NONE), b"\xc3\xa9"),
            (typed(Key::Char('x'), "", NONE), b"x"),
            (key(Key::Char('x'), NONE), b"x"),
            (typed(Key::Char('a'), "ignored", CTRL), b"\x01"),
            (typed(Key::Enter, "ignored", NONE), b"\r"),
            (typed(Key::Tab, "ignored", NONE), b"\t"),
            (typed(Key::Backspace, "ignored", NONE), b"\x7f"),
            (typed(Key::Escape, "ignored", NONE), b"\x1b"),
        ];

        for (input, expected) in cases {
            assert_eq!(encode_key(input, plain()), expected);
        }
    }

    #[test]
    fn test_control_keys() {
        let cases: [(KeyInput, &[u8]); 7] = [
            (key(Key::Enter, NONE), b"\r"),
            (key(Key::Tab, NONE), b"\t"),
            (key(Key::Backspace, NONE), b"\x7f"),
            (key(Key::Escape, NONE), b"\x1b"),
            (key(Key::Space, NONE), b" "),
            (key(Key::Enter, CTRL), b"\r"),
            (key(Key::Enter, ALT), b"\x1b\r"),
        ];

        for (input, expected) in cases {
            assert_eq!(encode_key(input, plain()), expected);
        }
    }

    #[test]
    fn test_shift_tab() {
        assert_eq!(encode_key(key(Key::Tab, SHIFT), plain()), b"\x1b[Z");
        assert_eq!(
            encode_key(typed(Key::Tab, "ignored", SHIFT), plain()),
            b"\x1b[Z"
        );
    }

    #[test]
    fn test_alt_prefix() {
        let cases: [(KeyInput, &[u8]); 7] = [
            (key(Key::Char('f'), ALT), b"\x1bf"),
            (typed(Key::Char('f'), "f", ALT), b"\x1bf"),
            (typed(Key::Char('A'), "A", SHIFT_ALT), b"\x1bA"),
            (key(Key::Char('1'), ALT), b"\x1b1"),
            (key(Key::Space, ALT), b"\x1b "),
            (key(Key::Char('a'), ALT), b"\x1ba"),
            (typed(Key::Char('é'), "é", ALT), b"\x1b\xc3\xa9"),
        ];

        for (input, expected) in cases {
            assert_eq!(encode_key(input, plain()), expected);
        }
    }

    #[test]
    fn test_ctrl_alt() {
        let cases: [(KeyInput, &[u8]); 4] = [
            (key(Key::Char('a'), CTRL_ALT), b"\x1b\x01"),
            (key(Key::Char('['), CTRL_ALT), b"\x1b\x1b"),
            (key(Key::Space, CTRL_ALT), b"\x1b\x00"),
            (key(Key::Char('1'), CTRL_ALT), b"\x1b1"),
        ];

        for (input, expected) in cases {
            assert_eq!(encode_key(input, plain()), expected);
        }
    }

    #[test]
    fn test_arrows() {
        let cases: [(KeyInput, KeyModes, &[u8]); 11] = [
            (key(Key::Up, NONE), plain(), b"\x1b[A"),
            (key(Key::Down, NONE), app_cursor(), b"\x1bOB"),
            (key(Key::Right, NONE), app_cursor(), b"\x1bOC"),
            (key(Key::Left, NONE), plain(), b"\x1b[D"),
            (key(Key::Up, CTRL), plain(), b"\x1b[1;5A"),
            (key(Key::Down, CTRL), app_cursor(), b"\x1b[1;5B"),
            (key(Key::Right, SHIFT), plain(), b"\x1b[1;2C"),
            (key(Key::Left, SHIFT_ALT), plain(), b"\x1b[1;4D"),
            (key(Key::Up, ALT), plain(), b"\x1b[1;3A"),
            (key(Key::Down, CTRL_ALT), plain(), b"\x1b[1;7B"),
            (key(Key::Up, CTRL_SHIFT), plain(), b"\x1b[1;6A"),
        ];

        for (input, modes, expected) in cases {
            assert_eq!(encode_key(input, modes), expected);
        }
    }

    #[test]
    fn test_nav() {
        let cases: [(KeyInput, &[u8]); 13] = [
            (key(Key::Home, NONE), b"\x1b[H"),
            (key(Key::End, NONE), b"\x1b[F"),
            (key(Key::Home, CTRL), b"\x1b[1;5H"),
            (key(Key::End, ALT), b"\x1b[1;3F"),
            (key(Key::Home, SHIFT_ALT), b"\x1b[1;4H"),
            (key(Key::Insert, NONE), b"\x1b[2~"),
            (key(Key::Delete, NONE), b"\x1b[3~"),
            (key(Key::PageUp, NONE), b"\x1b[5~"),
            (key(Key::PageDown, NONE), b"\x1b[6~"),
            (key(Key::Delete, ALT), b"\x1b[3;3~"),
            (key(Key::PageUp, CTRL_SHIFT), b"\x1b[5;6~"),
            (key(Key::PageDown, CTRL_ALT), b"\x1b[6;7~"),
            (key(Key::Insert, SHIFT_ALT), b"\x1b[2;4~"),
        ];

        for (input, expected) in cases {
            assert_eq!(encode_key(input, plain()), expected);
        }
    }

    #[test]
    fn test_function_keys() {
        let cases: [(KeyInput, KeyModes, &[u8]); 21] = [
            (key(Key::F1, NONE), plain(), b"\x1bOP"),
            (key(Key::F2, NONE), plain(), b"\x1bOQ"),
            (key(Key::F3, NONE), plain(), b"\x1bOR"),
            (key(Key::F4, NONE), plain(), b"\x1bOS"),
            (key(Key::F5, NONE), plain(), b"\x1b[15~"),
            (key(Key::F6, NONE), plain(), b"\x1b[17~"),
            (key(Key::F7, NONE), plain(), b"\x1b[18~"),
            (key(Key::F8, NONE), plain(), b"\x1b[19~"),
            (key(Key::F9, NONE), plain(), b"\x1b[20~"),
            (key(Key::F10, NONE), plain(), b"\x1b[21~"),
            (key(Key::F11, NONE), plain(), b"\x1b[23~"),
            (key(Key::F12, NONE), plain(), b"\x1b[24~"),
            (key(Key::F1, CTRL), plain(), b"\x1b[1;5P"),
            (key(Key::F2, SHIFT_ALT), plain(), b"\x1b[1;4Q"),
            (key(Key::F4, SHIFT_ALT), plain(), b"\x1b[1;4S"),
            (key(Key::F5, CTRL), plain(), b"\x1b[15;5~"),
            (key(Key::F12, SHIFT), plain(), b"\x1b[24;2~"),
            (key(Key::F1, NONE), app_cursor(), b"\x1bOP"),
            (key(Key::F6, CTRL_ALT), plain(), b"\x1b[17;7~"),
            (key(Key::F11, ALT), plain(), b"\x1b[23;3~"),
            (key(Key::F3, CTRL_SHIFT), plain(), b"\x1b[1;6R"),
        ];

        for (input, modes, expected) in cases {
            assert_eq!(encode_key(input, modes), expected);
        }
    }

    #[test]
    fn test_mouse_sgr() {
        let cases: [MouseCase; 8] = [
            (
                MousePhase::Press,
                MouseButton::Left,
                2,
                3,
                NONE,
                b"\x1b[<0;3;4M",
            ),
            (
                MousePhase::Release,
                MouseButton::Left,
                2,
                3,
                NONE,
                b"\x1b[<0;3;4m",
            ),
            (
                MousePhase::Press,
                MouseButton::Middle,
                0,
                0,
                NONE,
                b"\x1b[<1;1;1M",
            ),
            (
                MousePhase::Press,
                MouseButton::Right,
                9,
                19,
                CTRL_SHIFT,
                b"\x1b[<22;10;20M",
            ),
            (
                MousePhase::Motion,
                MouseButton::Left,
                0,
                0,
                NONE,
                b"\x1b[<32;1;1M",
            ),
            (
                MousePhase::Press,
                MouseButton::WheelUp,
                5,
                5,
                NONE,
                b"\x1b[<64;6;6M",
            ),
            (
                MousePhase::Press,
                MouseButton::WheelDown,
                2,
                3,
                ALT,
                b"\x1b[<73;3;4M",
            ),
            (
                MousePhase::Motion,
                MouseButton::Right,
                1,
                1,
                CTRL_ALT,
                b"\x1b[<58;2;2M",
            ),
        ];

        for (phase, button, col, row, mods, expected) in cases {
            assert_eq!(encode_mouse(phase, button, col, row, mods, true), expected);
        }
    }

    #[test]
    fn test_mouse_normal() {
        let cases: [MouseCase; 7] = [
            (
                MousePhase::Press,
                MouseButton::Left,
                0,
                0,
                NONE,
                b"\x1b[M !!",
            ),
            (
                MousePhase::Release,
                MouseButton::Left,
                2,
                3,
                NONE,
                b"\x1b[M##$",
            ),
            (
                MousePhase::Motion,
                MouseButton::Right,
                1,
                1,
                SHIFT,
                b"\x1b[MF\"\"",
            ),
            (
                MousePhase::Press,
                MouseButton::WheelUp,
                0,
                0,
                NONE,
                b"\x1b[M`!!",
            ),
            (
                MousePhase::Press,
                MouseButton::WheelDown,
                0,
                0,
                NONE,
                b"\x1b[Ma!!",
            ),
            (
                MousePhase::Motion,
                MouseButton::Left,
                0,
                0,
                CTRL_ALT,
                b"\x1b[MX!!",
            ),
            (
                MousePhase::Release,
                MouseButton::Left,
                0,
                0,
                ALT,
                b"\x1b[M+!!",
            ),
        ];

        for (phase, button, col, row, mods, expected) in cases {
            assert_eq!(encode_mouse(phase, button, col, row, mods, false), expected);
        }
    }

    #[test]
    fn shift_enter_sends_esc_cr() {
        let mods = Mods {
            shift: true,
            ..Mods::default()
        };
        let bytes = encode_key(KeyInput::new(Key::Enter, mods, None), KeyModes::default());
        assert_eq!(bytes, vec![0x1b, 0x0d]);
    }

    #[test]
    fn shift_enter_without_newline_setting_sends_plain_cr() {
        let plain_cr = KeyModes {
            shift_enter_newline: false,
            ..KeyModes::default()
        };
        assert_eq!(encode_key(key(Key::Enter, SHIFT), plain_cr), vec![0x0d]);
        assert_eq!(
            encode_key(key(Key::Enter, SHIFT_ALT), plain_cr),
            vec![0x1b, 0x0d],
            "Alt still prefixes ESC when Shift+Enter is plain CR"
        );
        assert_eq!(
            encode_key(key(Key::Enter, CTRL_SHIFT), KeyModes::default()),
            vec![0x0d],
            "Ctrl+Shift+Enter stays CR with the setting on"
        );
        assert_eq!(
            encode_key(key(Key::Enter, CTRL_SHIFT), plain_cr),
            vec![0x0d],
            "Ctrl+Shift+Enter stays CR with the setting off"
        );
    }

    #[test]
    fn shift_alt_enter_sends_one_esc() {
        let bytes = encode_key(key(Key::Enter, SHIFT_ALT), plain());
        assert_eq!(bytes, vec![0x1b, 0x0d]);
        assert_eq!(
            bytes.iter().filter(|byte| **byte == ESC).count(),
            1,
            "Shift+Alt+Enter must not emit two ESCs"
        );
    }

    #[test]
    fn enter_still_sends_cr() {
        assert_eq!(encode_key(key(Key::Enter, NONE), plain()), vec![0x0d]);
        assert_eq!(encode_key(key(Key::Enter, ALT), plain()), vec![0x1b, 0x0d]);
    }

    #[test]
    fn ctrl_enter_unchanged() {
        assert_eq!(encode_key(key(Key::Enter, CTRL), plain()), vec![0x0d]);
        assert_eq!(
            encode_key(key(Key::Enter, CTRL_ALT), plain()),
            vec![0x1b, 0x0d]
        );
        assert_eq!(
            encode_key(key(Key::Enter, CTRL_SHIFT), plain()),
            vec![0x0d],
            "Ctrl+Shift+Enter must keep sending CR"
        );
        assert_eq!(
            encode_key(
                key(
                    Key::Enter,
                    Mods {
                        alt: true,
                        ..CTRL_SHIFT
                    }
                ),
                plain()
            ),
            vec![0x1b, 0x0d],
            "Ctrl+Shift+Alt+Enter must keep sending ESC + CR"
        );
    }

    #[test]
    fn motion_without_button_sgr_is_code_35() {
        let bytes = encode_mouse(
            MousePhase::Motion,
            MouseButton::None,
            4,
            2,
            Mods::default(),
            true,
        );
        assert_eq!(bytes, b"\x1b[<35;5;3M");
    }

    #[test]
    fn motion_without_button_legacy_sets_the_motion_bit() {
        let bytes = encode_mouse(
            MousePhase::Motion,
            MouseButton::None,
            4,
            2,
            Mods::default(),
            false,
        );
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 67, 37, 35]);
    }

    #[test]
    fn release_without_button_emits_code_3_and_mods() {
        assert_eq!(
            encode_mouse(
                MousePhase::Release,
                MouseButton::None,
                4,
                2,
                Mods::default(),
                true
            ),
            b"\x1b[<3;5;3m"
        );
        assert_eq!(
            encode_mouse(
                MousePhase::Release,
                MouseButton::None,
                4,
                2,
                SHIFT_ALT,
                true
            ),
            b"\x1b[<15;5;3m"
        );
        assert_eq!(
            encode_mouse(
                MousePhase::Release,
                MouseButton::None,
                4,
                2,
                Mods::default(),
                false
            ),
            vec![0x1b, b'[', b'M', 35, 37, 35]
        );
    }

    #[test]
    fn test_paste() {
        assert_eq!(encode_paste("hello", false), b"hello");
        assert_eq!(encode_paste("hello", true), b"\x1b[200~hello\x1b[201~");
        assert_eq!(encode_paste("a\r\nb", true), b"\x1b[200~a\r\nb\x1b[201~");
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(encode_paste("a\r\nb", false), b"a\rb");
        assert_eq!(encode_paste("", true), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn paste_strips_escape_and_etx_when_bracketed() {
        assert_eq!(
            encode_paste("a\x1b[201~evil\x03b", true),
            b"\x1b[200~a[201~evilb\x1b[201~"
        );
        assert_eq!(
            encode_paste("a\x1bb", false),
            b"a\x1bb",
            "unbracketed paste keeps ESC"
        );
    }

    #[test]
    fn test_from_char() {
        let cases: [(char, Key); 9] = [
            ('\r', Key::Enter),
            ('\n', Key::Enter),
            ('\t', Key::Tab),
            ('\u{8}', Key::Backspace),
            ('\u{7f}', Key::Backspace),
            ('\u{1b}', Key::Escape),
            (' ', Key::Space),
            ('a', Key::Char('a')),
            ('é', Key::Char('é')),
        ];

        for (c, expected) in cases {
            assert_eq!(Key::from_char(c), expected);
        }
    }

    #[test]
    fn test_key_input_new() {
        let input = KeyInput::new(Key::Char('a'), CTRL_ALT, Some("a".to_owned()));
        assert_eq!(input.key, Key::Char('a'));
        assert_eq!(input.mods, CTRL_ALT);
        assert_eq!(input.text.as_deref(), Some("a"));
        assert!(!input.repeat);

        let mut repeated = KeyInput::new(Key::Char('a'), NONE, Some("a".to_owned()));
        repeated.repeat = true;
        assert_eq!(encode_key(repeated, plain()), b"a");
    }
}

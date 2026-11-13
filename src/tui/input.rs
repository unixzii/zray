//! The search box as raw material: how the query wraps into rows, and how
//! bytes typed ahead turn into keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// The query broken into terminal rows, and where the cursor lands.
pub(super) struct InputLayout {
    pub(super) rows: Vec<String>,
    /// (row, column) of the cursor.
    pub(super) cursor: (usize, usize),
}

/// Wraps `query` the way zsh wraps its edit buffer: text fills each row to
/// the last column, a wide character that does not fit moves to the next row
/// whole, and a cursor past a full row sits at the start of the next one.
/// The first row starts at `start_col`, after the prompt.
pub(super) fn layout_input(
    query: &str,
    cursor: usize,
    start_col: usize,
    width: usize,
) -> InputLayout {
    let mut rows = vec![String::new()];
    let mut col = start_col.min(width.saturating_sub(1));
    let mut cursor_at = None;
    for (i, c) in query.chars().enumerate() {
        let w = c.width().unwrap_or(0);
        if col + w > width && col > 0 {
            rows.push(String::new());
            col = 0;
        }
        if i == cursor {
            cursor_at = Some((rows.len() - 1, col));
        }
        rows.last_mut().expect("rows is never empty").push(c);
        col += w;
    }
    let cursor = cursor_at.unwrap_or_else(|| {
        if col >= width {
            rows.push(String::new());
            (rows.len() - 1, 0)
        } else {
            (rows.len() - 1, col)
        }
    });
    InputLayout { rows, cursor }
}

/// Keys typed before the event loop took over the terminal's input, decoded
/// from raw bytes. Covers what someone types ahead; other escape sequences
/// are dropped.
pub(super) fn decode_keys(bytes: &[u8]) -> Vec<KeyEvent> {
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (code, len) = match bytes[i..] {
            [0x1b, b'[' | b'O', b'A', ..] => (KeyCode::Up, 3),
            [0x1b, b'[' | b'O', b'B', ..] => (KeyCode::Down, 3),
            [0x1b, b'[' | b'O', b'C', ..] => (KeyCode::Right, 3),
            [0x1b, b'[' | b'O', b'D', ..] => (KeyCode::Left, 3),
            [0x1b, b'[' | b'O', ..] => {
                // Some other sequence: skip to its final byte.
                let end = bytes[i + 2..]
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b));
                i += 2 + end.map_or(bytes.len(), |e| e + 1);
                continue;
            }
            [0x1b, ..] => (KeyCode::Esc, 1),
            [b'\r' | b'\n', ..] => (KeyCode::Enter, 1),
            [b'\t', ..] => (KeyCode::Tab, 1),
            [0x7f | 0x08, ..] => (KeyCode::Backspace, 1),
            [b @ 0x01..=0x1a, ..] => {
                keys.push(KeyEvent::new(
                    KeyCode::Char((b'a' + b - 1) as char),
                    KeyModifiers::CONTROL,
                ));
                i += 1;
                continue;
            }
            _ => {
                // One UTF-8 character; its first byte gives its length.
                let len = match bytes[i] {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                let end = (i + len).min(bytes.len());
                match std::str::from_utf8(&bytes[i..end])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    Some(c) if !c.is_control() => (KeyCode::Char(c), end - i),
                    _ => {
                        i = end;
                        continue;
                    }
                }
            }
        };
        keys.push(key(code));
        i += len;
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_wraps_at_the_last_column() {
        // Width 6, prompt takes 2: "abcd" fills row 0, "ef" goes to row 1.
        let l = layout_input("abcdef", 6, 2, 6);
        assert_eq!(l.rows, ["abcd", "ef"]);
        assert_eq!(l.cursor, (1, 2));
        // A cursor after a full row sits at the start of the next row.
        let l = layout_input("abcd", 4, 2, 6);
        assert_eq!(l.rows, ["abcd", ""]);
        assert_eq!(l.cursor, (1, 0));
        // A cursor before the character that wrapped is where it landed.
        let l = layout_input("abcdef", 4, 2, 6);
        assert_eq!(l.cursor, (1, 0));
        assert_eq!(layout_input("ab", 1, 2, 6).cursor, (0, 3));
    }

    #[test]
    fn wide_char_moves_to_the_next_row_whole() {
        // Three columns are free after the prompt; the second "中" needs two
        // but only one is left.
        let l = layout_input("中中", 2, 2, 5);
        assert_eq!(l.rows, ["中", "中"]);
        assert_eq!(l.cursor, (1, 2));
    }

    #[test]
    fn typed_ahead_bytes_decode_to_keys() {
        let codes = |bytes: &[u8]| -> Vec<KeyCode> {
            decode_keys(bytes).into_iter().map(|k| k.code).collect()
        };
        assert_eq!(
            codes("g中\x1b[A\x1bOB\r\t\x7f".as_bytes()),
            [
                KeyCode::Char('g'),
                KeyCode::Char('中'),
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Enter,
                KeyCode::Tab,
                KeyCode::Backspace
            ]
        );
        assert_eq!(codes(b"\x1b"), [KeyCode::Esc]);
        // An unknown sequence is skipped whole, not typed as text.
        assert_eq!(codes(b"\x1b[1;5Hx"), [KeyCode::Char('x')]);
        let ctrl = decode_keys(b"\x10");
        assert_eq!(ctrl[0].code, KeyCode::Char('p'));
        assert!(ctrl[0].modifiers.contains(KeyModifiers::CONTROL));
    }
}

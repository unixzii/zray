//! Asking the terminal about itself: where the cursor is and what its
//! colors really are.
//!
//! Fading text in and out means mixing colors, and a theme color such as
//! "yellow" or "the default foreground" is only a number until the terminal
//! says what it looks like. Terminals that do not answer leave
//! [`Probe::colors`] empty and the caller goes without fades.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::style::Color;

pub type Rgb = (u8, u8, u8);

/// How long to wait for the terminal to answer.
const TIMEOUT: Duration = Duration::from_secs(1);

/// The colors a terminal draws with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermColors {
    pub foreground: Rgb,
    pub background: Rgb,
    /// The 16 ANSI colors, where the terminal reported them.
    pub palette: [Option<Rgb>; 16],
}

/// What the terminal said, plus anything the user typed meanwhile.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// Zero-based (column, row).
    pub cursor: (u16, u16),
    pub colors: Option<TermColors>,
    /// Bytes that arrived among the replies but are not part of one: keys
    /// pressed while we were waiting. They must not be dropped.
    pub typed: Vec<u8>,
}

/// xterm's default palette, for entries a terminal does not report.
const XTERM_16: [Rgb; 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

impl TermColors {
    fn ansi(&self, index: usize) -> Rgb {
        self.palette[index].unwrap_or(XTERM_16[index])
    }

    /// What `color` looks like. `default` is what [`Color::Reset`] stands
    /// for where the color is used: the foreground or the background.
    pub fn resolve(&self, color: Color, default: Rgb) -> Rgb {
        match color {
            Color::Reset => default,
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Black => self.ansi(0),
            Color::Red => self.ansi(1),
            Color::Green => self.ansi(2),
            Color::Yellow => self.ansi(3),
            Color::Blue => self.ansi(4),
            Color::Magenta => self.ansi(5),
            Color::Cyan => self.ansi(6),
            Color::Gray => self.ansi(7),
            Color::DarkGray => self.ansi(8),
            Color::LightRed => self.ansi(9),
            Color::LightGreen => self.ansi(10),
            Color::LightYellow => self.ansi(11),
            Color::LightBlue => self.ansi(12),
            Color::LightMagenta => self.ansi(13),
            Color::LightCyan => self.ansi(14),
            Color::White => self.ansi(15),
            Color::Indexed(i) => match i {
                0..16 => self.ansi(usize::from(i)),
                // The 6x6x6 color cube.
                16..232 => {
                    let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                    let i = i - 16;
                    (level(i / 36), level(i / 6 % 6), level(i % 6))
                }
                // The grayscale ramp.
                232..=255 => {
                    let v = 8 + 10 * (i - 232);
                    (v, v, v)
                }
            },
        }
    }
}

/// `from` at `t` = 0, `to` at `t` = 1.
pub fn mix(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

/// Whether the terminal can show arbitrary RGB colors, which a fade needs.
pub fn supports_rgb() -> bool {
    matches!(
        std::env::var("COLORTERM").as_deref(),
        Ok("truecolor" | "24bit")
    )
}

/// Asks for the cursor position and, if `with_colors`, the terminal's
/// colors. Stdin must be the terminal, in raw mode, and nothing else may be
/// reading it.
pub fn probe(with_colors: bool) -> io::Result<Probe> {
    let mut query = String::new();
    if with_colors {
        // Default foreground, default background, then the palette.
        query.push_str("\x1b]10;?\x07\x1b]11;?\x07");
        for i in 0..16 {
            query.push_str(&format!("\x1b]4;{i};?\x07"));
        }
    }
    // Asked last: every terminal answers it, and in order, so its reply
    // means no more replies are coming.
    query.push_str("\x1b[6n");
    let mut out = io::stdout();
    out.write_all(query.as_bytes())?;
    out.flush()?;

    let deadline = Instant::now() + TIMEOUT;
    let mut received = Vec::new();
    loop {
        if let Some(probe) = parse_replies(&received) {
            return Ok(probe);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wait_for_stdin(remaining)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the terminal did not report the cursor position",
            ));
        }
        let mut chunk = [0u8; 1024];
        // SAFETY: `chunk` is a valid buffer of the length passed.
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        match n {
            n if n > 0 => received.extend_from_slice(&chunk[..n as usize]),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            _ => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
}

/// Waits until stdin has input; `false` on timeout. Uses `select` because
/// macOS cannot `poll` terminal devices.
fn wait_for_stdin(timeout: Duration) -> io::Result<bool> {
    // SAFETY: `fds` is initialized by FD_ZERO before use and only fd 0,
    // which is below FD_SETSIZE, is added to it.
    unsafe {
        let mut fds: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut fds);
        libc::FD_SET(libc::STDIN_FILENO, &mut fds);
        let mut tv = libc::timeval {
            tv_sec: timeout.as_secs() as _,
            tv_usec: timeout.subsec_micros() as _,
        };
        let ready = libc::select(
            libc::STDIN_FILENO + 1,
            &mut fds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        );
        match ready {
            -1 => {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
            n => Ok(n > 0),
        }
    }
}

/// Splits what has been read so far into replies and typed bytes. `None`
/// until the cursor position report, which ends the replies, has arrived.
fn parse_replies(bytes: &[u8]) -> Option<Probe> {
    const ESC: u8 = 0x1b;
    let mut cursor = None;
    let mut foreground = None;
    let mut background = None;
    let mut palette = [None; 16];
    let mut typed = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if cursor.is_none() && rest.starts_with(b"\x1b]") {
            // OSC reply: ends with BEL or ESC \.
            let end = rest
                .iter()
                .position(|&b| b == 0x07)
                .map(|p| (p, 1))
                .into_iter()
                .chain(rest.windows(2).position(|w| w == b"\x1b\\").map(|p| (p, 2)));
            let Some((end, terminator)) = end.min() else {
                return None; // Not all here yet.
            };
            if let Ok(payload) = std::str::from_utf8(&rest[2..end]) {
                match payload.split_once(';') {
                    Some(("10", color)) => foreground = parse_rgb(color),
                    Some(("11", color)) => background = parse_rgb(color),
                    Some(("4", entry)) => {
                        if let Some((index, color)) = entry.split_once(';')
                            && let Ok(index) = index.parse::<usize>()
                            && index < 16
                        {
                            palette[index] = parse_rgb(color);
                        }
                    }
                    _ => {}
                }
            }
            i += end + terminator;
        } else if cursor.is_none() && rest.starts_with(b"\x1b[") {
            // Cursor position report: ESC [ row ; col R. Anything else that
            // starts this way is a key, such as an arrow.
            let body = &rest[2..];
            let len = body
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            match body.get(len) {
                None => return None, // Not all here yet.
                Some(b'R') => {
                    let text = std::str::from_utf8(&body[..len]).ok()?;
                    let (row, col) = text.split_once(';')?;
                    let (row, col): (u16, u16) = (row.parse().ok()?, col.parse().ok()?);
                    cursor = Some((col.saturating_sub(1), row.saturating_sub(1)));
                    i += 2 + len + 1;
                }
                Some(_) => {
                    typed.push(ESC);
                    i += 1;
                }
            }
        } else {
            typed.push(bytes[i]);
            i += 1;
        }
    }

    let colors = match (foreground, background) {
        (Some(foreground), Some(background)) => Some(TermColors {
            foreground,
            background,
            palette,
        }),
        _ => None,
    };
    Some(Probe {
        cursor: cursor?,
        colors,
        typed,
    })
}

/// Parses X11 color syntax as terminals report it: `rgb:RRRR/GGGG/BBBB`,
/// with one to four hex digits per channel.
fn parse_rgb(spec: &str) -> Option<Rgb> {
    let mut channels = spec.strip_prefix("rgb:")?.split('/').map(|c| {
        if c.is_empty() || c.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(c, 16).ok()?;
        let max = (1u32 << (4 * c.len())) - 1;
        Some((value * 255 / max) as u8)
    });
    let rgb = (channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_specs() {
        assert_eq!(parse_rgb("rgb:ffff/8080/0000"), Some((255, 128, 0)));
        assert_eq!(parse_rgb("rgb:ff/80/00"), Some((255, 128, 0)));
        assert_eq!(parse_rgb("rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_rgb("rgb:ff/80"), None);
        assert_eq!(parse_rgb("#ff8000"), None);
    }

    #[test]
    fn waits_for_the_cursor_report() {
        assert_eq!(parse_replies(b""), None);
        assert_eq!(parse_replies(b"\x1b]11;rgb:0000/0000/0000\x07"), None);
        assert_eq!(parse_replies(b"\x1b[12;"), None);
        assert_eq!(parse_replies(b"\x1b]11;rgb:00"), None);
    }

    #[test]
    fn cursor_only() {
        let probe = parse_replies(b"\x1b[12;5R").unwrap();
        assert_eq!(probe.cursor, (4, 11));
        assert_eq!(probe.colors, None);
        assert!(probe.typed.is_empty());
    }

    #[test]
    fn colors_with_either_terminator() {
        let probe = parse_replies(
            b"\x1b]10;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:1e1e/2020/3030\x1b\\\
              \x1b]4;3;rgb:ffff/cccc/0000\x07\x1b[1;1R",
        )
        .unwrap();
        let colors = probe.colors.unwrap();
        assert_eq!(colors.foreground, (255, 255, 255));
        assert_eq!(colors.background, (0x1e, 0x20, 0x30));
        assert_eq!(colors.palette[3], Some((255, 204, 0)));
        assert_eq!(colors.palette[4], None);
    }

    #[test]
    fn keys_typed_meanwhile_are_kept_in_order() {
        // "l", Up, then after the report "s" and Enter.
        let probe = parse_replies(b"l\x1b]11;rgb:00/00/00\x07\x1b[A\x1b[3;9Rs\r").unwrap();
        assert_eq!(probe.cursor, (8, 2));
        assert_eq!(probe.typed, b"l\x1b[As\r");
    }

    #[test]
    fn resolves_colors() {
        let mut palette = [None; 16];
        palette[3] = Some((1, 2, 3));
        let colors = TermColors {
            foreground: (200, 200, 200),
            background: (10, 10, 10),
            palette,
        };
        assert_eq!(
            colors.resolve(Color::Reset, colors.foreground),
            (200, 200, 200)
        );
        assert_eq!(colors.resolve(Color::Yellow, colors.foreground), (1, 2, 3));
        assert_eq!(colors.resolve(Color::Cyan, colors.foreground), XTERM_16[6]);
        assert_eq!(
            colors.resolve(Color::Indexed(3), colors.foreground),
            (1, 2, 3)
        );
        assert_eq!(
            colors.resolve(Color::Indexed(196), colors.foreground),
            (255, 0, 0)
        );
        assert_eq!(
            colors.resolve(Color::Indexed(244), colors.foreground),
            (128, 128, 128)
        );
        assert_eq!(
            colors.resolve(Color::Rgb(9, 8, 7), colors.foreground),
            (9, 8, 7)
        );
    }

    #[test]
    fn mixes() {
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 0.0), (0, 0, 0));
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 0.5), (100, 50, 25));
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 1.0), (200, 100, 50));
    }
}

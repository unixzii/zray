//! Inline-viewport search UI.
//!
//! The viewport starts on the row the shell's cursor is on (the last line of
//! the prompt) and extends downward. The tab bar is its last row, the input
//! line sits above that, and candidates stack upward from the input, best
//! match nearest it. Opening is animated: the prompt line slides down from
//! where the shell drew it to its place, then the rest fades in; closing
//! plays that backwards. On exit the
//! viewport is erased and the cursor put back where the shell left it, so the
//! shell can redraw its prompt in place.
//!
//! Requires stdin and stdout to be the terminal; the caller reports the
//! outcome through some other channel.

mod app;
mod input;
pub mod term;
pub mod theme;
mod transition;
mod view;

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{MoveTo, MoveUp, Show};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::matcher::Engine;
use app::{App, Step};
use input::{decode_keys, layout_input};
use theme::Theme;
use transition::FRAME;
use view::TICK;

pub struct Options {
    /// Initial contents of the search box.
    pub query: String,
    /// Initial cursor position in the query, in chars; the end if `None`.
    /// When the query comes from the shell's edit buffer this must be the
    /// shell's cursor, since it tells which screen row the terminal cursor
    /// is on when the buffer is wrapped.
    pub cursor: Option<usize>,
    /// The last line of the shell prompt, possibly with ANSI escapes.
    pub prompt: String,
    /// The most candidates to show at once. The viewport is this tall plus
    /// a row each for the input line and the tab bar; a query that wraps
    /// takes its extra rows from the candidates.
    pub max_items: u16,
    pub theme: Theme,
}

/// What can be searched. Commands are needed for the first frame;
/// directories are loaded only if their tab is opened.
pub struct Sources {
    pub commands: Engine,
    pub directories: Box<dyn FnOnce() -> Engine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run this command now.
    Execute(String),
    /// Put this text in the shell's edit buffer.
    Edit(String),
    /// Change to this directory.
    ChangeDirectory(String),
    /// Put this path in the shell's edit buffer, quoted as one word.
    EditPath(String),
    /// Leave the shell's buffer as it was.
    Cancel,
}

/// Turns on reporting of mouse buttons and the wheel, with coordinates in
/// the SGR encoding. Motion is deliberately left off: it would flood the
/// input, and whatever arrived after the search stopped reading would be
/// typed into the shell.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1000l";

/// Restores the tty mode when dropped, including on panic or early return.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("enabling raw mode")?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // The cursor is hidden during transitions; an error then must not
        // leave the shell without one.
        let _ = crossterm::execute!(io::stdout(), Show);
        let _ = io::stdout().write_all(DISABLE_MOUSE.as_bytes());
        let _ = io::stdout().flush();
        let _ = disable_raw_mode();
    }
}

pub fn run(sources: Sources, opts: Options) -> Result<Outcome> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("search needs stdin and stdout to be a terminal");
    }
    let _raw = RawMode::enable()?;

    let mut out = io::stdout();

    // The shell believes its cursor is here; it must be back here on exit.
    // The same round trip asks for the terminal's colors, if a fade could
    // use them.
    let probe = term::probe(term::supports_rgb()).context("querying the terminal")?;
    let (origin_col, _) = probe.cursor;
    let mut app = App::new(sources, &opts, probe.colors);
    // The viewport has to start on the prompt's row. If the shell's buffer
    // is wrapped, the cursor is some rows below that.
    let (width, _) = crossterm::terminal::size().context("querying terminal size")?;
    let rows_below_prompt = layout_input(
        &app.query,
        app.cursor,
        app.prompt.width(),
        usize::from(width.max(1)),
    )
    .cursor
    .0 as u16;
    if rows_below_prompt > 0 {
        crossterm::execute!(out, MoveUp(rows_below_prompt))?;
    }
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(opts.max_items.max(1).saturating_add(2)),
        },
    )?;
    // The viewport's first row still shows the shell's prompt line, which
    // ratatui's diffing would otherwise assume is blank.
    terminal.clear()?;
    out.write_all(ENABLE_MOUSE.as_bytes())?;
    out.flush()?;

    // Keys that arrived with the terminal's replies come first.
    for key in decode_keys(&probe.typed) {
        if let Step::Done(outcome) = app.on_key(key) {
            app.close(outcome);
            break;
        }
    }

    let outcome = loop {
        terminal.draw(|frame| app.draw(frame))?;
        if app.transition.closed() {
            break app.transition.outcome.take().unwrap_or(Outcome::Cancel);
        }
        if app.transition.closing() {
            // Input is left unread while closing: it is meant for the shell.
            std::thread::sleep(FRAME);
            continue;
        }
        // Block for input unless the frame just drawn is mid-animation.
        let wait = if app.transition.running() {
            Some(FRAME)
        } else if app.animating {
            Some(TICK)
        } else {
            None
        };
        if let Some(wait) = wait
            && !event::poll(wait)?
        {
            continue;
        }
        let step = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
            Event::Mouse(mouse) => {
                app.on_mouse(mouse);
                Step::Continue
            }
            Event::Paste(text) => {
                app.insert(&text);
                Step::Continue
            }
            _ => Step::Continue,
        };
        if let Step::Done(outcome) = step {
            app.close(outcome);
        }
    };

    // Making room may have scrolled the screen, so ask where the viewport
    // ended up rather than reusing the row from before.
    let top = terminal.get_frame().area().y;
    // Nothing is erased. The last frame of the closing transition shows only
    // the prompt line, back where the shell drew it and holding what the
    // shell's buffer is about to, so the shell redraws over the same thing
    // and the prompt never blinks out. (Erasing with `terminal.clear()`
    // would also ask the terminal for the cursor position and, reading the
    // reply, swallow whatever was typed for the shell meanwhile.)
    crossterm::execute!(out, MoveTo(origin_col, top + rows_below_prompt), Show)?;
    out.flush()?;
    Ok(outcome)
}

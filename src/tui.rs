//! Inline-viewport search UI.
//!
//! The viewport starts on the row the shell's cursor is on (the last line of
//! the prompt) and extends downward. The input line sits at the bottom and
//! candidates stack upward from it, best match nearest the input. On exit the
//! viewport is erased and the cursor put back where the shell left it, so the
//! shell can redraw its prompt in place.
//!
//! Requires stdin and stdout to be the terminal; the caller reports the
//! outcome through some other channel.

use std::io::{self, IsTerminal, Write};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

use crate::matcher::{Engine, Hit};

pub struct Options {
    /// Initial contents of the search box.
    pub query: String,
    /// The last line of the shell prompt, possibly with ANSI escapes.
    pub prompt: String,
    /// Total viewport height, input line included.
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run this command now.
    Execute(String),
    /// Put this text in the shell's edit buffer.
    Edit(String),
    /// Leave the shell's buffer as it was.
    Cancel,
}

/// What a key press asks the event loop to do.
enum Step {
    Continue,
    Done(Outcome),
}

struct App<'a> {
    engine: &'a mut Engine,
    prompt: Line<'static>,
    query: String,
    /// Cursor position in the query, in chars.
    cursor: usize,
    hits: Vec<Hit>,
    /// Index into `hits`; 0 is the best match, drawn nearest the input.
    selected: usize,
    /// Index into `hits` of the row drawn nearest the input.
    offset: usize,
}

impl<'a> App<'a> {
    fn new(engine: &'a mut Engine, opts: &Options) -> Self {
        let query: String = opts.query.lines().next().unwrap_or("").to_string();
        let mut app = App {
            engine,
            prompt: parse_prompt(&opts.prompt),
            cursor: query.chars().count(),
            query,
            hits: Vec::new(),
            selected: 0,
            offset: 0,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.hits = self.engine.search(&self.query);
        self.selected = 0;
        self.offset = 0;
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.query.char_indices().nth(char_index).map_or(self.query.len(), |(i, _)| i)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_index(self.cursor);
        let clean: String =
            text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.query.insert_str(at, &clean);
        self.cursor += clean.chars().count();
        self.refresh();
    }

    fn delete_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let (a, b) = (self.byte_index(from), self.byte_index(to));
        self.query.replace_range(a..b, "");
        self.cursor = from;
        self.refresh();
    }

    /// Start of the word before the cursor, for Ctrl-W.
    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.query.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn selected_text(&self) -> Option<String> {
        self.hits.get(self.selected).map(|h| self.engine.row(h.index).text.clone())
    }

    fn on_key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.query.chars().count();
        match key.code {
            KeyCode::Esc => {
                if self.query.is_empty() {
                    return Step::Done(Outcome::Cancel);
                }
                self.query.clear();
                self.cursor = 0;
                self.refresh();
            }
            KeyCode::Char('c' | 'g') if ctrl => return Step::Done(Outcome::Cancel),
            KeyCode::Char('d') if ctrl && self.query.is_empty() => {
                return Step::Done(Outcome::Cancel);
            }
            KeyCode::Enter | KeyCode::Char('j' | 'm') if ctrl || key.code == KeyCode::Enter => {
                // With nothing to pick, hand the typed text back for editing
                // rather than running something the user never saw matched.
                return Step::Done(match self.selected_text() {
                    Some(text) => Outcome::Execute(text),
                    None => Outcome::Edit(self.query.clone()),
                });
            }
            KeyCode::Tab => {
                return Step::Done(Outcome::Edit(
                    self.selected_text().unwrap_or_else(|| self.query.clone()),
                ));
            }
            KeyCode::Up => self.move_selection(1),
            KeyCode::Down => self.move_selection(-1),
            KeyCode::Char('p') if ctrl => self.move_selection(1),
            KeyCode::Char('n') if ctrl => self.move_selection(-1),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Backspace => self.delete_range(self.cursor.saturating_sub(1), self.cursor),
            KeyCode::Char('h') if ctrl => {
                self.delete_range(self.cursor.saturating_sub(1), self.cursor);
            }
            KeyCode::Delete => self.delete_range(self.cursor, (self.cursor + 1).min(len)),
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Char('u') if ctrl => self.delete_range(0, self.cursor),
            KeyCode::Char('k') if ctrl => self.delete_range(self.cursor, len),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            _ => {}
        }
        Step::Continue
    }

    /// Positive `delta` moves away from the input line, toward worse matches.
    fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let max = self.hits.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(max);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height == 0 || area.width == 0 {
            return;
        }
        let input_y = area.bottom() - 1;
        let list_rows = usize::from(area.height - 1);

        // Keep the selection inside the visible window.
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if list_rows > 0 && self.selected >= self.offset + list_rows {
            self.offset = self.selected + 1 - list_rows;
        }

        let prompt_width = self.prompt.width() as u16;
        let indent = if prompt_width == 0 { 2 } else { prompt_width };
        let now = self.engine.now_ms();

        for row in 0..list_rows {
            let hit_index = self.offset + row;
            let Some(hit) = self.hits.get(hit_index).copied() else { break };
            let y = input_y - 1 - row as u16;
            let selected = hit_index == self.selected;
            let positions = self.engine.match_positions(&self.query, hit.index);
            let command = self.engine.row(hit.index);

            let base = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let matched = base.fg(Color::Yellow);
            let marker = if selected { "▌" } else { " " };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::raw(" ".repeat(usize::from(indent.saturating_sub(1)))),
            ];
            let mut text_width = 0usize;
            let mut next_match = positions.iter().copied().peekable();
            for (i, c) in command.text.chars().enumerate() {
                let c = match c {
                    '\n' => '↵',
                    c if c.is_control() => ' ',
                    c => c,
                };
                let is_match = next_match.next_if_eq(&(i as u32)).is_some();
                text_width += c.width().unwrap_or(0);
                spans.push(Span::styled(c.to_string(), if is_match { matched } else { base }));
            }
            frame.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));

            // Age on the right, only when it would not crowd the command.
            let age = format_age(now - command.last_used_at);
            let age_width = age.chars().count() as u16;
            let needed = usize::from(indent) + text_width + 2 + usize::from(age_width);
            if needed <= usize::from(area.width) {
                frame.render_widget(
                    Span::styled(age, Style::default().fg(Color::DarkGray)),
                    Rect::new(area.right() - age_width, y, age_width, 1),
                );
            }
        }

        let mut input = self.prompt.clone();
        input.spans.push(Span::raw(self.query.clone()));
        frame.render_widget(input, Rect::new(area.x, input_y, area.width, 1));

        let before_cursor: usize =
            self.query.chars().take(self.cursor).map(|c| c.width().unwrap_or(0)).sum();
        let x = (usize::from(area.x) + usize::from(prompt_width) + before_cursor)
            .min(usize::from(area.right().saturating_sub(1)));
        frame.set_cursor_position((x as u16, input_y));
    }
}

/// Turns the shell's prompt line into styled spans.
fn parse_prompt(prompt: &str) -> Line<'static> {
    let last_line = prompt.rsplit('\n').next().unwrap_or("");
    match last_line.as_bytes().into_text() {
        Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
        Err(_) => Line::raw(last_line.to_string()),
    }
}

fn format_age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        86_400..604_800 => format!("{}d", s / 86_400),
        604_800..5_184_000 => format!("{}w", s / 604_800),
        5_184_000..31_536_000 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

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
        let _ = disable_raw_mode();
    }
}

pub fn run(engine: &mut Engine, opts: Options) -> Result<Outcome> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("search needs stdin and stdout to be a terminal");
    }
    let _raw = RawMode::enable()?;

    // The shell believes its cursor is here; it must be back here on exit.
    let (origin_col, _) = crossterm::cursor::position().context("querying cursor position")?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions { viewport: Viewport::Inline(opts.height.max(2)) },
    )?;
    // The viewport's first row still shows the shell's prompt line, which
    // ratatui's diffing would otherwise assume is blank.
    terminal.clear()?;

    let mut app = App::new(engine, &opts);
    let outcome = loop {
        terminal.draw(|frame| app.draw(frame))?;
        let step = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
            Event::Paste(text) => {
                app.insert(&text);
                Step::Continue
            }
            _ => Step::Continue,
        };
        if let Step::Done(outcome) = step {
            break outcome;
        }
    };

    // Making room may have scrolled the screen, so ask where the viewport
    // ended up rather than reusing the row from before.
    let top = terminal.get_frame().area().y;
    terminal.clear()?;
    let mut out = io::stdout();
    crossterm::execute!(out, MoveTo(origin_col, top), Show)?;
    out.flush()?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_keeps_text_and_drops_escapes() {
        let line = parse_prompt("dir on main\n\x1b[1;32m❯\x1b[0m ");
        assert_eq!(line.width(), 2);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "❯ ");
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(5_000), "now");
        assert_eq!(format_age(90_000), "1m");
        assert_eq!(format_age(3 * 86_400_000), "3d");
        assert_eq!(format_age(400 * 86_400_000), "1y");
    }
}

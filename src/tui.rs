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

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use crossterm::cursor::{MoveTo, MoveUp, Show};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

use crate::matcher::{Engine, Hit};
use crate::term::{self, TermColors};
use crate::theme::Theme;

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

/// How long an overflowing selected row rests at its start before scrolling.
const MARQUEE_PAUSE: Duration = Duration::from_millis(800);
/// Time per column while the selected row scrolls.
const MARQUEE_STEP: Duration = Duration::from_millis(120);
/// Blank columns between the end of the text and its repetition.
const MARQUEE_GAP: usize = 4;
/// Redraw interval while something is animating.
const TICK: Duration = Duration::from_millis(40);

/// Opening: how long the prompt line takes to slide into place.
const SLIDE: Duration = Duration::from_millis(150);
/// Opening: how long the list and tab bar then take to fade in.
const FADE: Duration = Duration::from_millis(100);
/// Redraw interval during the opening and closing transitions.
const FRAME: Duration = Duration::from_millis(12);

/// Where the opening transition stands. Closing runs it backwards from
/// wherever it is, so closing right after opening costs no more time than
/// the opening got to take.
struct Transition {
    /// Time into the opening: the slide first, then the fade.
    elapsed: Duration,
    last_tick: Instant,
    /// Set once the user is done: what to report when the closing ends.
    outcome: Option<Outcome>,
    /// Without the terminal's colors there is nothing to mix, so the fade
    /// is skipped and the list appears as the slide ends.
    fade: Duration,
}

impl Transition {
    fn new(can_fade: bool) -> Self {
        Transition {
            elapsed: Duration::ZERO,
            last_tick: Instant::now(),
            outcome: None,
            fade: if can_fade { FADE } else { Duration::ZERO },
        }
    }

    fn advance(&mut self) {
        let now = Instant::now();
        let step = now - self.last_tick;
        self.last_tick = now;
        self.elapsed = if self.outcome.is_some() {
            self.elapsed.saturating_sub(step)
        } else {
            (self.elapsed + step).min(SLIDE + self.fade)
        };
    }

    fn closing(&self) -> bool {
        self.outcome.is_some()
    }

    /// Still moving, in either direction.
    fn running(&self) -> bool {
        self.closing() || self.elapsed < SLIDE + self.fade
    }

    fn closed(&self) -> bool {
        self.closing() && self.elapsed.is_zero()
    }

    /// 0 with the prompt line where the shell drew it, 1 with it in place.
    /// Eased so it decelerates into place.
    fn slide(&self) -> f32 {
        let t = (self.elapsed.as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        1.0 - (1.0 - t).powi(2)
    }

    /// Opacity of everything but the prompt line.
    fn alpha(&self) -> f32 {
        match self.elapsed.checked_sub(SLIDE) {
            None => 0.0,
            Some(_) if self.fade.is_zero() => 1.0,
            Some(into_fade) => (into_fade.as_secs_f32() / self.fade.as_secs_f32()).min(1.0),
        }
    }
}

/// Turns on reporting of mouse buttons and the wheel, with coordinates in
/// the SGR encoding. Motion is deliberately left off: it would flood the
/// input, and whatever arrived after the search stopped reading would be
/// typed into the shell.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1000l";

/// Where things were drawn in the last frame, for finding what the mouse
/// is pointing at.
#[derive(Default, Clone, Copy)]
struct Geometry {
    /// Screen row of the candidate nearest the input.
    first_row_y: u16,
    /// How many candidates fit.
    list_rows: usize,
    /// Screen row of the tab bar, if it is showing.
    tab_y: Option<u16>,
}

/// What a key press asks the event loop to do.
enum Step {
    Continue,
    Done(Outcome),
}

/// The lists the search box can show, switched with Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Commands,
    Directories,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Commands, Tab::Directories];

    fn label(self) -> &'static str {
        match self {
            Tab::Commands => "commands",
            Tab::Directories => "directories",
        }
    }

    fn next(self) -> Tab {
        match self {
            Tab::Commands => Tab::Directories,
            Tab::Directories => Tab::Commands,
        }
    }
}

/// One tab's candidates and where the user is in them.
struct List {
    engine: Engine,
    hits: Vec<Hit>,
    /// Index into `hits`; 0 is the best match, drawn nearest the input.
    selected: usize,
    /// Index into `hits` of the row drawn nearest the input.
    offset: usize,
    /// The query changed while this tab was not showing.
    stale: bool,
}

impl List {
    fn new(engine: Engine) -> Self {
        List {
            engine,
            hits: Vec::new(),
            selected: 0,
            offset: 0,
            stale: true,
        }
    }

    fn search(&mut self, query: &str) {
        self.hits = self.engine.search(query);
        self.selected = 0;
        self.offset = 0;
        self.stale = false;
    }
}

struct App {
    commands: List,
    /// `None` until the tab is first shown.
    directories: Option<List>,
    load_directories: Option<Box<dyn FnOnce() -> Engine>>,
    tab: Tab,
    theme: Theme,
    prompt: Line<'static>,
    /// Shared by all tabs.
    query: String,
    /// The query the search was opened with; Esc returns to it.
    initial_query: String,
    /// While closing, what the shell's buffer is about to hold. Shown in
    /// place of the query so the line does not change as the shell takes over.
    closing_text: Option<String>,
    transition: Transition,
    /// The terminal's colors, if it reported them; needed for fading.
    colors: Option<TermColors>,
    /// Cursor position in the query, in chars.
    cursor: usize,
    /// When the current row became selected; the marquee's clock.
    selected_at: Instant,
    /// Whether the last frame needs redrawing on a timer.
    animating: bool,
    geometry: Geometry,
}

impl App {
    fn new(sources: Sources, opts: &Options, colors: Option<TermColors>) -> Self {
        let query: String = opts.query.lines().next().unwrap_or("").to_string();
        let len = query.chars().count();
        let mut app = App {
            commands: List::new(sources.commands),
            directories: None,
            load_directories: Some(sources.directories),
            tab: Tab::Commands,
            theme: opts.theme.clone(),
            prompt: parse_prompt(&opts.prompt),
            cursor: opts.cursor.map_or(len, |c| c.min(len)),
            initial_query: query.clone(),
            query,
            closing_text: None,
            transition: Transition::new(colors.is_some()),
            colors,
            selected_at: Instant::now(),
            animating: false,
            geometry: Geometry::default(),
        };
        app.refresh();
        app
    }

    /// The list of the tab being shown, loading it if this is its first use.
    fn list(&mut self) -> &mut List {
        match self.tab {
            Tab::Commands => &mut self.commands,
            Tab::Directories => {
                let load = &mut self.load_directories;
                self.directories.get_or_insert_with(|| {
                    let load = load.take().expect("directories are loaded only once");
                    List::new(load())
                })
            }
        }
    }

    /// Re-runs the search after the query changed.
    fn refresh(&mut self) {
        self.commands.stale = true;
        if let Some(directories) = &mut self.directories {
            directories.stale = true;
        }
        self.show_current();
    }

    /// Brings the current tab's list up to date with the query.
    fn show_current(&mut self) {
        let query = self.query.clone();
        let list = self.list();
        if list.stale {
            list.search(&query);
        }
        self.selected_at = Instant::now();
    }

    /// Starts the closing transition; `outcome` is reported when it ends.
    fn close(&mut self, outcome: Outcome) {
        self.closing_text = Some(match &outcome {
            Outcome::Execute(text) | Outcome::Edit(text) | Outcome::EditPath(text) => {
                text.replace('\n', " ")
            }
            // These leave the shell's buffer as it was.
            Outcome::ChangeDirectory(_) | Outcome::Cancel => self.initial_query.clone(),
        });
        self.transition.outcome = Some(outcome);
        // The clock last ticked whenever the last frame was drawn, which
        // may be long ago.
        self.transition.last_tick = Instant::now();
        // From here on input is the shell's, and it cannot read mouse reports.
        let mut out = io::stdout();
        let _ = out.write_all(DISABLE_MOUSE.as_bytes()).and_then(|()| out.flush());
    }

    fn switch_tab(&mut self) {
        self.tab = self.tab.next();
        self.show_current();
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.query
            .char_indices()
            .nth(char_index)
            .map_or(self.query.len(), |(i, _)| i)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_index(self.cursor);
        let clean: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
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

    fn selected_text(&mut self) -> Option<String> {
        let list = self.list();
        list.hits
            .get(list.selected)
            .map(|h| list.engine.row(h.index).text.clone())
    }

    /// Enter: act on the selection the way its tab calls for.
    fn accept(&mut self) -> Outcome {
        match (self.selected_text(), self.tab) {
            (Some(text), Tab::Commands) => Outcome::Execute(text),
            (Some(path), Tab::Directories) => Outcome::ChangeDirectory(path),
            // With nothing to pick, hand the typed text back for editing
            // rather than running something the user never saw matched.
            (None, _) => Outcome::Edit(self.query.clone()),
        }
    }

    /// Right at the end of the query: take the selection into the shell's
    /// edit buffer. `None` if there is nothing selected.
    fn take(&mut self) -> Option<Outcome> {
        let text = self.selected_text()?;
        Some(match self.tab {
            Tab::Commands => Outcome::Edit(text),
            Tab::Directories => Outcome::EditPath(text),
        })
    }

    /// Moves the query cursor right, or at the end of the query takes the
    /// selection, as accepting a suggestion does in a shell.
    fn right(&mut self, len: usize) -> Step {
        if self.cursor < len {
            self.cursor += 1;
        } else if let Some(outcome) = self.take() {
            return Step::Done(outcome);
        }
        Step::Continue
    }

    /// End of the word after the cursor.
    fn word_end(&self) -> usize {
        let chars: Vec<char> = self.query.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let Geometry { first_row_y, list_rows, tab_y } = self.geometry;
        match mouse.kind {
            // The list grows upward, so rolling the wheel up brings later
            // candidates into view.
            MouseEventKind::ScrollUp => self.scroll(1),
            MouseEventKind::ScrollDown => self.scroll(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if Some(mouse.row) == tab_y {
                    let mut x = 0;
                    for tab in Tab::ALL {
                        let width = tab.label().len() as u16 + 2;
                        if (x..x + width).contains(&mouse.column) && tab != self.tab {
                            self.tab = tab;
                            self.show_current();
                        }
                        x += width;
                    }
                } else if mouse.row <= first_row_y
                    && usize::from(first_row_y - mouse.row) < list_rows
                {
                    let list = self.list();
                    let index = list.offset + usize::from(first_row_y - mouse.row);
                    if index < list.hits.len() && index != list.selected {
                        list.selected = index;
                        self.selected_at = Instant::now();
                    }
                }
            }
            _ => {}
        }
    }

    /// Scrolls the list by `delta` rows without moving the selection,
    /// unless it would leave the window; then it rides the edge.
    fn scroll(&mut self, delta: isize) {
        let rows = self.geometry.list_rows;
        let list = self.list();
        let max_offset = list.hits.len().saturating_sub(rows);
        let offset = list.offset.saturating_add_signed(delta).min(max_offset);
        if offset == list.offset || rows == 0 {
            return;
        }
        list.offset = offset;
        let selected = list.selected.clamp(offset, offset + rows - 1);
        if selected != list.selected {
            list.selected = selected;
            self.selected_at = Instant::now();
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.geometry.list_rows.max(1) as isize;
        let len = self.query.chars().count();
        match key.code {
            KeyCode::Esc => {
                // Back to the state the search was opened in first; from
                // there, out.
                if self.query == self.initial_query {
                    return Step::Done(Outcome::Cancel);
                }
                self.query = self.initial_query.clone();
                self.cursor = self.query.chars().count();
                self.refresh();
            }
            KeyCode::Char('c' | 'g') if ctrl => return Step::Done(Outcome::Cancel),
            KeyCode::Char('d') if ctrl && self.query.is_empty() => {
                return Step::Done(Outcome::Cancel);
            }
            KeyCode::Enter | KeyCode::Char('j' | 'm') if ctrl || key.code == KeyCode::Enter => {
                return Step::Done(self.accept());
            }
            KeyCode::Tab | KeyCode::BackTab => self.switch_tab(),
            KeyCode::Up => self.move_selection(1),
            KeyCode::Down => self.move_selection(-1),
            KeyCode::Char('p') if ctrl => self.move_selection(1),
            KeyCode::Char('n') if ctrl => self.move_selection(-1),
            KeyCode::PageUp => self.move_selection(page),
            KeyCode::PageDown => self.move_selection(-page),
            // By word. Option+arrow arrives as Alt+arrow or as Esc b / Esc f
            // depending on the terminal; Ctrl+arrow is the Linux habit.
            KeyCode::Left if alt || ctrl => self.cursor = self.word_start(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_end(),
            KeyCode::Char('b') if alt => self.cursor = self.word_start(),
            KeyCode::Char('f') if alt => self.cursor = self.word_end(),
            KeyCode::Backspace if alt => self.delete_range(self.word_start(), self.cursor),
            KeyCode::Char('d') if alt => self.delete_range(self.cursor, self.word_end()),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => return self.right(len),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => return self.right(len),
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
        let list = self.list();
        if list.hits.is_empty() {
            return;
        }
        let max = list.hits.len() - 1;
        let next = list.selected.saturating_add_signed(delta).min(max);
        if next != list.selected {
            list.selected = next;
            self.selected_at = Instant::now();
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.animating = false;
        self.transition.advance();
        let area = frame.area();
        if area.height == 0 || area.width == 0 {
            return;
        }
        let width = usize::from(area.width);
        let prompt_width = self.prompt.width();
        let alpha = self.transition.alpha();
        let (shown_query, shown_cursor) = match &self.closing_text {
            Some(text) => (text.as_str(), text.chars().count()),
            None => (self.query.as_str(), self.cursor),
        };

        // Bottom up: the tab bar, the query, then the list. The query wraps
        // like a shell edit buffer and takes rows from the list; if it
        // outgrows the viewport, the rows around the cursor win.
        let tab_rows = usize::from(area.height >= 3);
        let input = layout_input(shown_query, shown_cursor, prompt_width, width);
        let input_rows = input.rows.len().min(usize::from(area.height) - tab_rows);
        let first_input_row = (input.cursor.0 + 1).saturating_sub(input_rows);
        let input_top = area.bottom() - (tab_rows + input_rows) as u16;
        let list_rows = usize::from(area.height) - tab_rows - input_rows;
        let indent = if prompt_width == 0 { 2 } else { prompt_width }
            .min(width / 2)
            .max(1);

        self.list();
        let list = match self.tab {
            Tab::Commands => &mut self.commands,
            Tab::Directories => self.directories.as_mut().expect("loaded by `list`"),
        };

        self.geometry = Geometry {
            first_row_y: input_top.saturating_sub(1),
            list_rows,
            tab_y: (tab_rows > 0).then(|| area.bottom() - 1),
        };

        // Keep the selection inside the visible window.
        if list.selected < list.offset {
            list.offset = list.selected;
        } else if list_rows > 0 && list.selected >= list.offset + list_rows {
            list.offset = list.selected + 1 - list_rows;
        }

        let now = list.engine.now_ms();

        // Nothing but the prompt line shows until the slide is over.
        let list_rows = if alpha > 0.0 { list_rows } else { 0 };
        for row in 0..list_rows {
            let hit_index = list.offset + row;
            let Some(hit) = list.hits.get(hit_index).copied() else {
                break;
            };
            let y = input_top - 1 - row as u16;
            let selected = hit_index == list.selected;
            let positions = list.engine.match_positions(&self.query, hit.index);
            let command = list.engine.row(hit.index);

            let theme = &self.theme;
            let base = if selected {
                Style::default()
                    .fg(theme.selected)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let muted = Style::default().fg(theme.muted);
            let columns = text_columns(&command.text, &positions, base, base.fg(theme.matched));

            // The age is pinned to the right edge; the command gets what is
            // left between the indent and it.
            let age = format_age(now - command.last_used_at);
            let age_width = age.chars().count();
            let show_age = indent + 2 + age_width < width;
            let avail = if show_age {
                width - indent - 2 - age_width
            } else {
                width - indent
            };

            // What tells two long paths apart is their end, so a path that
            // does not fit keeps its tail; a command keeps its head.
            let keep_tail = self.tab == Tab::Directories;
            let text = if columns.len() <= avail {
                window(&columns, 0, avail, false)
            } else if selected {
                self.animating = true;
                let cycle = columns.len() + MARQUEE_GAP;
                let rest = if keep_tail { columns.len() - avail } else { 0 };
                let offset = (rest + marquee_offset(self.selected_at.elapsed(), cycle)) % cycle;
                window(&columns, offset, avail, true)
            } else if keep_tail {
                let shown = avail.saturating_sub(1);
                let mut spans = vec![Span::styled("…", muted)];
                spans.extend(window(&columns, columns.len() - shown, shown, false));
                spans
            } else {
                let mut spans = window(&columns, 0, avail.saturating_sub(1), false);
                spans.push(Span::styled("…", muted));
                spans
            };

            let marker = if selected { "▌" } else { " " };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(theme.marker)),
                Span::raw(" ".repeat(indent - 1)),
            ];
            spans.extend(text);
            let row_area = Rect::new(area.x, y, area.width, 1);
            frame.render_widget(Line::from(spans), row_area);
            if show_age {
                frame.render_widget(
                    Span::styled(age, muted),
                    Rect::new(area.right() - age_width as u16, y, age_width as u16, 1),
                );
            }
            if selected {
                // Covers the whole row, including the blank columns.
                frame
                    .buffer_mut()
                    .set_style(row_area, Style::default().bg(theme.selected_bg));
            }
        }

        // The prompt line starts on the viewport's first row, where the
        // shell drew it, and slides down to its place.
        let travel = f32::from(input_top - area.y);
        let input_top = area.y + (travel * self.transition.slide()).round() as u16;
        for (i, text) in input
            .rows
            .iter()
            .enumerate()
            .skip(first_input_row)
            .take(input_rows)
        {
            let y = input_top + (i - first_input_row) as u16;
            let mut line = if i == 0 {
                self.prompt.clone()
            } else {
                Line::default()
            };
            line.spans.push(Span::styled(
                text.clone(),
                Style::default().fg(self.theme.query),
            ));
            frame.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
        if tab_rows > 0 && alpha > 0.0 {
            // Each name is a block padded by a column on either side, the
            // first flush with the left edge so the active tab's background
            // leaves no gap there.
            let mut spans = Vec::new();
            for tab in Tab::ALL {
                let style = if tab == self.tab {
                    Style::default()
                        .fg(self.theme.tab_active)
                        .bg(self.theme.tab_active_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.tab)
                };
                spans.push(Span::styled(format!(" {} ", tab.label()), style));
            }
            frame.render_widget(
                Line::from(spans),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        }

        if alpha > 0.0
            && alpha < 1.0
            && let Some(colors) = &self.colors
        {
            let above = Rect::new(area.x, area.y, area.width, input_top - area.y);
            let below_top = input_top + input_rows as u16;
            let below = Rect::new(area.x, below_top, area.width, area.bottom() - below_top);
            fade(frame.buffer_mut(), above, colors, alpha);
            fade(frame.buffer_mut(), below, colors, alpha);
        }

        if !self.transition.running() {
            let (cursor_row, cursor_col) = input.cursor;
            frame.set_cursor_position((
                area.x + cursor_col.min(width - 1) as u16,
                input_top + (cursor_row - first_input_row) as u16,
            ));
        }
    }
}

/// Draws `area` at `alpha` opacity by mixing every color in it toward the
/// terminal's background.
fn fade(buffer: &mut Buffer, area: Rect, colors: &TermColors, alpha: f32) {
    let background = colors.background;
    let rgb = |(r, g, b): term::Rgb| Color::Rgb(r, g, b);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            let fg = colors.resolve(cell.fg, colors.foreground);
            cell.fg = rgb(term::mix(background, fg, alpha));
            // A cell without a background of its own already shows the
            // terminal's.
            if cell.bg != Color::Reset {
                let bg = colors.resolve(cell.bg, background);
                cell.bg = rgb(term::mix(background, bg, alpha));
            }
        }
    }
}

/// Keys typed before the event loop took over the terminal's input, decoded
/// from raw bytes. Covers what someone types ahead; other escape sequences
/// are dropped.
fn decode_keys(bytes: &[u8]) -> Vec<KeyEvent> {
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

/// The query broken into terminal rows, and where the cursor lands.
struct InputLayout {
    rows: Vec<String>,
    /// (row, column) of the cursor.
    cursor: (usize, usize),
}

/// Wraps `query` the way zsh wraps its edit buffer: text fills each row to
/// the last column, a wide character that does not fit moves to the next row
/// whole, and a cursor past a full row sits at the start of the next one.
/// The first row starts at `start_col`, after the prompt.
fn layout_input(query: &str, cursor: usize, start_col: usize, width: usize) -> InputLayout {
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

/// One terminal column of a candidate's text.
#[derive(Clone, Copy)]
enum Column {
    /// A character and how many columns it spans, starting here.
    Start(char, usize, Style),
    /// The second column of a wide character.
    Continuation,
}

/// Lays a command out one entry per terminal column, styling the characters
/// at `positions` (sorted char indices) with `matched`.
fn text_columns(text: &str, positions: &[u32], base: Style, matched: Style) -> Vec<Column> {
    let mut columns = Vec::with_capacity(text.len());
    let mut next_match = positions.iter().copied().peekable();
    for (i, c) in text.chars().enumerate() {
        let is_match = next_match.next_if_eq(&(i as u32)).is_some();
        let c = match c {
            '\n' => '↵',
            c if c.is_control() => ' ',
            c => c,
        };
        // Zero-width characters have no column of their own to scroll by.
        let w = c.width().unwrap_or(0);
        if w == 0 {
            continue;
        }
        columns.push(Column::Start(c, w, if is_match { matched } else { base }));
        columns.extend(std::iter::repeat_n(Column::Continuation, w - 1));
    }
    columns
}

/// Spans for the `len` columns starting at `offset`. With `cyclic`, the text
/// repeats after [`MARQUEE_GAP`] blank columns. A wide character cut by
/// either edge is drawn as blanks so the row keeps its width.
fn window(columns: &[Column], offset: usize, len: usize, cyclic: bool) -> Vec<Span<'static>> {
    let cycle = columns.len() + MARQUEE_GAP;
    let mut spans = Vec::with_capacity(len);
    for i in 0..len {
        let at = if cyclic {
            (offset + i) % cycle
        } else {
            offset + i
        };
        match columns.get(at) {
            Some(&Column::Start(c, w, style)) if i + w <= len => {
                spans.push(Span::styled(c.to_string(), style));
            }
            Some(Column::Start(..)) => spans.push(Span::raw(" ")),
            // Only reachable when the character's first column is off-window.
            Some(Column::Continuation) if i == 0 => spans.push(Span::raw(" ")),
            Some(Column::Continuation) => {}
            None if cyclic => spans.push(Span::raw(" ")),
            None => break,
        }
    }
    spans
}

/// Column the marquee has scrolled to: it rests at 0 for [`MARQUEE_PAUSE`],
/// advances one column per [`MARQUEE_STEP`], and starts over after `cycle`
/// columns, where the repeated text has reached the start position.
fn marquee_offset(elapsed: Duration, cycle: usize) -> usize {
    let period = MARQUEE_PAUSE + MARQUEE_STEP * cycle as u32;
    let t = Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64);
    match t.checked_sub(MARQUEE_PAUSE) {
        Some(scrolling) => (scrolling.as_nanos() / MARQUEE_STEP.as_nanos()) as usize % cycle,
        None => 0,
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
    // Erased by hand rather than with `terminal.clear()`, which asks the
    // terminal for the cursor position and, reading the reply, would swallow
    // whatever was typed for the shell during the closing transition.
    crossterm::execute!(
        out,
        MoveTo(0, top),
        Clear(ClearType::FromCursorDown),
        MoveTo(origin_col, top + rows_below_prompt),
        Show
    )?;
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

    fn window_text(text: &str, offset: usize, len: usize, cyclic: bool) -> String {
        let style = Style::default();
        let columns = text_columns(text, &[], style, style);
        window(&columns, offset, len, cyclic)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn window_scrolls_and_wraps_around() {
        assert_eq!(window_text("abcdef", 0, 4, false), "abcd");
        assert_eq!(window_text("abcdef", 2, 4, true), "cdef");
        // Past the end: the gap, then the text again.
        assert_eq!(window_text("abcdef", 5, 8, true), "f    abc");
        assert_eq!(window_text("abcdef", 6 + MARQUEE_GAP, 3, true), "abc");
    }

    #[test]
    fn window_blanks_wide_chars_cut_by_an_edge() {
        // "a中b" occupies columns a|中|中|b.
        assert_eq!(window_text("a中b", 0, 2, false), "a ");
        assert_eq!(window_text("a中b", 2, 2, true), " b");
        assert_eq!(window_text("a中b", 1, 3, true), "中b");
    }

    #[test]
    fn marquee_rests_then_advances_then_repeats() {
        let cycle = 10;
        assert_eq!(marquee_offset(Duration::ZERO, cycle), 0);
        assert_eq!(
            marquee_offset(MARQUEE_PAUSE - Duration::from_millis(1), cycle),
            0
        );
        assert_eq!(marquee_offset(MARQUEE_PAUSE + MARQUEE_STEP * 3, cycle), 3);
        let period = MARQUEE_PAUSE + MARQUEE_STEP * cycle as u32;
        assert_eq!(
            marquee_offset(period + MARQUEE_PAUSE + MARQUEE_STEP * 3, cycle),
            3
        );
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

    #[test]
    fn transition_slides_then_fades_and_reverses() {
        let mut t = Transition::new(true);
        assert_eq!((t.slide(), t.alpha()), (0.0, 0.0));
        t.elapsed = SLIDE / 2;
        assert!(t.slide() > 0.5 && t.slide() < 1.0, "eases out");
        assert_eq!(t.alpha(), 0.0);
        t.elapsed = SLIDE + FADE / 2;
        assert_eq!(t.slide(), 1.0);
        assert!((t.alpha() - 0.5).abs() < 0.01);
        t.elapsed = SLIDE + FADE;
        assert!(!t.running());

        t.outcome = Some(Outcome::Cancel);
        assert!(t.running() && !t.closed());
        t.elapsed = Duration::ZERO;
        assert!(t.closed());
    }

    #[test]
    fn without_colors_the_list_appears_when_the_slide_ends() {
        let mut t = Transition::new(false);
        t.elapsed = SLIDE - Duration::from_millis(1);
        assert_eq!(t.alpha(), 0.0);
        t.elapsed = SLIDE;
        assert_eq!(t.alpha(), 1.0);
        assert!(!t.running());
    }

    fn app(texts: &[&str], query: &str) -> App {
        use crate::item::Item;
        use crate::ranker::{DefaultRanker, RankContext};
        let n = texts.len() as i64;
        let items = texts
            .iter()
            .enumerate()
            .map(|(i, t)| Item {
                text: t.to_string(),
                count: 1,
                last_used_at: n - i as i64,
                last_exit: None,
                cwd_count: 0,
            })
            .collect();
        let ctx = RankContext { now_ms: n };
        let sources = Sources {
            commands: Engine::new(items, Box::new(DefaultRanker), ctx.clone()),
            directories: Box::new(move || {
                let dir = Item {
                    text: "/tmp".into(),
                    count: 1,
                    last_used_at: 0,
                    last_exit: None,
                    cwd_count: 0,
                };
                Engine::for_paths(vec![dir], Box::new(DefaultRanker), ctx)
            }),
        };
        let opts = Options {
            query: query.to_string(),
            cursor: None,
            prompt: "> ".to_string(),
            max_items: 10,
            theme: Theme::default(),
        };
        App::new(sources, &opts, None)
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Outcome> {
        match app.on_key(KeyEvent::new(code, modifiers)) {
            Step::Done(outcome) => Some(outcome),
            Step::Continue => None,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn option_arrows_move_by_word_in_either_encoding() {
        let mut app = app(&[], "git commit  -m msg");
        let alt = KeyModifiers::ALT;
        press(&mut app, KeyCode::Left, alt);
        assert_eq!(app.cursor, 15, "start of `msg`");
        press(&mut app, KeyCode::Char('b'), alt);
        assert_eq!(app.cursor, 12, "start of `-m`");
        press(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(app.cursor, 4, "start of `commit`, across two spaces");
        press(&mut app, KeyCode::Right, alt);
        assert_eq!(app.cursor, 10, "end of `commit`");
        press(&mut app, KeyCode::Char('f'), alt);
        assert_eq!(app.cursor, 14, "end of `-m`");
        // Moving by word at the end of the query does not take the selection.
        app.cursor = 18;
        assert_eq!(press(&mut app, KeyCode::Right, alt), None);
    }

    #[test]
    fn option_deletes_by_word() {
        let mut app = app(&[], "git commit -m");
        press(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(app.query, "git commit ");
        app.cursor = 4;
        press(&mut app, KeyCode::Char('d'), KeyModifiers::ALT);
        assert_eq!(app.query, "git  ");
        // A plain letter with Option held is not typed.
        press(&mut app, KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(app.query, "git  ");
    }

    #[test]
    fn esc_returns_to_the_initial_query_then_closes() {
        let mut app = app(&["echo hello"], "ec");
        press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(app.query, "ech");
        assert_eq!(press(&mut app, KeyCode::Esc, KeyModifiers::NONE), None);
        assert_eq!((app.query.as_str(), app.cursor), ("ec", 2));
        assert_eq!(press(&mut app, KeyCode::Esc, KeyModifiers::NONE), Some(Outcome::Cancel));
    }

    #[test]
    fn wheel_scrolls_the_window_and_carries_the_selection_at_its_edge() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let mut app = app(&texts.iter().map(String::as_str).collect::<Vec<_>>(), "");
        app.geometry = Geometry { first_row_y: 10, list_rows: 5, tab_y: Some(12) };

        app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!((app.commands.offset, app.commands.selected), (1, 1));
        app.commands.selected = 3;
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!((app.commands.offset, app.commands.selected), (2, 3), "selection stays");
        for _ in 0..50 {
            app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        }
        assert_eq!((app.commands.offset, app.commands.selected), (15, 15), "stops at the end");
        for _ in 0..50 {
            app.on_mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        }
        assert_eq!((app.commands.offset, app.commands.selected), (0, 4));
    }

    #[test]
    fn page_keys_move_by_a_screenful() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let mut app = app(&texts.iter().map(String::as_str).collect::<Vec<_>>(), "");
        app.geometry = Geometry { first_row_y: 10, list_rows: 5, tab_y: Some(12) };
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.commands.selected, 5);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.commands.selected, 0);
    }

    #[test]
    fn clicks_select_rows_and_switch_tabs() {
        let mut app = app(&["a", "b", "c"], "");
        app.geometry = Geometry { first_row_y: 10, list_rows: 5, tab_y: Some(12) };
        let click = |x, y| mouse(MouseEventKind::Down(MouseButton::Left), x, y);

        app.on_mouse(click(7, 8));
        assert_eq!(app.commands.selected, 2);
        // A row with no candidate on it.
        app.on_mouse(click(7, 6));
        assert_eq!(app.commands.selected, 2);
        // " commands " is columns 0..10, " directories " follows.
        app.on_mouse(click(12, 12));
        assert_eq!(app.tab, Tab::Directories);
        app.on_mouse(click(12, 12));
        assert_eq!(app.tab, Tab::Directories, "clicking the current tab does nothing");
        app.on_mouse(click(0, 12));
        assert_eq!(app.tab, Tab::Commands);
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(5_000), "now");
        assert_eq!(format_age(90_000), "1m");
        assert_eq!(format_age(3 * 86_400_000), "3d");
        assert_eq!(format_age(400 * 86_400_000), "1y");
    }
}

//! Drawing a frame of the search box.

use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::app::{App, Geometry, Tab};
use super::input::layout_input;
use super::term::{self, TermColors};

/// How long an overflowing selected row rests at its start before scrolling.
pub(super) const MARQUEE_PAUSE: Duration = Duration::from_millis(800);
/// Time per column while the selected row scrolls.
pub(super) const MARQUEE_STEP: Duration = Duration::from_millis(120);
/// Blank columns between the end of the text and its repetition.
pub(super) const MARQUEE_GAP: usize = 4;
/// Redraw interval while something is animating.
pub(super) const TICK: Duration = Duration::from_millis(40);

impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
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
pub(super) fn fade(buffer: &mut Buffer, area: Rect, colors: &TermColors, alpha: f32) {
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

/// One terminal column of a candidate's text.
#[derive(Clone, Copy)]
pub(super) enum Column {
    /// A character and how many columns it spans, starting here.
    Start(char, usize, Style),
    /// The second column of a wide character.
    Continuation,
}

/// Lays a command out one entry per terminal column, styling the characters
/// at `positions` (sorted char indices) with `matched`.
pub(super) fn text_columns(
    text: &str,
    positions: &[u32],
    base: Style,
    matched: Style,
) -> Vec<Column> {
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
pub(super) fn window(
    columns: &[Column],
    offset: usize,
    len: usize,
    cyclic: bool,
) -> Vec<Span<'static>> {
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
pub(super) fn marquee_offset(elapsed: Duration, cycle: usize) -> usize {
    let period = MARQUEE_PAUSE + MARQUEE_STEP * cycle as u32;
    let t = Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64);
    match t.checked_sub(MARQUEE_PAUSE) {
        Some(scrolling) => (scrolling.as_nanos() / MARQUEE_STEP.as_nanos()) as usize % cycle,
        None => 0,
    }
}

/// Turns the shell's prompt line into styled spans.
pub(super) fn parse_prompt(prompt: &str) -> Line<'static> {
    let last_line = prompt.rsplit('\n').next().unwrap_or("");
    match last_line.as_bytes().into_text() {
        Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
        Err(_) => Line::raw(last_line.to_string()),
    }
}

pub(super) fn format_age(ms: i64) -> String {
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
    fn ages() {
        assert_eq!(format_age(5_000), "now");
        assert_eq!(format_age(90_000), "1m");
        assert_eq!(format_age(3 * 86_400_000), "3d");
        assert_eq!(format_age(400 * 86_400_000), "1y");
    }
}

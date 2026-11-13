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
use super::theme::Theme;
use crate::item::Run;

/// How long an overflowing selected row rests at its start before scrolling.
pub(super) const MARQUEE_PAUSE: Duration = Duration::from_millis(800);
/// Time per column while the selected row scrolls.
pub(super) const MARQUEE_STEP: Duration = Duration::from_millis(120);
/// Blank columns between the end of the text and its repetition.
pub(super) const MARQUEE_GAP: usize = 4;
/// Redraw interval while something is animating.
pub(super) const TICK: Duration = Duration::from_millis(40);

/// Marks a command whose last run failed.
const MARK_FAILED: &str = "✗";
/// Marks a command that has been run in the current directory.
const MARK_HERE: &str = "⌂";
/// Columns taken by the marks: one each, a blank between.
const MARKS_WIDTH: usize = 3;
/// Columns taken by an age; the longest is `12mo`.
const AGE_WIDTH: usize = 4;

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

        // Looked up before the list is borrowed. It needs a row of its own,
        // and the selected command's row above it.
        let run = if list_rows >= 2 {
            self.selected_run()
        } else {
            None
        };
        let max_items = self.max_items;
        let home = self.home.clone();
        self.list();
        let list = match self.tab {
            Tab::Commands => &mut self.commands,
            Tab::Directories => self.directories.as_mut().expect("loaded by `list`"),
        };

        // The selected candidate takes a second row for its last run.
        let selected_index = list.selected;
        let height = |i: usize| {
            if i == selected_index && run.is_some() {
                2
            } else {
                1
            }
        };
        // Whether the candidates from `first` to `last` fit at once.
        let fits = |first: usize, last: usize| {
            last + 1 - first <= max_items && (first..=last).map(height).sum::<usize>() <= list_rows
        };

        // Keep the selection inside the visible window.
        if list.selected < list.offset {
            list.offset = list.selected;
        }
        while list.offset < list.selected && !fits(list.offset, list.selected) {
            list.offset += 1;
        }
        let mut shown = 0;
        while list.offset + shown < list.hits.len() && fits(list.offset, list.offset + shown) {
            shown += 1;
        }

        let now = list.engine.now_ms();
        let mut rows = Vec::new();

        // Nothing but the prompt line shows until the slide is over.
        let shown_now = if alpha > 0.0 { shown } else { 0 };
        let mut bottom = input_top;
        for row in 0..shown_now {
            let hit_index = list.offset + row;
            let hit = list.hits[hit_index];
            let rows_taken = height(hit_index) as u16;
            let y = bottom - rows_taken;
            bottom = y;
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

            // The marks and the age are pinned to the right edge, each in a
            // column of its own so they line up from row to row; the text
            // gets what is left between the indent and them. Without room
            // the marks go first, then the age.
            let full = if self.tab == Tab::Commands {
                MARKS_WIDTH + 1 + AGE_WIDTH
            } else {
                AGE_WIDTH
            };
            let right_width = if indent + 2 + full < width {
                full
            } else if indent + 2 + AGE_WIDTH < width {
                AGE_WIDTH
            } else {
                0
            };
            let avail = if right_width > 0 {
                width - indent - 2 - right_width
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

            let lead = || {
                let marker = if selected { "▌" } else { " " };
                vec![
                    Span::styled(marker, Style::default().fg(theme.marker)),
                    Span::raw(" ".repeat(indent - 1)),
                ]
            };
            let mut spans = lead();
            spans.extend(text);
            let row_area = Rect::new(area.x, y, area.width, rows_taken);
            frame.render_widget(
                Line::from(spans),
                Rect {
                    height: 1,
                    ..row_area
                },
            );
            rows.push((y, hit_index));

            if right_width > 0 {
                let mut right = Vec::new();
                if right_width > AGE_WIDTH {
                    let mark = |on: bool, icon: &'static str, color: Color| {
                        if on {
                            Span::styled(icon, Style::default().fg(color))
                        } else {
                            Span::raw(" ")
                        }
                    };
                    right.push(mark(command.failed(), MARK_FAILED, theme.error));
                    right.push(Span::raw(" "));
                    right.push(mark(command.cwd_count > 0, MARK_HERE, theme.here));
                    right.push(Span::raw(" "));
                }
                // The selected row's age matches its text, so the selection
                // shows at the right edge too.
                let age = format_age(now - command.last_used_at);
                let age_style = if selected { base } else { muted };
                right.push(Span::styled(format!("{age:>AGE_WIDTH$}"), age_style));
                frame.render_widget(
                    Line::from(right),
                    Rect::new(area.right() - right_width as u16, y, right_width as u16, 1),
                );
            }

            if rows_taken == 2
                && let Some(run) = &run
            {
                let mut spans = lead();
                spans.extend(run_spans(run, now, home.as_deref(), width - indent, theme));
                frame.render_widget(Line::from(spans), Rect::new(area.x, y + 1, area.width, 1));
                rows.push((y + 1, hit_index));
            }

            if selected {
                // Covers the whole row, including the blank columns.
                frame
                    .buffer_mut()
                    .set_style(row_area, Style::default().bg(theme.selected_bg));
            }
        }

        self.geometry = Geometry {
            rows,
            shown,
            tab_y: (tab_rows > 0).then(|| area.bottom() - 1),
        };

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

/// The line under the selected command: where and when it last ran, how
/// long it took and how it ended, cut to `width` columns. Imported runs
/// know only some of this; what is unknown is left out.
pub(super) fn run_spans(
    run: &Run,
    now_ms: i64,
    home: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let muted = Style::default().fg(theme.muted);
    let mut parts: Vec<(String, Style)> = Vec::new();
    if let Some(ms) = run.duration_ms {
        parts.push((format_duration(ms), muted));
    }
    match run.exit_code {
        None | Some(0) => {}
        Some(130) => parts.push(("interrupted".to_string(), muted)),
        Some(code) => parts.push((format!("exit {code}"), Style::default().fg(theme.error))),
    }
    if let (Some(at), Some(now)) = (clock(run.started_at), clock(now_ms)) {
        parts.push((format_clock(&at, &now), muted));
    }
    if let Some(host) = &run.hostname {
        parts.push((format!("on {host}"), muted));
    }

    // The directory comes first and gives up its head when space is short.
    // Everything after it is preceded by a separator.
    const SEP: &str = " · ";
    let rest: usize = parts
        .iter()
        .map(|(text, _)| text_width(SEP) + text_width(text))
        .sum();
    if let Some(cwd) = &run.cwd {
        let cwd = tilde(cwd, home);
        let room = width.saturating_sub(rest);
        let fitted = if text_width(&cwd) <= room {
            Some(cwd)
        } else if room >= 8 {
            Some(keep_tail(&cwd, room))
        } else {
            None
        };
        if let Some(cwd) = fitted {
            parts.insert(0, (cwd, muted));
        }
    }

    let mut spans = Vec::new();
    let mut used = 0;
    for (i, (text, style)) in parts.into_iter().enumerate() {
        // The separator is always muted, whatever the part after it is.
        let sep = if i == 0 { "" } else { SEP };
        let w = text_width(sep) + text_width(&text);
        if used + w > width {
            break;
        }
        used += w;
        if !sep.is_empty() {
            spans.push(Span::styled(sep, muted));
        }
        spans.push(Span::styled(text, style));
    }
    spans
}

/// Width in columns of `text` as drawn.
fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// The end of `text` that fits in `width` columns, after an ellipsis.
fn keep_tail(text: &str, width: usize) -> String {
    let mut tail = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// `path` with the home directory written as `~`.
pub(super) fn tilde(path: &str, home: Option<&str>) -> String {
    match home.and_then(|h| path.strip_prefix(h)) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

pub(super) fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..10_000 => format!("{:.1}s", ms as f64 / 1000.0),
        10_000..60_000 => format!("{}s", ms / 1000),
        60_000..3_600_000 => format!("{}m {}s", ms / 60_000, ms / 1000 % 60),
        _ => format!("{}h {}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

/// A moment in local time, as much of it as is shown.
pub(super) struct Clock {
    year: i32,
    day_of_year: i32,
    /// 0 is January.
    month: usize,
    day: i32,
    hour: i32,
    minute: i32,
}

fn clock(ms: i64) -> Option<Clock> {
    let t = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is plain data, and both pointers are valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(Clock {
        year: tm.tm_year + 1900,
        day_of_year: tm.tm_yday,
        month: tm.tm_mon as usize,
        day: tm.tm_mday,
        hour: tm.tm_hour,
        minute: tm.tm_min,
    })
}

/// The time of day for today, the date and time for earlier this year, and
/// the date before that. The age next to the command already says how long
/// ago; this says when.
pub(super) fn format_clock(at: &Clock, now: &Clock) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.get(at.month).copied().unwrap_or("?");
    let time = format!("{:02}:{:02}", at.hour, at.minute);
    if at.year != now.year {
        format!("{month} {} {}", at.day, at.year)
    } else if at.day_of_year != now.day_of_year {
        format!("{month} {} {time}", at.day)
    } else {
        time
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
    fn durations() {
        assert_eq!(format_duration(120), "120ms");
        assert_eq!(format_duration(3_240), "3.2s");
        assert_eq!(format_duration(38_000), "38s");
        assert_eq!(format_duration(125_000), "2m 5s");
        assert_eq!(format_duration(3_780_000), "1h 3m");
    }

    #[test]
    fn home_is_written_as_tilde() {
        let home = Some("/Users/me");
        assert_eq!(tilde("/Users/me", home), "~");
        assert_eq!(tilde("/Users/me/src", home), "~/src");
        assert_eq!(tilde("/Users/meg", home), "/Users/meg");
        assert_eq!(tilde("/tmp", None), "/tmp");
    }

    #[test]
    fn clock_shows_less_the_closer_it_is() {
        let at = |year, day_of_year, month, day| Clock {
            year,
            day_of_year,
            month,
            day,
            hour: 9,
            minute: 5,
        };
        let now = at(2026, 317, 10, 14);
        assert_eq!(format_clock(&at(2026, 317, 10, 14), &now), "09:05");
        assert_eq!(format_clock(&at(2026, 315, 10, 12), &now), "Nov 12 09:05");
        assert_eq!(format_clock(&at(2025, 315, 10, 12), &now), "Nov 12 2025");
    }

    fn run_text(run: &Run, width: usize) -> String {
        run_spans(
            run,
            run.started_at,
            Some("/home/me"),
            width,
            &Theme::default(),
        )
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
    }

    #[test]
    fn run_line_leaves_out_what_is_unknown_and_shortens_the_directory() {
        let mut run = Run {
            started_at: 0,
            duration_ms: Some(38_000),
            exit_code: Some(1),
            cwd: Some("/home/me/work/zray".into()),
            hostname: Some("box".into()),
        };
        let full = run_text(&run, 80);
        assert!(full.starts_with("~/work/zray · 38s · exit 1 · "), "{full}");
        assert!(full.ends_with(" · on box"), "{full}");

        // Only the exit code takes the error color, not the separator before it.
        let theme = Theme::default();
        let spans = run_spans(&run, 0, Some("/home/me"), 80, &theme);
        let red: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.fg == Some(theme.error))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(red, ["exit 1"]);

        let narrow = run_text(&run, text_width(&full) - 3);
        assert!(narrow.starts_with("…rk/zray · 38s"), "{narrow}");

        run.exit_code = Some(0);
        run.hostname = None;
        run.cwd = None;
        run.duration_ms = None;
        assert!(!run_text(&run, 80).contains('·'), "only the time is left");
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(5_000), "now");
        assert_eq!(format_age(90_000), "1m");
        assert_eq!(format_age(3 * 86_400_000), "3d");
        assert_eq!(format_age(400 * 86_400_000), "1y");
    }
}

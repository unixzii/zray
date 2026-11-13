//! The search box's state and how keys and the mouse change it. Drawing is
//! in `view`.

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::text::Line;

use super::term::TermColors;
use super::theme::Theme;
use super::transition::Transition;
use super::view::parse_prompt;
use super::{DISABLE_MOUSE, LastRun, Options, Outcome, Sources};
use crate::item::Run;
use crate::matcher::{Engine, Hit};

/// Where things were drawn in the last frame, for finding what the mouse
/// is pointing at.
#[derive(Default, Clone)]
pub(super) struct Geometry {
    /// Each screen row of the list with the index into `hits` of the
    /// candidate drawn there. The selected candidate may take two rows.
    pub(super) rows: Vec<(u16, usize)>,
    /// How many candidates were shown.
    pub(super) shown: usize,
    /// Screen row of the tab bar, if it is showing.
    pub(super) tab_y: Option<u16>,
}

/// What a key press asks the event loop to do.
pub(super) enum Step {
    Continue,
    Done(Outcome),
}

/// The lists the search box can show, switched with Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Commands,
    Directories,
}

impl Tab {
    pub(super) const ALL: [Tab; 2] = [Tab::Commands, Tab::Directories];

    pub(super) fn label(self) -> &'static str {
        match self {
            Tab::Commands => "commands",
            Tab::Directories => "directories",
        }
    }

    pub(super) fn next(self) -> Tab {
        match self {
            Tab::Commands => Tab::Directories,
            Tab::Directories => Tab::Commands,
        }
    }
}

/// One tab's candidates and where the user is in them.
pub(super) struct List {
    pub(super) engine: Engine,
    pub(super) hits: Vec<Hit>,
    /// Index into `hits`; 0 is the best match, drawn nearest the input.
    pub(super) selected: usize,
    /// Index into `hits` of the row drawn nearest the input.
    pub(super) offset: usize,
    /// The query changed while this tab was not showing.
    pub(super) stale: bool,
}

impl List {
    pub(super) fn new(engine: Engine) -> Self {
        List {
            engine,
            hits: Vec::new(),
            selected: 0,
            offset: 0,
            stale: true,
        }
    }

    pub(super) fn search(&mut self, query: &str) {
        self.hits = self.engine.search(query);
        self.selected = 0;
        self.offset = 0;
        self.stale = false;
    }
}

pub(super) struct App {
    pub(super) commands: List,
    /// `None` until the tab is first shown.
    pub(super) directories: Option<List>,
    pub(super) load_directories: Option<Box<dyn FnOnce() -> Engine>>,
    pub(super) tab: Tab,
    pub(super) theme: Theme,
    pub(super) prompt: Line<'static>,
    /// Shared by all tabs.
    pub(super) query: String,
    /// The query the search was opened with; Esc returns to it.
    pub(super) initial_query: String,
    /// While closing, what the shell's buffer is about to hold. Shown in
    /// place of the query so the line does not change as the shell takes over.
    pub(super) closing_text: Option<String>,
    pub(super) transition: Transition,
    /// The terminal's colors, if it reported them; needed for fading.
    pub(super) colors: Option<TermColors>,
    /// Cursor position in the query, in chars.
    pub(super) cursor: usize,
    /// When the current row became selected; the marquee's clock.
    pub(super) selected_at: Instant,
    /// Whether the last frame needs redrawing on a timer.
    pub(super) animating: bool,
    pub(super) geometry: Geometry,
    /// The most candidates to show at once.
    pub(super) max_items: usize,
    /// Looks up a command's last run.
    pub(super) last_run: LastRun,
    /// Last runs looked up so far, by index into the commands engine.
    pub(super) runs: HashMap<usize, Option<Run>>,
    /// `$HOME`, shown as `~` in paths.
    pub(super) home: Option<String>,
}

impl App {
    pub(super) fn new(sources: Sources, opts: &Options, colors: Option<TermColors>) -> Self {
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
            max_items: usize::from(opts.max_items.max(1)),
            last_run: sources.last_run,
            runs: HashMap::new(),
            home: std::env::var("HOME").ok().filter(|h| !h.is_empty()),
        };
        app.refresh();
        app
    }

    /// The list of the tab being shown, loading it if this is its first use.
    pub(super) fn list(&mut self) -> &mut List {
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
    pub(super) fn refresh(&mut self) {
        self.commands.stale = true;
        if let Some(directories) = &mut self.directories {
            directories.stale = true;
        }
        self.show_current();
    }

    /// Brings the current tab's list up to date with the query.
    pub(super) fn show_current(&mut self) {
        let query = self.query.clone();
        let list = self.list();
        if list.stale {
            list.search(&query);
        }
        self.selected_at = Instant::now();
    }

    /// Starts the closing transition; `outcome` is reported when it ends.
    pub(super) fn close(&mut self, outcome: Outcome) {
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
        let _ = out
            .write_all(DISABLE_MOUSE.as_bytes())
            .and_then(|()| out.flush());
    }

    pub(super) fn switch_tab(&mut self) {
        self.tab = self.tab.next();
        self.show_current();
    }

    pub(super) fn byte_index(&self, char_index: usize) -> usize {
        self.query
            .char_indices()
            .nth(char_index)
            .map_or(self.query.len(), |(i, _)| i)
    }

    pub(super) fn insert(&mut self, text: &str) {
        let at = self.byte_index(self.cursor);
        let clean: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        self.query.insert_str(at, &clean);
        self.cursor += clean.chars().count();
        self.refresh();
    }

    pub(super) fn delete_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let (a, b) = (self.byte_index(from), self.byte_index(to));
        self.query.replace_range(a..b, "");
        self.cursor = from;
        self.refresh();
    }

    /// Start of the word before the cursor, for Ctrl-W.
    pub(super) fn word_start(&self) -> usize {
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

    /// The last run of the selected command, if the commands tab is showing.
    pub(super) fn selected_run(&mut self) -> Option<Run> {
        if self.tab != Tab::Commands {
            return None;
        }
        let index = self.commands.hits.get(self.commands.selected)?.index;
        if let Some(run) = self.runs.get(&index) {
            return run.clone();
        }
        let run = (self.last_run)(&self.commands.engine.row(index).text);
        self.runs.insert(index, run.clone());
        run
    }

    pub(super) fn selected_text(&mut self) -> Option<String> {
        let list = self.list();
        list.hits
            .get(list.selected)
            .map(|h| list.engine.row(h.index).text.clone())
    }

    /// Enter: act on the selection the way its tab calls for.
    pub(super) fn accept(&mut self) -> Outcome {
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
    pub(super) fn take(&mut self) -> Option<Outcome> {
        let text = self.selected_text()?;
        Some(match self.tab {
            Tab::Commands => Outcome::Edit(text),
            Tab::Directories => Outcome::EditPath(text),
        })
    }

    /// Moves the query cursor right, or at the end of the query takes the
    /// selection, as accepting a suggestion does in a shell.
    pub(super) fn right(&mut self, len: usize) -> Step {
        if self.cursor < len {
            self.cursor += 1;
        } else if let Some(outcome) = self.take() {
            return Step::Done(outcome);
        }
        Step::Continue
    }

    /// End of the word after the cursor.
    pub(super) fn word_end(&self) -> usize {
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

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            // The list grows upward, so rolling the wheel up brings later
            // candidates into view.
            MouseEventKind::ScrollUp => self.scroll(1),
            MouseEventKind::ScrollDown => self.scroll(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = self
                    .geometry
                    .rows
                    .iter()
                    .find(|(y, _)| *y == mouse.row)
                    .map(|&(_, index)| index);
                if Some(mouse.row) == self.geometry.tab_y {
                    let mut x = 0;
                    for tab in Tab::ALL {
                        let width = tab.label().len() as u16 + 2;
                        if (x..x + width).contains(&mouse.column) && tab != self.tab {
                            self.tab = tab;
                            self.show_current();
                        }
                        x += width;
                    }
                } else if let Some(index) = row {
                    let list = self.list();
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
    pub(super) fn scroll(&mut self, delta: isize) {
        let rows = self.geometry.shown;
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

    pub(super) fn on_key(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.geometry.shown.max(1) as isize;
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
    pub(super) fn move_selection(&mut self, delta: isize) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(texts: &[&str], query: &str) -> App {
        app_with(texts, query, |_| {}, Box::new(|_| None))
    }

    /// Commands listed newest first, each changed by `tweak`, with last runs
    /// from `last_run`.
    fn app_with(
        texts: &[&str],
        query: &str,
        tweak: impl Fn(&mut crate::item::Item),
        last_run: LastRun,
    ) -> App {
        use crate::item::Item;
        use crate::ranker::{DefaultRanker, RankContext};
        let n = texts.len() as i64;
        let items = texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut item = Item {
                    text: t.to_string(),
                    count: 1,
                    last_used_at: n - i as i64,
                    last_exit: None,
                    cwd_count: 0,
                };
                tweak(&mut item);
                item
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
            last_run,
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

    /// Draws a frame with the opening transition over; returns the screen
    /// one string per row, trailing blanks trimmed.
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        app.transition.elapsed = std::time::Duration::from_secs(1);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect()
    }

    fn run() -> Run {
        Run {
            started_at: 0,
            duration_ms: Some(38_000),
            exit_code: Some(1),
            cwd: Some("/w".into()),
            hostname: None,
        }
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Outcome> {
        match app.on_key(KeyEvent::new(code, modifiers)) {
            Step::Done(outcome) => Some(outcome),
            Step::Continue => None,
        }
    }

    /// Five one-row candidates, the first on screen row 10.
    fn geometry() -> Geometry {
        Geometry {
            rows: (0..5).map(|i| (10 - i as u16, i)).collect(),
            shown: 5,
            tab_y: Some(12),
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
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
        assert_eq!(
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE),
            Some(Outcome::Cancel)
        );
    }

    #[test]
    fn wheel_scrolls_the_window_and_carries_the_selection_at_its_edge() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let mut app = app(&texts.iter().map(String::as_str).collect::<Vec<_>>(), "");
        app.geometry = geometry();

        app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!((app.commands.offset, app.commands.selected), (1, 1));
        app.commands.selected = 3;
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(
            (app.commands.offset, app.commands.selected),
            (2, 3),
            "selection stays"
        );
        for _ in 0..50 {
            app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        }
        assert_eq!(
            (app.commands.offset, app.commands.selected),
            (15, 15),
            "stops at the end"
        );
        for _ in 0..50 {
            app.on_mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        }
        assert_eq!((app.commands.offset, app.commands.selected), (0, 4));
    }

    #[test]
    fn page_keys_move_by_a_screenful() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let mut app = app(&texts.iter().map(String::as_str).collect::<Vec<_>>(), "");
        app.geometry = geometry();
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.commands.selected, 5);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.commands.selected, 0);
    }

    #[test]
    fn clicks_select_rows_and_switch_tabs() {
        let mut app = app(&["a", "b", "c"], "");
        app.geometry = geometry();
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
        assert_eq!(
            app.tab,
            Tab::Directories,
            "clicking the current tab does nothing"
        );
        app.on_mouse(click(0, 12));
        assert_eq!(app.tab, Tab::Commands);
    }

    #[test]
    fn selected_command_shows_its_last_run_and_marks_line_up() {
        let mut app = app_with(
            &["make", "ls"],
            "",
            |item| {
                if item.text == "make" {
                    item.last_exit = Some(2);
                    item.cwd_count = 1;
                } else {
                    item.cwd_count = 1;
                }
            },
            Box::new(|_| Some(run())),
        );
        let screen = render(&mut app, 30, 6);
        // Rows 0..4 are the list, 4 the input, 5 the tab bar.
        // The marks sit in fixed columns, 22 and 24, before the age.
        assert_eq!(screen[1], format!("  ls{}⌂  now", " ".repeat(20)));
        assert_eq!(screen[2], format!("▌ make{}✗ ⌂  now", " ".repeat(16)));
        assert!(
            screen[3].starts_with("▌ /w · 38s · exit 1 · "),
            "{screen:?}"
        );
        // Both rows of the selection belong to it.
        assert_eq!(app.geometry.rows, [(2, 0), (3, 0), (1, 1)]);

        // Moving the selection moves the line with it.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        let screen = render(&mut app, 30, 6);
        assert!(screen[3].starts_with("  make"), "{screen:?}");
        assert!(screen[1].starts_with("▌ ls"), "{screen:?}");
        assert!(screen[2].starts_with("▌ /w"), "{screen:?}");
    }

    #[test]
    fn the_selection_with_its_last_run_stays_in_view() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut app = app_with(&texts, "", |_| {}, Box::new(|_| Some(run())));
        // Six rows for the list: five one-row candidates, or four and the
        // selected one with its second row.
        app.commands.selected = 5;
        render(&mut app, 40, 8);
        assert_eq!((app.commands.offset, app.geometry.shown), (1, 5));
        app.commands.selected = 0;
        render(&mut app, 40, 8);
        assert_eq!((app.commands.offset, app.geometry.shown), (0, 5));
    }

    #[test]
    fn without_a_last_run_the_selection_takes_one_row() {
        let texts: Vec<String> = (0..20).map(|i| format!("cmd {i}")).collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut app = app(&texts, "");
        render(&mut app, 40, 8);
        assert_eq!(app.geometry.shown, 6);
    }
}

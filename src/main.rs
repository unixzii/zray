use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use zray::matcher::Engine;
use zray::ranker::{DefaultRanker, RankContext};
use zray::store::{Store, now_ms};
use zray::tui::theme::{self, THEME_ENV, Theme};
use zray::tui::{self, Outcome, Sources};

const ZSH_INIT: &str = include_str!("../shell/zray.zsh");

/// How many distinct commands a search considers.
const CANDIDATE_LIMIT: usize = 50_000;
/// How many directories a search considers.
const DIRECTORY_LIMIT: usize = 10_000;

#[derive(Parser)]
#[command(version, about = "Interactive shell history search")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the shell integration script; use as `eval "$(zray init zsh)"`.
    Init { shell: Shell },
    /// Open the interactive search on the current terminal line.
    Search {
        /// Initial contents of the search box.
        #[arg(long, default_value = "")]
        query: String,
        /// Cursor position in the query, in characters; defaults to the end.
        #[arg(long)]
        cursor: Option<usize>,
        /// Directory to rank for; defaults to the current directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Last line of the shell prompt (may contain ANSI escapes).
        #[arg(long, default_value = "> ")]
        prompt: String,
        /// The most candidates to show at once.
        #[arg(long, env = "ZRAY_MAX_ITEMS", default_value_t = 10)]
        max_items: u16,
        /// Write the result to this file descriptor instead of stdout, so
        /// stdout can stay attached to the terminal.
        #[arg(long)]
        result_fd: Option<RawFd>,
    },
    /// Record command executions (called by the shell hooks).
    Record {
        #[command(subcommand)]
        action: Record,
    },
    /// Show the theme in effect and report problems in `$ZRAY_THEME`.
    Theme,
    /// Import an existing zsh history file.
    Import {
        /// Defaults to `$HISTFILE`, then `~/.zsh_history`.
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Record {
    /// A command is about to run; prints the execution id.
    Start {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        text: String,
    },
    /// The shell entered this directory.
    Dir { path: String },
    /// The command with this execution id finished.
    End {
        #[arg(long)]
        id: i64,
        #[arg(long, allow_hyphen_values = true)]
        exit: i32,
    },
}

#[derive(Clone, ValueEnum)]
enum Shell {
    Zsh,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init { shell: Shell::Zsh } => init_zsh(),
        Command::Search {
            query,
            cursor,
            cwd,
            prompt,
            max_items,
            result_fd,
        } => search(query, cursor, cwd, prompt, max_items, result_fd),
        Command::Record {
            action:
                Record::Start {
                    cwd,
                    session,
                    host,
                    text,
                },
        } => {
            let id = Store::open_default()?.record_start(
                &text,
                cwd.as_deref(),
                session.as_deref(),
                host.as_deref(),
            )?;
            println!("{id}");
            Ok(())
        }
        Command::Record {
            action: Record::Dir { path },
        } => Store::open_default()?.record_directory(&path),
        Command::Record {
            action: Record::End { id, exit },
        } => Store::open_default()?.record_end(id, exit),
        Command::Theme => show_theme(),
        Command::Import { file } => import(file),
    }
}

fn init_zsh() -> Result<()> {
    let exe = std::env::current_exe().context("locating the zray binary")?;
    let exe = exe
        .to_str()
        .context("zray binary path is not valid UTF-8")?;
    // Single-quote for zsh.
    let quoted = format!("'{}'", exe.replace('\'', r"'\''"));
    print!("{}", ZSH_INIT.replace("__ZRAY_BIN__", &quoted));
    Ok(())
}

fn search(
    query: String,
    cursor: Option<usize>,
    cwd: Option<String>,
    prompt: String,
    max_items: u16,
    result_fd: Option<RawFd>,
) -> Result<()> {
    let cwd = cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let store = Store::open_default()?;
    let ctx = RankContext { now_ms: now_ms() };
    let commands = Engine::new(
        store.load_commands(cwd.as_deref(), CANDIDATE_LIMIT)?,
        Box::new(DefaultRanker),
        ctx.clone(),
    );
    let directories = Box::new(move || {
        // Errors cannot be shown while the TUI owns the screen; an empty
        // tab is the visible symptom.
        let mut dirs = store.load_directories(DIRECTORY_LIMIT).unwrap_or_default();
        // Skip where we already are, and directories that are gone. They
        // stay in the database: the volume may just not be mounted.
        dirs.retain(|d| Some(&d.text) != cwd.as_ref() && Path::new(&d.text).is_dir());
        Engine::for_paths(dirs, Box::new(DefaultRanker), ctx)
    });
    // Problems in the theme are not reported here: anything printed now
    // would land in the middle of the prompt. `zray theme` reports them.
    let (theme, _) = Theme::from_env();
    let outcome = tui::run(
        Sources {
            commands,
            directories,
        },
        tui::Options {
            query,
            cursor,
            prompt,
            max_items,
            theme,
        },
    )?;

    // First line is the action, the rest is the command text.
    let result = match outcome {
        Outcome::Execute(text) => format!("execute\n{text}\n"),
        Outcome::Edit(text) => format!("edit\n{text}\n"),
        Outcome::ChangeDirectory(path) => format!("cd\n{path}\n"),
        Outcome::EditPath(path) => format!("edit-path\n{path}\n"),
        Outcome::Cancel => "cancel\n".to_string(),
    };
    match result_fd {
        Some(fd) => {
            if fd < 3 {
                bail!("--result-fd must not be a standard stream");
            }
            // SAFETY: the caller passed this descriptor for us to write the
            // result to and close; nothing else in this process uses it.
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(result.as_bytes())
                .context("writing result")?;
        }
        None => print!("{result}"),
    }
    Ok(())
}

fn show_theme() -> Result<()> {
    let (theme, problems) = Theme::from_env();
    let defaults = Theme::default();
    for token in Theme::TOKENS {
        let color = theme.get(token).context("unknown theme token")?;
        let changed = Some(color) != defaults.get(token);
        // `selected-bg` is a background; the others are foregrounds.
        let sgr = theme::sgr(color, token.ends_with("-bg"));
        let sample = format!("\x1b[{sgr}m  sample  \x1b[0m");
        let name = theme::color_name(color);
        println!(
            "{token:<12} {sample} {name}{}",
            if changed { "  (set)" } else { "" }
        );
    }
    if !problems.is_empty() {
        eprintln!("\nproblems in ${THEME_ENV}:");
        for p in &problems {
            eprintln!("  {p}");
        }
        std::process::exit(1);
    }
    Ok(())
}

fn import(file: Option<PathBuf>) -> Result<()> {
    let path = match file {
        Some(p) => p,
        None => match std::env::var_os("HISTFILE").filter(|p| !p.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".zsh_history"),
        },
    };
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    // Entries without timestamps are dated relative to the file's mtime so
    // that importing an unchanged file twice adds nothing.
    let mtime_ms = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or_else(now_ms, |d| d.as_millis() as i64);
    let entries = zray::history::parse(&raw, mtime_ms);
    let added = Store::open_default()?.import(&entries)?;
    println!(
        "imported {added} of {} entries from {}",
        entries.len(),
        path.display()
    );
    Ok(())
}

use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use zray::matcher::Engine;
use zray::ranker::{DefaultRanker, RankContext};
use zray::store::{Store, now_ms};
use zray::tui::{self, Outcome};

const ZSH_INIT: &str = include_str!("../shell/zray.zsh");

/// How many distinct commands a search considers.
const CANDIDATE_LIMIT: usize = 50_000;

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
        /// Directory to rank for; defaults to the current directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Last line of the shell prompt (may contain ANSI escapes).
        #[arg(long, default_value = "> ")]
        prompt: String,
        /// Viewport height in rows, input line included.
        #[arg(long, env = "ZRAY_HEIGHT", default_value_t = 10)]
        height: u16,
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
        Command::Search { query, cwd, prompt, height, result_fd } => {
            search(query, cwd, prompt, height, result_fd)
        }
        Command::Record { action: Record::Start { cwd, session, host, text } } => {
            let id = Store::open_default()?.record_start(
                &text,
                cwd.as_deref(),
                session.as_deref(),
                host.as_deref(),
            )?;
            println!("{id}");
            Ok(())
        }
        Command::Record { action: Record::End { id, exit } } => {
            Store::open_default()?.record_end(id, exit)
        }
        Command::Import { file } => import(file),
    }
}

fn init_zsh() -> Result<()> {
    let exe = std::env::current_exe().context("locating the zray binary")?;
    let exe = exe.to_str().context("zray binary path is not valid UTF-8")?;
    // Single-quote for zsh.
    let quoted = format!("'{}'", exe.replace('\'', r"'\''"));
    print!("{}", ZSH_INIT.replace("__ZRAY_BIN__", &quoted));
    Ok(())
}

fn search(
    query: String,
    cwd: Option<String>,
    prompt: String,
    height: u16,
    result_fd: Option<RawFd>,
) -> Result<()> {
    let cwd = cwd.or_else(|| {
        std::env::current_dir().ok().and_then(|p| p.to_str().map(str::to_string))
    });
    let rows = Store::open_default()?.load_commands(cwd.as_deref(), CANDIDATE_LIMIT)?;
    let mut engine =
        Engine::new(rows, Box::new(DefaultRanker), RankContext { now_ms: now_ms() });
    let outcome = tui::run(&mut engine, tui::Options { query, prompt, height })?;

    // First line is the action, the rest is the command text.
    let result = match outcome {
        Outcome::Execute(text) => format!("execute\n{text}\n"),
        Outcome::Edit(text) => format!("edit\n{text}\n"),
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
            file.write_all(result.as_bytes()).context("writing result")?;
        }
        None => print!("{result}"),
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
    println!("imported {added} of {} entries from {}", entries.len(), path.display());
    Ok(())
}

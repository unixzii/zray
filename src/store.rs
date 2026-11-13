//! SQLite-backed history store.
//!
//! `commands` holds one row per distinct command line with aggregate stats;
//! `executions` holds one row per run.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Session id used for rows imported from a zsh history file.
const IMPORT_SESSION: &str = "import";

const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE commands (
         id           INTEGER PRIMARY KEY,
         text         TEXT NOT NULL UNIQUE,
         count        INTEGER NOT NULL DEFAULT 0,
         last_used_at INTEGER NOT NULL DEFAULT 0,
         last_exit    INTEGER
     );
     CREATE TABLE executions (
         id          INTEGER PRIMARY KEY,
         command_id  INTEGER NOT NULL REFERENCES commands(id),
         started_at  INTEGER NOT NULL,
         duration_ms INTEGER,
         exit_code   INTEGER,
         cwd         TEXT,
         session_id  TEXT,
         hostname    TEXT
     );
     CREATE INDEX executions_command ON executions(command_id, started_at);
     CREATE INDEX executions_cwd ON executions(cwd, command_id);
     CREATE INDEX commands_last_used ON commands(last_used_at DESC);",
];

/// A distinct command line plus the stats the ranker works with.
#[derive(Debug, Clone)]
pub struct CommandRow {
    pub id: i64,
    pub text: String,
    pub count: u32,
    /// Milliseconds since the Unix epoch.
    pub last_used_at: i64,
    pub last_exit: Option<i32>,
    /// How many times this command ran in the directory being searched from.
    pub cwd_count: u32,
}

pub struct Store {
    conn: Connection,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    /// `$ZRAY_DB`, else `$XDG_DATA_HOME/zray/history.db`, else
    /// `~/.local/share/zray/history.db`.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("ZRAY_DB").filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(p));
        }
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".local/share")
            }
        };
        Ok(base.join("zray/history.db"))
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&Self::default_path()?)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // Several shells write concurrently; wait instead of failing.
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: usize = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?
            as usize;
        if version >= MIGRATIONS.len() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for sql in &MIGRATIONS[version..] {
            tx.execute_batch(sql)?;
        }
        tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
        tx.commit()?;
        Ok(())
    }

    /// Records that `text` started running now; returns the execution id to
    /// pass to [`Store::record_end`].
    pub fn record_start(
        &mut self,
        text: &str,
        cwd: Option<&str>,
        session: Option<&str>,
        hostname: Option<&str>,
    ) -> Result<i64> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let command_id: i64 = tx.query_row(
            "INSERT INTO commands (text, count, last_used_at) VALUES (?1, 1, ?2)
             ON CONFLICT(text) DO UPDATE SET
                 count = count + 1,
                 last_used_at = max(last_used_at, excluded.last_used_at)
             RETURNING id",
            params![text, now],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO executions (command_id, started_at, cwd, session_id, hostname)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![command_id, now, cwd, session, hostname],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Fills in the exit code and duration of an execution.
    pub fn record_end(&mut self, id: i64, exit_code: i32) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE executions SET exit_code = ?2, duration_ms = max(0, ?3 - started_at)
             WHERE id = ?1",
            params![id, exit_code, now],
        )?;
        tx.execute(
            "UPDATE commands SET last_exit = ?2
             WHERE id = (SELECT command_id FROM executions WHERE id = ?1)",
            params![id, exit_code],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Imports history entries; re-importing the same entries is a no-op.
    /// Returns how many executions were added.
    pub fn import(&mut self, entries: &[crate::history::Entry]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut added = 0;
        {
            let mut insert_cmd =
                tx.prepare("INSERT OR IGNORE INTO commands (text) VALUES (?1)")?;
            let mut select_cmd = tx.prepare("SELECT id FROM commands WHERE text = ?1")?;
            let mut exists = tx.prepare(
                "SELECT 1 FROM executions
                 WHERE command_id = ?1 AND started_at = ?2 AND session_id = ?3",
            )?;
            let mut insert_exec = tx.prepare(
                "INSERT INTO executions (command_id, started_at, duration_ms, session_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut bump = tx.prepare(
                "UPDATE commands SET count = count + 1, last_used_at = max(last_used_at, ?2)
                 WHERE id = ?1",
            )?;
            for e in entries {
                insert_cmd.execute(params![e.text])?;
                let command_id: i64 = select_cmd.query_row(params![e.text], |r| r.get(0))?;
                let seen = exists
                    .query_row(params![command_id, e.started_at, IMPORT_SESSION], |_| Ok(()))
                    .optional()?
                    .is_some();
                if seen {
                    continue;
                }
                insert_exec.execute(params![
                    command_id,
                    e.started_at,
                    e.duration_ms,
                    IMPORT_SESSION
                ])?;
                bump.execute(params![command_id, e.started_at])?;
                added += 1;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// The `limit` most recently used distinct commands, newest first.
    pub fn load_commands(&self, cwd: Option<&str>, limit: usize) -> Result<Vec<CommandRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.text, c.count, c.last_used_at, c.last_exit, coalesce(d.n, 0)
             FROM commands c
             LEFT JOIN (
                 SELECT command_id, count(*) AS n FROM executions
                 WHERE cwd = ?1 GROUP BY command_id
             ) d ON d.command_id = c.id
             ORDER BY c.last_used_at DESC, c.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cwd, limit as i64], |r| {
            Ok(CommandRow {
                id: r.get(0)?,
                text: r.get(1)?,
                count: r.get(2)?,
                last_used_at: r.get(3)?,
                last_exit: r.get(4)?,
                cwd_count: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Entry;

    #[test]
    fn record_dedups_and_tracks_stats() {
        let mut s = Store::open_in_memory().unwrap();
        let a = s.record_start("git status", Some("/repo"), Some("s1"), None).unwrap();
        s.record_end(a, 0).unwrap();
        let b = s.record_start("git status", Some("/other"), Some("s1"), None).unwrap();
        s.record_end(b, 1).unwrap();
        s.record_start("ls", Some("/repo"), Some("s1"), None).unwrap();

        let rows = s.load_commands(Some("/repo"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        let git = rows.iter().find(|r| r.text == "git status").unwrap();
        assert_eq!(git.count, 2);
        assert_eq!(git.last_exit, Some(1));
        assert_eq!(git.cwd_count, 1);
        let ls = rows.iter().find(|r| r.text == "ls").unwrap();
        assert_eq!(ls.last_exit, None);
    }

    #[test]
    fn import_is_idempotent() {
        let mut s = Store::open_in_memory().unwrap();
        let entries = vec![
            Entry { text: "make".into(), started_at: 1000, duration_ms: Some(5) },
            Entry { text: "make".into(), started_at: 2000, duration_ms: None },
            Entry { text: "ls".into(), started_at: 3000, duration_ms: None },
        ];
        assert_eq!(s.import(&entries).unwrap(), 3);
        assert_eq!(s.import(&entries).unwrap(), 0);
        let rows = s.load_commands(None, 10).unwrap();
        assert_eq!(rows[0].text, "ls");
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[1].last_used_at, 2000);
    }
}

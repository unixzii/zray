//! The unit of search: something that can be listed, matched and picked.

/// A command line or a directory path, with the usage stats ranking uses.
#[derive(Debug, Clone)]
pub struct Item {
    /// What is shown and matched against.
    pub text: String,
    /// How many times it was used.
    pub count: u32,
    /// Milliseconds since the Unix epoch.
    pub last_used_at: i64,
    /// Exit code of the last run; `None` if unknown or not applicable.
    pub last_exit: Option<i32>,
    /// How many times it was used in the directory being searched from.
    pub cwd_count: u32,
}

impl Item {
    /// Whether the last run failed. Exit code 130 is Ctrl-C, which says
    /// nothing about the command being wrong.
    pub fn failed(&self) -> bool {
        matches!(self.last_exit, Some(code) if code != 0 && code != 130)
    }
}

/// One execution of a command, as shown under the selected candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Milliseconds since the Unix epoch.
    pub started_at: i64,
    /// `None` if it has not finished, or was imported without a duration.
    pub duration_ms: Option<i64>,
    /// `None` if it has not finished, or was imported.
    pub exit_code: Option<i32>,
    /// Working directory; `None` for imported runs.
    pub cwd: Option<String>,
    /// Machine it ran on; `None` if unknown.
    pub hostname: Option<String>,
}

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

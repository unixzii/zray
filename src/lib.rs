//! zray: interactive shell history search.
//!
//! Everything except `tui` is terminal-independent.

pub mod history;
pub mod item;
pub mod matcher;
pub mod ranker;
pub mod store;
pub mod term;
pub mod theme;
pub mod tui;

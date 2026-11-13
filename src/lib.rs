//! zray: interactive shell history search.
//!
//! `store`, `history`, `matcher` and `ranker` are terminal-independent;
//! only `tui` touches the tty.

pub mod history;
pub mod matcher;
pub mod ranker;
pub mod store;
pub mod tui;

//! Ranking: turns a fuzzy score plus usage stats into a final score.
//!
//! This is the extension point for smarter ranking; the search engine only
//! depends on the [`Ranker`] trait.

use crate::item::Item;

/// Facts about the search that are constant across candidates.
#[derive(Debug, Clone)]
pub struct RankContext {
    pub now_ms: i64,
}

pub trait Ranker {
    /// Higher is better. `fuzzy` is `None` when the query is empty, in which
    /// case every candidate is a match.
    fn score(&self, row: &Item, fuzzy: Option<u32>, ctx: &RankContext) -> f64;
}

/// Empty query: strictly newest first, so Up + Enter behaves like the stock
/// shell. Otherwise: fuzzy score nudged by frecency, by whether the command
/// has been run in the current directory, and by whether it last failed.
///
/// The bonuses are small relative to nucleo's scores (16 per matched char
/// plus adjacency/boundary bonuses), so they reorder near-ties rather than
/// override match quality.
pub struct DefaultRanker;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

impl DefaultRanker {
    fn recency_weight(age_ms: i64) -> f64 {
        match age_ms {
            a if a < HOUR_MS => 1.0,
            a if a < DAY_MS => 0.8,
            a if a < 7 * DAY_MS => 0.5,
            a if a < 30 * DAY_MS => 0.3,
            _ => 0.1,
        }
    }
}

impl Ranker for DefaultRanker {
    fn score(&self, row: &Item, fuzzy: Option<u32>, ctx: &RankContext) -> f64 {
        let Some(fuzzy) = fuzzy else {
            return row.last_used_at as f64;
        };
        let age = (ctx.now_ms - row.last_used_at).max(0);
        let frecency =
            Self::recency_weight(age) * (4.0 + 3.0 * f64::from(row.count).ln_1p().min(4.0));
        let cwd = if row.cwd_count > 0 { 6.0 } else { 0.0 };
        // 130 is Ctrl-C, which says nothing about the command being wrong.
        let failed = match row.last_exit {
            Some(code) if code != 0 && code != 130 => -4.0,
            _ => 0.0,
        };
        f64::from(fuzzy) + frecency + cwd + failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(count: u32, last_used_at: i64, last_exit: Option<i32>, cwd_count: u32) -> Item {
        Item { text: String::new(), count, last_used_at, last_exit, cwd_count }
    }

    #[test]
    fn empty_query_is_pure_recency() {
        let ctx = RankContext { now_ms: 10 * DAY_MS };
        let old_popular = row(500, DAY_MS, Some(0), 9);
        let new_rare = row(1, 2 * DAY_MS, Some(1), 0);
        assert!(
            DefaultRanker.score(&new_rare, None, &ctx)
                > DefaultRanker.score(&old_popular, None, &ctx)
        );
    }

    #[test]
    fn bonuses_break_ties() {
        let ctx = RankContext { now_ms: 100 * DAY_MS };
        let base = row(1, 0, Some(0), 0);
        let s = |r: &Item| DefaultRanker.score(r, Some(100), &ctx);
        assert!(s(&row(1, 100 * DAY_MS, Some(0), 0)) > s(&base), "recent wins");
        assert!(s(&row(50, 0, Some(0), 0)) > s(&base), "frequent wins");
        assert!(s(&row(1, 0, Some(0), 3)) > s(&base), "same cwd wins");
        assert!(s(&row(1, 0, Some(1), 0)) < s(&base), "failed loses");
        assert_eq!(s(&row(1, 0, Some(130), 0)), s(&base), "ctrl-c is neutral");
    }
}

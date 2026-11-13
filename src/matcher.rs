//! In-memory fuzzy search over the loaded commands.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};

use crate::item::Item;
use crate::ranker::{RankContext, Ranker};

struct Candidate {
    row: Item,
    haystack: Utf32String,
}

/// A candidate that matched the current query.
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    /// Index into the engine's candidate list; pass to [`Engine::row`].
    pub index: usize,
    pub score: f64,
}

pub struct Engine {
    candidates: Vec<Candidate>,
    matcher: Matcher,
    ranker: Box<dyn Ranker>,
    ctx: RankContext,
}

impl Engine {
    pub fn new(rows: Vec<Item>, ranker: Box<dyn Ranker>, ctx: RankContext) -> Self {
        Self::with_config(rows, ranker, ctx, Config::DEFAULT)
    }

    /// Like [`Engine::new`], but scores matches the way that suits file
    /// paths: a match right after a `/` counts as the start of a word.
    pub fn for_paths(rows: Vec<Item>, ranker: Box<dyn Ranker>, ctx: RankContext) -> Self {
        Self::with_config(rows, ranker, ctx, Config::DEFAULT.match_paths())
    }

    fn with_config(
        rows: Vec<Item>,
        ranker: Box<dyn Ranker>,
        ctx: RankContext,
        config: Config,
    ) -> Self {
        let candidates = rows
            .into_iter()
            .map(|row| Candidate {
                haystack: Utf32String::from(row.text.as_str()),
                row,
            })
            .collect();
        Engine {
            candidates,
            matcher: Matcher::new(config),
            ranker,
            ctx,
        }
    }

    /// The time the search started; ages are shown relative to it.
    pub fn now_ms(&self) -> i64 {
        self.ctx.now_ms
    }

    pub fn row(&self, index: usize) -> &Item {
        &self.candidates[index].row
    }

    fn pattern(query: &str) -> Pattern {
        Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
    }

    /// Matching candidates, best first.
    pub fn search(&mut self, query: &str) -> Vec<Hit> {
        let pattern = (!query.trim().is_empty()).then(|| Self::pattern(query));
        let mut hits: Vec<Hit> = Vec::new();
        for (index, c) in self.candidates.iter().enumerate() {
            let fuzzy = match &pattern {
                None => None,
                Some(p) => match p.score(c.haystack.slice(..), &mut self.matcher) {
                    Some(s) => Some(s),
                    None => continue,
                },
            };
            hits.push(Hit {
                index,
                score: self.ranker.score(&c.row, fuzzy, &self.ctx),
            });
        }
        // Candidates are loaded newest first, so the stable sort leaves equal
        // scores in recency order.
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits
    }

    /// Positions (in chars) of `index`'s text that the query matched, sorted.
    pub fn match_positions(&mut self, query: &str, index: usize) -> Vec<u32> {
        let mut positions = Vec::new();
        if query.trim().is_empty() {
            return positions;
        }
        let haystack = self.candidates[index].haystack.slice(..);
        Self::pattern(query).indices(haystack, &mut self.matcher, &mut positions);
        positions.sort_unstable();
        positions.dedup();
        positions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranker::DefaultRanker;

    fn engine(texts: &[&str]) -> Engine {
        // Listed newest first, like `Store::load_commands` returns them.
        let n = texts.len() as i64;
        let rows = texts
            .iter()
            .enumerate()
            .map(|(i, t)| Item {
                text: t.to_string(),
                count: 1,
                last_used_at: (n - i as i64) * 1000,
                last_exit: Some(0),
                cwd_count: 0,
            })
            .collect();
        Engine::new(
            rows,
            Box::new(DefaultRanker),
            RankContext { now_ms: n * 1000 },
        )
    }

    fn texts(e: &mut Engine, query: &str) -> Vec<String> {
        e.search(query)
            .iter()
            .map(|h| e.row(h.index).text.clone())
            .collect()
    }

    #[test]
    fn empty_query_lists_everything_newest_first() {
        let mut e = engine(&["c", "b", "a"]);
        assert_eq!(texts(&mut e, ""), ["c", "b", "a"]);
    }

    #[test]
    fn fuzzy_filters_and_prefers_tighter_matches() {
        let mut e = engine(&["cargo build", "grep -i foo", "git init", "ls"]);
        let got = texts(&mut e, "gi");
        assert_eq!(got[0], "git init");
        assert!(got.contains(&"grep -i foo".to_string()));
        assert!(!got.contains(&"ls".to_string()));
    }

    #[test]
    fn match_positions_point_at_matched_chars() {
        let mut e = engine(&["git push"]);
        assert_eq!(e.match_positions("gp", 0), [0, 4]);
        assert!(e.match_positions("", 0).is_empty());
    }
}

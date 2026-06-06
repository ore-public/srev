//! ペイン内インラインフィルタ用の、使い回せるあいまいランキング。

use std::cmp::Reverse;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

pub struct Fuzzy {
    matcher: Matcher,
}

impl Fuzzy {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }

    /// `query` で `items` を絞り込み、スコア降順のインデックス列を返す。
    /// 空クエリならそのままの順序で全件。
    pub fn rank(&mut self, query: &str, items: &[&str]) -> Vec<usize> {
        if query.is_empty() {
            return (0..items.len()).collect();
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                pattern
                    .score(Utf32Str::new(s, &mut buf), &mut self.matcher)
                    .map(|sc| (i, sc))
            })
            .collect();
        scored.sort_by_key(|&(_, s)| Reverse(s));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all_in_order() {
        let mut f = Fuzzy::new();
        let items = ["a", "b", "c"];
        assert_eq!(f.rank("", &items), vec![0, 1, 2]);
    }

    #[test]
    fn ranks_better_match_first() {
        let mut f = Fuzzy::new();
        let items = ["foobar", "xfx", "fb"];
        let r = f.rank("fb", &items);
        // "fb" と "foobar" がマッチ、"xfx" は不一致で除外
        assert!(!r.is_empty());
        assert!(r.iter().all(|&i| items[i] != "xfx"));
    }
}

//! nucleo を用いたファイル名あいまい検索オーバーレイ。

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// 1 ファイルぶんの索引項目。ツリーフィルタなどでも共有する。
#[derive(Clone)]
pub struct FileEntry {
    /// 表示・マッチ対象（root からの相対パス）。
    pub rel: String,
    /// 開くときの絶対パス。
    pub abs: PathBuf,
}

/// root 配下の（gitignore を尊重した）ファイル一覧を収集する。
pub fn collect_files(root: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let Ok(entry) = result else { continue };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .to_string();
        entries.push(FileEntry { rel, abs });
    }
    entries
}

pub struct Finder {
    pub active: bool,
    pub query: String,
    pub selected: usize,
    /// `results` は entries へのインデックス（スコア降順）。
    pub(crate) results: Vec<usize>,
    entries: Vec<FileEntry>,
    matcher: Matcher,
}

impl Finder {
    /// 収集済みのファイル一覧から構築する。
    pub fn from_files(entries: Vec<FileEntry>) -> Self {
        let mut finder = Self {
            active: false,
            query: String::new(),
            selected: 0,
            results: Vec::new(),
            entries,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        };
        finder.recompute();
        finder
    }

    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.selected = 0;
        self.recompute();
    }

    pub fn close(&mut self) {
        self.active = false;
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.recompute();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.recompute();
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// 現在選択中の候補の絶対パス。
    pub fn selected_path(&self) -> Option<PathBuf> {
        let idx = *self.results.get(self.selected)?;
        Some(self.entries[idx].abs.clone())
    }

    /// 表示用に、上位 `limit` 件の相対パスを返す。
    pub fn visible(&self, limit: usize) -> Vec<(&str, bool)> {
        self.results
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, &idx)| (self.entries[idx].rel.as_str(), i == self.selected))
            .collect()
    }

    fn recompute(&mut self) {
        if self.query.is_empty() {
            self.results = (0..self.entries.len()).collect();
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                pattern
                    .score(Utf32Str::new(&e.rel, &mut buf), &mut self.matcher)
                    .map(|s| (i, s))
            })
            .collect();
        scored.sort_by_key(|&(_, s)| Reverse(s));
        self.results = scored.into_iter().map(|(i, _)| i).collect();
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranks_matching_paths_first() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut finder = Finder::from_files(collect_files(root.as_path()));
        finder.open();
        for c in "highlight".chars() {
            finder.push_char(c);
        }
        let top = finder.selected_path().expect("a match");
        assert!(
            top.file_name().unwrap().to_string_lossy().contains("highlight"),
            "top result was {top:?}"
        );
    }
}

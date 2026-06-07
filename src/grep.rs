//! プロジェクト全体の本文検索（部分一致・大文字小文字無視）。
//!
//! `Ctrl-F` で開き、入力に応じて全ファイルの行を検索、結果から該当行へ飛ぶ。
//! 開くたびにファイルを読み直すので、編集後の内容も反映される。

use std::path::PathBuf;

use crate::finder::FileEntry;

/// 結果が多すぎる場合の打ち切り。
const MAX_RESULTS: usize = 1000;
/// プレビュー行の最大文字数。
const PREVIEW_CHARS: usize = 300;

/// 読み込み済みファイル（行に分割してキャッシュ）。
struct Doc {
    rel: String,
    abs: PathBuf,
    lines: Vec<String>,
}

/// 1 件のマッチ。
pub struct Hit {
    pub abs: PathBuf,
    pub rel: String,
    /// 0 始まりの行番号。
    pub line: usize,
    pub preview: String,
}

pub struct ProjectSearch {
    pub active: bool,
    pub query: String,
    pub selected: usize,
    sources: Vec<FileEntry>,
    docs: Vec<Doc>,
    results: Vec<Hit>,
    truncated: bool,
}

impl ProjectSearch {
    pub fn new(files: &[FileEntry]) -> Self {
        Self {
            active: false,
            query: String::new(),
            selected: 0,
            sources: files.to_vec(),
            docs: Vec::new(),
            results: Vec::new(),
            truncated: false,
        }
    }

    /// 開くたびにファイルを読み直す（最新内容で検索）。
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.selected = 0;
        self.results.clear();
        self.truncated = false;
        self.docs = self
            .sources
            .iter()
            .filter_map(|e| {
                std::fs::read_to_string(&e.abs).ok().map(|content| Doc {
                    rel: e.rel.clone(),
                    abs: e.abs.clone(),
                    lines: content.lines().map(|l| l.to_string()).collect(),
                })
            })
            .collect();
    }

    pub fn close(&mut self) {
        self.active = false;
        self.docs = Vec::new(); // メモリを解放
        self.results = Vec::new();
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

    /// 選択中マッチの (絶対パス, 行)。
    pub fn selected_target(&self) -> Option<(PathBuf, usize)> {
        self.results.get(self.selected).map(|h| (h.abs.clone(), h.line))
    }

    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// 表示用に上位 `limit` 件の (マッチ, 選択中か) を返す。
    pub fn visible(&self, limit: usize) -> Vec<(&Hit, bool)> {
        self.results
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, h)| (h, i == self.selected))
            .collect()
    }

    fn recompute(&mut self) {
        self.results.clear();
        self.truncated = false;
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        'outer: for doc in &self.docs {
            for (i, line) in doc.lines.iter().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    self.results.push(Hit {
                        abs: doc.abs.clone(),
                        rel: doc.rel.clone(),
                        line: i,
                        preview: line.trim_start().chars().take(PREVIEW_CHARS).collect(),
                    });
                    if self.results.len() >= MAX_RESULTS {
                        self.truncated = true;
                        break 'outer;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn finds_substring_case_insensitive_across_files() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let files = crate::finder::collect_files(&root, &|_| false);
        let mut s = ProjectSearch::new(&files);
        s.open();
        for c in "projectsearch".chars() {
            s.push_char(c);
        }
        // 大文字小文字無視で構造体名 ProjectSearch がヒットするはず。
        assert!(s.result_count() > 0, "expected hits for 'projectsearch'");
        let (path, _line) = s.selected_target().expect("a target");
        assert_eq!(path.extension().unwrap(), "rs");
    }

    #[test]
    fn empty_query_has_no_results() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let files = crate::finder::collect_files(&root, &|_| false);
        let mut s = ProjectSearch::new(&files);
        s.open();
        assert_eq!(s.result_count(), 0);
    }
}

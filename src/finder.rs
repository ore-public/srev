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
    /// `.gitignore` 等で無視対象か（表示色を変える）。
    pub ignored: bool,
}

/// 走査で得た 1 エントリ（ファイル/ディレクトリ）。
pub struct Entry {
    pub abs: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

/// root 配下を走査する。`.gitignore` 対象でも**ファイルは表示**し（色分け用に
/// `ignored` を立てる）、無視対象の**ディレクトリには降りない**（target/ 等の洪水回避）。
/// ドットファイルは表示、`.git` ディレクトリは常に除外。
pub fn walk_visible(root: &Path, is_ignored: &dyn Fn(&Path) -> bool) -> Vec<Entry> {
    let mut out = Vec::new();
    visit_dir(root, is_ignored, false, &mut out);
    out
}

fn visit_dir(dir: &Path, is_ignored: &dyn Fn(&Path) -> bool, parent_ignored: bool, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kids: Vec<(PathBuf, bool)> = rd
        .flatten()
        .filter(|e| e.file_name() != ".git")
        .map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (e.path(), is_dir)
        })
        .collect();
    // ディレクトリ優先 → 名前順。
    kids.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(a.0.file_name().cmp(&b.0.file_name()))
    });
    for (abs, is_dir) in kids {
        let ignored = parent_ignored || is_ignored(&abs);
        out.push(Entry {
            abs: abs.clone(),
            is_dir,
            ignored,
        });
        // 無視対象ディレクトリの中身は出さない（洪水回避）。
        if is_dir && !ignored {
            visit_dir(&abs, is_ignored, ignored, out);
        }
    }
}

/// 共通のファイル走査ビルダ。`.gitignore` は尊重しつつ、ドットファイルも
/// 表示する（`.hidden(false)`）。ただし `.git` ディレクトリは除外。
pub fn walker(root: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.hidden(false)
        .filter_entry(|e| e.file_name().to_str() != Some(".git"));
    b
}

/// root 配下のファイル一覧を収集する。無視対象ファイルも含む（`ignored` で識別）。
pub fn collect_files(root: &Path, is_ignored: &dyn Fn(&Path) -> bool) -> Vec<FileEntry> {
    walk_visible(root, is_ignored)
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| {
            let rel = e
                .abs
                .strip_prefix(root)
                .unwrap_or(&e.abs)
                .to_string_lossy()
                .to_string();
            FileEntry {
                rel,
                abs: e.abs,
                ignored: e.ignored,
            }
        })
        .collect()
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
    /// (相対パス, 選択中か, 無視対象か) を返す。
    pub fn visible(&self, limit: usize) -> Vec<(&str, bool, bool)> {
        self.results
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, &idx)| {
                let e = &self.entries[idx];
                (e.rel.as_str(), i == self.selected, e.ignored)
            })
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
        let mut finder = Finder::from_files(collect_files(root.as_path(), &|_| false));
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

    #[test]
    fn collect_files_includes_dotfiles_but_not_git() {
        let dir = std::env::temp_dir().join(format!("srev_dot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "x").unwrap();
        std::fs::write(dir.join("normal.txt"), "y").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("config"), "z").unwrap();

        let rels: Vec<String> = collect_files(&dir, &|_| false)
            .into_iter()
            .map(|f| f.rel)
            .collect();
        assert!(rels.iter().any(|r| r == ".env"), "dotfile missing: {rels:?}");
        assert!(rels.iter().any(|r| r == "normal.txt"));
        assert!(
            !rels.iter().any(|r| r.contains(".git")),
            ".git should be excluded: {rels:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignored_files_shown_but_ignored_dirs_not_descended() {
        let dir = std::env::temp_dir().join(format!("srev_ign_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("ignore_dir")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("ignored.txt"), "i").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("ignore_dir").join("inner.txt"), "x").unwrap();

        let is_ignored = |p: &Path| {
            matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("ignored.txt") | Some("ignore_dir")
            )
        };
        let files = collect_files(&dir, &is_ignored);
        let find = |rel: &str| files.iter().find(|f| f.rel == rel);

        assert!(find("a.txt").is_some_and(|f| !f.ignored));
        assert!(find("ignored.txt").is_some_and(|f| f.ignored), "ignored flag");
        assert!(find("sub/b.txt").is_some(), "normal subdir descended");
        assert!(
            find("ignore_dir/inner.txt").is_none(),
            "ignored dir should not be descended"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

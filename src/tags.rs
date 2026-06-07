//! tree-sitter-tags を用いたシンボル抽出。
//!
//! - [`LangConfigs::file_symbols`]: 1 ファイルの定義一覧（左下アウトライン用）
//! - [`ProjectIndex`]: プロジェクト全体の name → 定義位置（`gd` ジャンプ用、別スレッド構築）

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;

use ignore::WalkBuilder;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

/// 抽出された 1 つの定義。
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    /// 0 始まりの行番号。
    pub line: usize,
    /// 0 始まりの列（名前の開始位置、バイト基準）。ジャンプ時にカーソルを名前へ。
    pub col: usize,
}

/// 対応言語の tags 設定一式。`TagsConfiguration` は `Send`/`Sync` でないため
/// static には置けず、`App` が所有して使い回す。
pub struct LangConfigs {
    rust: Option<TagsConfiguration>,
    python: Option<TagsConfiguration>,
    javascript: Option<TagsConfiguration>,
    go: Option<TagsConfiguration>,
    ruby: Option<TagsConfiguration>,
    c: Option<TagsConfiguration>,
}

impl LangConfigs {
    pub fn new() -> Self {
        fn cfg(lang: tree_sitter::Language, query: &str) -> Option<TagsConfiguration> {
            TagsConfiguration::new(lang, query, "").ok()
        }
        Self {
            rust: cfg(tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::TAGS_QUERY),
            python: cfg(tree_sitter_python::LANGUAGE.into(), tree_sitter_python::TAGS_QUERY),
            javascript: cfg(
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::TAGS_QUERY,
            ),
            go: cfg(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY),
            ruby: cfg(tree_sitter_ruby::LANGUAGE.into(), tree_sitter_ruby::TAGS_QUERY),
            c: cfg(tree_sitter_c::LANGUAGE.into(), tree_sitter_c::TAGS_QUERY),
        }
    }

    fn for_ext(&self, ext: &str) -> Option<&TagsConfiguration> {
        let cell = match ext {
            "rs" => &self.rust,
            "py" => &self.python,
            "js" | "jsx" | "mjs" | "cjs" => &self.javascript,
            "go" => &self.go,
            "rb" => &self.ruby,
            "c" | "h" => &self.c,
            _ => return None,
        };
        cell.as_ref()
    }

    fn config_for(&self, path: &Path) -> Option<&TagsConfiguration> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.for_ext(ext))
    }

    /// 1 ファイルの定義一覧を行順で返す（アウトライン用）。
    pub fn file_symbols(&self, path: &Path, source: &[u8]) -> Vec<Symbol> {
        let Some(config) = self.config_for(path) else {
            return Vec::new();
        };
        let mut ctx = TagsContext::new();
        let Ok((tags, _)) = ctx.generate_tags(config, source, None) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for tag in tags.flatten() {
            if !tag.is_definition {
                continue;
            }
            out.push(Symbol {
                name: String::from_utf8_lossy(source.get(tag.name_range).unwrap_or_default())
                    .to_string(),
                kind: config.syntax_type_name(tag.syntax_type_id).to_string(),
                line: tag.span.start.row,
                col: tag.span.start.column,
            });
        }
        out.sort_by_key(|s| s.line);
        out
    }

    /// 1 ファイルの全タグを (名前, 0始まり行, 0始まり列, 定義か) で返す（索引構築用）。
    /// `is_definition=false` は参照（呼び出し箇所など）。
    pub fn file_tags(&self, path: &Path, source: &[u8]) -> Vec<(String, usize, usize, bool)> {
        let Some(config) = self.config_for(path) else {
            return Vec::new();
        };
        let mut ctx = TagsContext::new();
        let Ok((tags, _)) = ctx.generate_tags(config, source, None) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for tag in tags.flatten() {
            let name = String::from_utf8_lossy(source.get(tag.name_range).unwrap_or_default())
                .to_string();
            out.push((name, tag.span.start.row, tag.span.start.column, tag.is_definition));
        }
        out
    }
}

/// プロジェクト全体の索引エントリ（名前, ファイル, 0始まり行, 0始まり列）。
type Def = (String, PathBuf, usize, usize);

/// 定義（`gd` 用）と参照（`gr` 用）の索引。
struct Index {
    defs: Vec<Def>,
    refs: Vec<Def>,
}

enum State {
    Idle,
    Building(Receiver<Index>),
    Ready(Index),
}

/// プロジェクト全体の定義索引（`gd` 用）。別スレッドで構築し UI を止めない。
pub struct ProjectIndex {
    root: PathBuf,
    state: State,
}

impl ProjectIndex {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            state: State::Idle,
        }
    }

    /// バックグラウンドで索引構築を開始する（未開始時のみ）。
    pub fn start(&mut self) {
        if matches!(self.state, State::Idle) {
            let (tx, rx) = channel();
            let root = self.root.clone();
            thread::spawn(move || {
                // LangConfigs は Send でないためスレッド内で生成する。
                let configs = LangConfigs::new();
                let _ = tx.send(collect_index(&root, &configs));
            });
            self.state = State::Building(rx);
        }
    }

    /// スレッド完了を取り込む（毎フレーム呼ぶ）。完了した瞬間だけ `true`。
    pub fn poll(&mut self) -> bool {
        if let State::Building(rx) = &self.state {
            match rx.try_recv() {
                Ok(index) => {
                    self.state = State::Ready(index);
                    return true;
                }
                Err(TryRecvError::Empty) => {}
                // スレッドが落ちた等。空の Ready にして無限待ちを避ける。
                Err(TryRecvError::Disconnected) => {
                    self.state = State::Ready(Index {
                        defs: Vec::new(),
                        refs: Vec::new(),
                    });
                    return true;
                }
            }
        }
        false
    }

    pub fn is_building(&self) -> bool {
        matches!(self.state, State::Building(_))
    }

    /// 名前に一致する定義位置 (パス, 行, 列)（索引が未完了なら `None`）。
    pub fn definition(&self, name: &str) -> Option<(PathBuf, usize, usize)> {
        match &self.state {
            State::Ready(idx) => idx
                .defs
                .iter()
                .find(|(n, _, _, _)| n == name)
                .map(|(_, p, l, c)| (p.clone(), *l, *c)),
            _ => None,
        }
    }

    /// 名前を参照している箇所（呼び出し元など）の一覧 (パス, 行, 列)。未完了なら空。
    pub fn references(&self, name: &str) -> Vec<(PathBuf, usize, usize)> {
        match &self.state {
            State::Ready(idx) => idx
                .refs
                .iter()
                .filter(|(n, _, _, _)| n == name)
                .map(|(_, p, l, c)| (p.clone(), *l, *c))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// プロジェクト配下の全定義・参照を収集する（スレッド内で実行）。
fn collect_index(root: &Path, configs: &LangConfigs) -> Index {
    let mut defs = Vec::new();
    let mut refs = Vec::new();
    for entry in WalkBuilder::new(root).build().flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        let Ok(source) = std::fs::read(path) else {
            continue;
        };
        for (name, line, col, is_def) in configs.file_tags(path, &source) {
            if is_def {
                defs.push((name, path.to_path_buf(), line, col));
            } else {
                refs.push((name, path.to_path_buf(), line, col));
            }
        }
    }
    Index { defs, refs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_symbols_lists_definitions_in_order() {
        let configs = LangConfigs::new();
        let path = PathBuf::from("lib.rs");
        let src = b"struct A;\nfn foo() {}\nfn bar() {}\n";
        let syms = configs.file_symbols(&path, src);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"A"));
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"bar"));
        // 行順
        assert!(syms.windows(2).all(|w| w[0].line <= w[1].line));
    }

    #[test]
    fn project_index_resolves_definition() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut index = ProjectIndex::new(root.as_path());
        index.start();
        // バックグラウンド構築の完了を待つ。
        while index.is_building() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            index.poll();
        }
        let (path, _line, _col) = index
            .definition("ProjectIndex")
            .expect("ProjectIndex defined somewhere");
        assert_eq!(path.extension().unwrap(), "rs");
    }

    #[test]
    fn project_index_collects_references() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut index = ProjectIndex::new(root.as_path());
        index.start();
        while index.is_building() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            index.poll();
        }
        // jump_to_code_line は app.rs 内で複数回呼ばれている。
        let refs = index.references("jump_to_code_line");
        assert!(!refs.is_empty(), "expected call sites for jump_to_code_line");
        assert!(refs.iter().all(|(p, _, _)| p.extension().unwrap() == "rs"));
    }
}

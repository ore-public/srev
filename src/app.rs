use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use crate::diffview::{self, DiffRender};
use crate::finder::{Finder, collect_files};
use crate::fuzzy::Fuzzy;
use crate::git::{BranchEntry, CommitFile, CommitInfo, FileStatus, GitInfo};
use crate::grep::ProjectSearch;
use crate::highlight::CodeHighlighter;
use crate::keymap::{Action, Chord, Keymap};
use crate::lsp::{self, Loc, LspManager, Reply, SymInfo};
use crate::tags::{LangConfigs, ProjectIndex, Symbol};
use crate::tree::Tree;

/// どのペインにフォーカスがあるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Outline,
    Content,
}

/// メインペインの表示モード。差分とフルコードをキーで往復する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Diff,
    Code,
    /// コミットビュー：左上=コミット一覧、左下=変更ファイル、右=ファイル差分。
    Commits,
    /// PR 差分ビュー：左上=デフォルトブランチとの変更ファイル一覧、右=差分（merge-base→HEAD）。
    Branch,
}

/// visual mode の選択。アンカーを固定し、もう一端はカーソル。
struct Selection {
    anchor_line: usize,
    anchor_col: usize,
    /// `V`（行単位）なら true、`v`（文字単位）なら false。
    linewise: bool,
}

/// 正規化済みの選択範囲（ui へ渡す）。
pub struct SelRegion {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub linewise: bool,
}

/// ペイン内インラインフィルタ（`/` であいまい絞り込み）の状態。
struct FilterState {
    query: String,
    selected: usize,
    /// マッチ対象ラベル（開いた時点のスナップショット）。
    labels: Vec<String>,
    /// 各ラベルに対応するアクション先。
    targets: Vec<FilterTarget>,
    /// ラベルに付随するステータス（ツリー用、シンボルでは None）。
    statuses: Vec<Option<FileStatus>>,
}

enum FilterTarget {
    File(PathBuf),
    Line(usize),
}

/// git の変更ファイル 1 件。
struct ChangedEntry {
    rel: String,
    abs: PathBuf,
    status: FileStatus,
}

/// ジャンプ履歴の 1 地点（ファイル＋カーソル位置）。
#[derive(Clone, PartialEq, Eq)]
struct JumpPos {
    path: PathBuf,
    line: usize,
    col: usize,
}

/// ジャンプ履歴ペインに描く 1 行（ui へ渡す）。
pub struct JumpRow {
    /// `相対パス:行` 表記。
    pub label: String,
    /// 現在地（`▶`）か。
    pub current: bool,
}

/// 参照（呼び出し元）一覧の 1 件。
pub struct RefHit {
    pub rel: String,
    pub abs: PathBuf,
    /// 0 始まりの行。
    pub line: usize,
    /// 0 始まりの列（名前の位置）。
    pub col: usize,
    pub preview: String,
}

/// `gr` で開く参照一覧オーバーレイの状態。
pub struct RefList {
    /// 検索した名前（タイトル表示用）。
    pub name: String,
    pub hits: Vec<RefHit>,
    pub selected: usize,
}

impl RefList {
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.hits.len() {
            self.selected += 1;
        }
    }
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// ブランチ切替ピッカーのオーバーレイ状態（`Ctrl-V`）。あいまい絞り込み対応。
pub struct BranchList {
    /// 全ブランチ（fetch 時のスナップショット）。
    pub entries: Vec<BranchEntry>,
    /// 絞り込みクエリ。
    pub query: String,
    /// クエリでランキングした `entries` への添字列（表示順）。
    pub results: Vec<usize>,
    /// `results` 内の選択位置。
    pub selected: usize,
}

impl BranchList {
    pub fn new(entries: Vec<BranchEntry>) -> Self {
        let results = (0..entries.len()).collect();
        // 既定の選択は現在ブランチ。
        let selected = entries.iter().position(|e| e.is_head).unwrap_or(0);
        Self {
            entries,
            query: String::new(),
            results,
            selected,
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
        }
    }
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// 現在選択中のブランチ（絞り込み結果が空なら `None`）。
    pub fn current(&self) -> Option<&BranchEntry> {
        self.results.get(self.selected).and_then(|&i| self.entries.get(i))
    }
}

/// `[`/`]` で順送りする対象リスト。
#[derive(Clone, Copy)]
enum NavList {
    /// 変更ファイル一覧（差分モード）。
    Changed,
    /// 全ファイル（コードモード）。
    AllFiles,
    /// 選択中コミットの変更ファイル（コミットビュー）。
    CommitFiles,
    /// デフォルトブランチとの変更ファイル（PR 差分ビュー）。
    BranchFiles,
}

/// 左上ペインの描画指示（ui へ渡す）。
pub enum LeftPane {
    /// 階層ツリー（ui が `app.tree` を読む）。
    Tree,
    /// フラットリスト（変更ファイル / フィルタ結果）。
    List {
        title: String,
        query: Option<String>,
        rows: Vec<ListRow>,
    },
}

pub struct ListRow {
    pub label: String,
    pub status: Option<FileStatus>,
    pub selected: bool,
}

/// アウトライン（シンボル一覧）を何で抽出したか。Symbols ペインのタイトルに表示する。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutlineSource {
    TreeSitter,
    Lsp,
}

impl OutlineSource {
    pub fn label(self) -> &'static str {
        match self {
            OutlineSource::TreeSitter => "tree-sitter",
            OutlineSource::Lsp => "LSP",
        }
    }
}

/// アウトラインペインの描画指示。
pub enum OutlinePane {
    Empty(&'static str),
    List {
        query: Option<String>,
        rows: Vec<OutlineRow>,
        /// 抽出元のラベル（"LSP" / "tree-sitter"）。タイトルに併記する。
        source: Option<&'static str>,
    },
}

pub struct OutlineRow {
    pub kind: String,
    pub name: String,
    pub selected: bool,
}

/// いま開いているファイルの状態一式。
pub struct OpenFile {
    pub path: PathBuf,
    /// ハイライト済みの表示行。
    pub lines: Vec<Line<'static>>,
    /// 素のテキスト行（カーソル操作・検索・語抽出に使う）。
    pub raw_lines: Vec<String>,
    pub diff: Option<DiffRender>,
    /// コード表示のスクロール（ビューポート先頭行）。
    pub scroll: usize,
    pub diff_scroll: usize,
    /// 差分表示の横スクロール量（桁。長い行が幅に収まらないとき左へずらす）。
    pub diff_hscroll: usize,
    /// vim ライクカーソル（コード表示時、0 始まり）。
    pub cursor_line: usize,
    pub cursor_col: usize,
    /// このファイルのシンボル定義一覧（アウトライン）。
    pub outline: Vec<Symbol>,
    /// アウトラインの抽出元（tree-sitter 即時表示 → LSP で差し替わる）。
    pub outline_source: OutlineSource,
    pub outline_selected: usize,
    /// 各行の変更印（HEAD との差分、コードビューの gutter 用）。
    pub change_marks: Vec<crate::diffview::LineMark>,
}

impl OpenFile {
    pub fn has_diff(&self) -> bool {
        self.diff.as_ref().is_some_and(|d| !d.rows.is_empty())
    }

    fn line_len(&self, line: usize) -> usize {
        self.raw_lines.get(line).map_or(0, |l| l.chars().count())
    }

    fn last_line(&self) -> usize {
        self.raw_lines.len().saturating_sub(1)
    }

    /// 縦移動。希望列を保ったままクランプする。
    fn move_cursor_v(&mut self, delta: isize) {
        let new = (self.cursor_line as isize + delta).clamp(0, self.last_line() as isize) as usize;
        self.cursor_line = new;
        self.cursor_col = self.cursor_col.min(self.line_len(new).saturating_sub(1));
    }

    fn move_cursor_h(&mut self, delta: isize) {
        let max = self.line_len(self.cursor_line).saturating_sub(1);
        self.cursor_col = (self.cursor_col as isize + delta).clamp(0, max as isize) as usize;
    }

    /// カーソル下の識別子。
    fn word_under_cursor(&self) -> Option<String> {
        let line = self.raw_lines.get(self.cursor_line)?;
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            return None;
        }
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let col = self.cursor_col.min(chars.len() - 1);
        if !is_ident(chars[col]) {
            return None;
        }
        let mut start = col;
        while start > 0 && is_ident(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < chars.len() && is_ident(chars[end + 1]) {
            end += 1;
        }
        Some(chars[start..=end].iter().collect())
    }
}

/// 応答待ちの LSP ナビゲーション要求の種別。
enum NavKind {
    Def,
    Refs,
}

/// 送信済み LSP 要求のメタ情報（応答到着時/タイムアウト時に使う）。
struct PendingNav {
    kind: NavKind,
    /// カーソル下の語（空応答/タイムアウト時の tags フォールバックに使う）。
    word: String,
    /// 送信時刻（タイムアウト判定用）。
    sent: Instant,
}

pub struct App {
    pub root: PathBuf,
    pub focus: Focus,
    pub view_mode: ViewMode,
    pub tree: Tree,
    pub open: Option<OpenFile>,
    pub statuses: HashMap<PathBuf, FileStatus>,
    pub finder: Finder,
    /// プロジェクト全体の本文検索（`Ctrl-F`）。
    pub grep: ProjectSearch,
    /// ファイル内検索の確定済みクエリ（マッチ行ハイライト・n/N に使う）。
    pub search_query: String,
    /// 検索入力中のバッファ（`/` で開始）。`None` なら非入力。
    pub search_input: Option<String>,
    /// 差分を side-by-side（左右）で表示するか。false なら unified。
    pub diff_split: bool,
    /// 次の描画でカーソルを画面中央に再センターする（ジャンプ直後の一度きり）。
    pub recenter: bool,
    /// visual mode の選択（コード表示時のみ有効）。
    selection: Option<Selection>,
    /// ジャンプ履歴（戻る用＝飛ぶ前の位置スタック）。
    jump_back: Vec<JumpPos>,
    /// ジャンプ履歴（進む用）。
    jump_forward: Vec<JumpPos>,
    /// 右側のジャンプ履歴ペインを表示するか（既定 true、`J` で切替）。
    pub show_jumps: bool,
    /// 参照（呼び出し元）一覧オーバーレイ（`gr`）。None なら非表示。
    pub refs: Option<RefList>,
    /// ブランチ切替ピッカー（`B`）。None なら非表示。
    pub branches: Option<BranchList>,
    /// ステータス行に一時表示するメッセージ（yank 結果など）。
    pub flash: Option<String>,
    /// flash を表示し始めた時刻（一定時間で自動的に消す）。
    flash_at: Option<Instant>,
    /// 全ファイル一覧（コードモードのツリーフィルタ候補）。
    all_files: Vec<crate::finder::FileEntry>,
    /// git の変更ファイル一覧（diff モードのツリー）。
    changed: Vec<ChangedEntry>,
    changed_selected: usize,
    tree_filter: Option<FilterState>,
    outline_filter: Option<FilterState>,
    fuzzy: Fuzzy,
    lang_configs: LangConfigs,
    index: ProjectIndex,
    /// LSP クライアント（`gd`/`gr` を LSP 優先で解決。未対応言語は tags にフォールバック）。
    lsp: LspManager,
    /// 送信済みで応答待ちの LSP ナビゲーション要求（要求 ID → 内容）。
    pending_nav: HashMap<i64, PendingNav>,
    /// 送信済みで応答待ちの documentSymbol 要求（要求 ID → 対象ファイル）。
    pending_outline: HashMap<i64, PathBuf>,
    /// LSP アウトラインを取得したいファイル（サーバー準備でき次第 documentSymbol を送る）。
    outline_want: Option<PathBuf>,
    /// コミットビュー：デフォルトブランチとの差分コミット一覧（または全履歴）。
    commits: Vec<CommitInfo>,
    commit_selected: usize,
    /// 選択中コミットの変更ファイル一覧（左下ペイン）。
    commit_files: Vec<CommitFile>,
    commit_file_selected: usize,
    /// コミット一覧を構築済みか（初回 `c` で遅延構築）。
    commits_loaded: bool,
    /// 現在 HEAD がデフォルトブランチか（タイトル表示用）。
    on_default_branch: bool,
    /// 基準デフォルトブランチ名（タイトル表示用）。
    default_branch_label: String,
    /// PR 差分ビュー：デフォルトブランチとの変更ファイル一覧（merge-base→HEAD）。
    branch_files: Vec<CommitFile>,
    branch_file_selected: usize,
    /// PR 差分一覧を構築済みか（初回 `C` で遅延構築）。
    branch_diff_loaded: bool,
    /// PR 差分の基準デフォルトブランチ名（タイトル表示用）。
    branch_default_label: String,
    /// HEAD がデフォルトと同一（分岐していない）で固有差分が無いか。空表示時の
    /// メッセージを「デフォルト上」と「差分ゼロ」で出し分けるのに使う。
    branch_on_default: bool,
    /// 現在チェックアウト中のブランチ名（ステータスバー表示用にキャッシュ）。
    branch: Option<String>,
    git: Option<GitInfo>,
    highlighter: CodeHighlighter,
    keymap: Keymap,
    pending_g: bool,
    should_quit: bool,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let git = GitInfo::discover(&root);
        // git の無視判定（非 git なら何も無視しない）。
        let (tree, mut all_files) = {
            let is_ignored = |p: &Path| git.as_ref().is_some_and(|g| g.is_ignored(p));
            (
                Tree::new(&root, &is_ignored),
                collect_files(&root, &is_ignored),
            )
        };
        all_files.sort_by(|a, b| a.rel.cmp(&b.rel)); // [ ] の順送りを安定させる
        let finder = Finder::from_files(all_files.clone());
        let grep = ProjectSearch::new(&all_files);
        let mut index = ProjectIndex::new(&root);
        index.start(); // 起動直後にバックグラウンド構築を開始
        let lsp = LspManager::new(&root);
        let branch = git.as_ref().and_then(|g| g.current_branch());
        let statuses = git.as_ref().map(|g| g.statuses()).unwrap_or_default();
        let changed = changed_entries(&statuses, &root);
        Self {
            root,
            focus: Focus::Tree,
            // 起動時は通常（コード）モード。差分は d で切り替える。
            view_mode: ViewMode::Code,
            tree,
            open: None,
            statuses,
            finder,
            grep,
            search_query: String::new(),
            search_input: None,
            diff_split: true,
            recenter: false,
            selection: None,
            jump_back: Vec::new(),
            jump_forward: Vec::new(),
            show_jumps: true,
            refs: None,
            branches: None,
            flash: None,
            flash_at: None,
            all_files,
            changed,
            changed_selected: 0,
            tree_filter: None,
            outline_filter: None,
            fuzzy: Fuzzy::new(),
            lang_configs: LangConfigs::new(),
            index,
            lsp,
            pending_nav: HashMap::new(),
            pending_outline: HashMap::new(),
            outline_want: None,
            commits: Vec::new(),
            commit_selected: 0,
            commit_files: Vec::new(),
            commit_file_selected: 0,
            commits_loaded: false,
            on_default_branch: false,
            default_branch_label: String::new(),
            branch_files: Vec::new(),
            branch_file_selected: 0,
            branch_diff_loaded: false,
            branch_default_label: String::new(),
            branch_on_default: false,
            branch,
            git,
            highlighter: CodeHighlighter::new(),
            keymap: Keymap::load(),
            pending_g: false,
            should_quit: false,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            self.handle_events()?;
            // バックグラウンド索引の完了を取り込む。完了時は一度だけ知らせる。
            if self.index.poll() {
                self.flash = Some("index ready".into());
            }
            // LSP の応答を取り込み、ジャンプ／参照一覧を確定する。
            self.poll_lsp();
            self.expire_flash();
        }
        Ok(())
    }

    /// flash を一定時間で自動的に消す（操作しなくても見えなくならないように）。
    fn expire_flash(&mut self) {
        const FLASH_TTL: Duration = Duration::from_millis(2500);
        match (self.flash.is_some(), self.flash_at) {
            (true, None) => self.flash_at = Some(Instant::now()), // 表示開始
            (true, Some(t)) if t.elapsed() >= FLASH_TTL => {
                self.flash = None;
                self.flash_at = None;
            }
            (false, _) => self.flash_at = None,
            _ => {}
        }
    }

    fn handle_events(&mut self) -> Result<()> {
        // キー入力を待ちつつ、定期的に起きてバックグラウンド作業を反映する。
        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        // 一時メッセージは次のキー入力で消す（yank 直後だけ表示）。
        self.flash = None;
        self.flash_at = None;

        // モーダル（検索入力・あいまい検索・ペインフィルタ）は専用ハンドラへ。
        if self.search_input.is_some() {
            self.on_key_search_input(key);
            return;
        }
        if self.finder.active {
            self.on_key_finder(key);
            return;
        }
        if self.grep.active {
            self.on_key_grep(key);
            return;
        }
        if self.refs.is_some() {
            self.on_key_refs(key);
            return;
        }
        if self.branches.is_some() {
            self.on_key_branches(key);
            return;
        }
        if self.tree_filter.is_some() {
            self.on_key_tree_filter(key);
            return;
        }
        if self.outline_filter.is_some() {
            self.on_key_outline_filter(key);
            return;
        }

        // g プレフィックス（本文フォーカス時。設定では変更不可）。
        // gg=先頭は差分/コード両方で有効。gd=定義はコード表示でのみ意味を持つ。
        if self.pending_g {
            self.pending_g = false;
            if self.focus == Focus::Content {
                match key.code {
                    KeyCode::Char('g') => self.dispatch(Action::Top),
                    KeyCode::Char('d') => self.dispatch(Action::GotoDef),
                    KeyCode::Char('r') => self.dispatch(Action::GotoReferences),
                    _ => {}
                }
            }
            return;
        }
        if self.focus == Focus::Content
            && key.code == KeyCode::Char('g')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.pending_g = true;
            return;
        }

        // 設定可能なキーマップ経由でアクションへ。
        if let Some(action) = self.keymap.get(Chord::from_event(key)) {
            self.dispatch(action);
        }
    }

    /// アクションを現在のフォーカス・モードに応じて実行する。
    fn dispatch(&mut self, action: Action) {
        use Action::*;
        match action {
            Quit => self.should_quit = true,
            FocusNext => {
                self.selection = None;
                self.focus = match self.focus {
                    Focus::Tree => Focus::Outline,
                    Focus::Outline => Focus::Content,
                    Focus::Content => Focus::Tree,
                };
            }
            ToggleDiff => self.toggle_view_mode(),
            FuzzyFind => self.finder.open(),
            Grep => self.grep.open(),
            JumpBack => self.go_back(),
            JumpForward => self.go_forward(),
            ToggleJumps => self.show_jumps = !self.show_jumps,
            ToggleCommits => self.toggle_commit_view(),
            ToggleBranchDiff => self.toggle_branch_diff(),
            NextChange => self.change_jump(true),
            PrevChange => self.change_jump(false),
            SwitchBranch => self.open_branch_list(),
            Reload => self.reload(),
            // 差分モードでは変更ファイル一覧、コードモードでは全ファイルを順送り。
            NextFile => self.navigate_files(self.nav_list(), true),
            PrevFile => self.navigate_files(self.nav_list(), false),
            ToggleSplit => {
                self.diff_split = !self.diff_split;
                self.ensure_diff_split();
                // 表示行数が変わるのでスクロールをクランプ。
                let split = self.effective_split();
                if let Some(open) = self.open.as_mut()
                    && let Some(diff) = open.diff.as_ref()
                {
                    let len = diff.row_count(split);
                    open.diff_scroll = open.diff_scroll.min(len.saturating_sub(1));
                }
            }
            _ => match self.focus {
                Focus::Tree => self.tree_action(action),
                Focus::Outline => self.outline_action(action),
                Focus::Content => self.content_action(action),
            },
        }
    }

    fn tree_action(&mut self, action: Action) {
        use Action::*;
        // コミットビューでは左上=コミット一覧を操作する。
        if self.view_mode == ViewMode::Commits {
            match action {
                Down if self.commit_selected + 1 < self.commits.len() => {
                    self.select_commit(self.commit_selected + 1)
                }
                Up if self.commit_selected > 0 => self.select_commit(self.commit_selected - 1),
                // Enter/→ で左下のファイル一覧へフォーカス移動。
                Activate | Right if !self.commit_files.is_empty() => self.focus = Focus::Outline,
                _ => {}
            }
            return;
        }
        // PR 差分ビューでは左上=デフォルトブランチとの変更ファイル一覧を操作する。
        if self.view_mode == ViewMode::Branch {
            match action {
                Down if self.branch_file_selected + 1 < self.branch_files.len() => {
                    self.open_branch_file(self.branch_file_selected + 1)
                }
                Up if self.branch_file_selected > 0 => {
                    self.open_branch_file(self.branch_file_selected - 1)
                }
                Activate | Right if !self.branch_files.is_empty() => self.focus = Focus::Content,
                _ => {}
            }
            return;
        }
        // diff モードでは「変更ファイル一覧」を操作する。
        if self.view_mode == ViewMode::Diff {
            match action {
                Down => {
                    if self.changed_selected + 1 < self.changed.len() {
                        self.changed_selected += 1;
                    }
                }
                Up => self.changed_selected = self.changed_selected.saturating_sub(1),
                Activate | Right => {
                    if let Some(entry) = self.changed.get(self.changed_selected) {
                        let path = entry.abs.clone();
                        self.open_file(&path);
                        self.focus = Focus::Content;
                    }
                }
                Find => self.open_tree_filter(),
                _ => {}
            }
            return;
        }
        match action {
            Down => self.tree.move_down(),
            Up => self.tree.move_up(),
            Left => self.tree.collapse(),
            Right | Activate => {
                if let Some(path) = self.tree.activate() {
                    self.open_file(&path);
                    self.focus = Focus::Content;
                }
            }
            Find => self.open_tree_filter(),
            _ => {}
        }
    }

    fn outline_action(&mut self, action: Action) {
        use Action::*;
        // コミットビューでは左下=変更ファイル一覧を操作する。
        if self.view_mode == ViewMode::Commits {
            match action {
                Down if self.commit_file_selected + 1 < self.commit_files.len() => {
                    self.open_commit_file(self.commit_file_selected + 1)
                }
                Up if self.commit_file_selected > 0 => {
                    self.open_commit_file(self.commit_file_selected - 1)
                }
                Activate | Right => self.focus = Focus::Content,
                Left => self.focus = Focus::Tree,
                _ => {}
            }
            return;
        }
        match action {
            Down => {
                if let Some(open) = self.open.as_mut()
                    && open.outline_selected + 1 < open.outline.len()
                {
                    open.outline_selected += 1;
                }
            }
            Up => {
                if let Some(open) = self.open.as_mut() {
                    open.outline_selected = open.outline_selected.saturating_sub(1);
                }
            }
            Activate | Right => {
                let pos = self
                    .open
                    .as_ref()
                    .and_then(|o| o.outline.get(o.outline_selected))
                    .map(|s| (s.line, s.col));
                if let Some((line, col)) = pos {
                    self.record_jump_origin();
                    self.jump_to_code_pos(line, col);
                }
            }
            Find => self.open_outline_filter(),
            _ => {}
        }
    }

    fn content_action(&mut self, action: Action) {
        use Action::*;
        // 差分表示中はスクロール操作（カーソルはコード表示の概念）。
        if self.diff_content() {
            // n/N は次/前の hunk へ（self を可変借用するので先に処理）。
            match action {
                SearchNext => {
                    self.hunk(true);
                    return;
                }
                SearchPrev => {
                    self.hunk(false);
                    return;
                }
                _ => {}
            }
            self.ensure_diff_split();
            let split = self.effective_split();
            let Some(open) = self.open.as_mut() else {
                return;
            };
            let last = open
                .diff
                .as_ref()
                .map_or(0, |d| d.row_count(split))
                .saturating_sub(1);
            const H_SCROLL_STEP: usize = 8;
            // 横スクロールのクランプ上限：最長行から最低表示桁ぶんを残した位置まで。
            // 可視幅はここでは不明なので、末尾付近でも一定桁は残る保守値で抑える
            // （行末送り `$` で本文がすべて画面外へ消えるのを防ぐ）。
            const MIN_VISIBLE_COLS: usize = 24;
            let max_h = open
                .raw_lines
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
                .saturating_sub(MIN_VISIBLE_COLS);
            match action {
                Down => open.diff_scroll = (open.diff_scroll + 1).min(last),
                Up => open.diff_scroll = open.diff_scroll.saturating_sub(1),
                Top => open.diff_scroll = 0,
                Bottom => open.diff_scroll = last,
                HalfPageDown => open.diff_scroll = (open.diff_scroll + 15).min(last),
                HalfPageUp => open.diff_scroll = open.diff_scroll.saturating_sub(15),
                // 長い行が幅に収まらないときの横スクロール（h/l, ←/→, 0/$）。
                Right => open.diff_hscroll = (open.diff_hscroll + H_SCROLL_STEP).min(max_h),
                Left => open.diff_hscroll = open.diff_hscroll.saturating_sub(H_SCROLL_STEP),
                LineStart => open.diff_hscroll = 0,
                LineEnd => open.diff_hscroll = max_h,
                _ => {}
            }
            return;
        }
        match action {
            Down => self.move_v(1),
            Up => self.move_v(-1),
            Left => {
                if let Some(o) = self.open.as_mut() {
                    o.move_cursor_h(-1)
                }
            }
            Right => {
                if let Some(o) = self.open.as_mut() {
                    o.move_cursor_h(1)
                }
            }
            WordForward => self.move_word(true),
            WordBack => self.move_word(false),
            LineStart => {
                if let Some(o) = self.open.as_mut() {
                    o.cursor_col = 0
                }
            }
            LineEnd => {
                if let Some(o) = self.open.as_mut() {
                    o.cursor_col = o.line_len(o.cursor_line).saturating_sub(1)
                }
            }
            Top => {
                self.record_jump_origin();
                self.cursor_to(0);
                self.recenter = true;
            }
            Bottom => {
                if let Some(last) = self.open.as_ref().map(|o| o.last_line()) {
                    self.record_jump_origin();
                    self.cursor_to(last);
                    self.recenter = true;
                }
            }
            HalfPageDown => self.move_v(15),
            HalfPageUp => self.move_v(-15),
            GotoDef => self.goto_definition(),
            GotoReferences => self.goto_references(),
            Find => self.search_input = Some(String::new()),
            // 検索中（クエリあり）なら検索の次/前へ。未検索なら変更ブロックを辿る
            // （差分系モードの n/N、およびコードビューの } { と挙動を揃える）。
            SearchNext => {
                if self.search_query.is_empty() {
                    self.goto_change(true);
                } else {
                    self.search_jump(true);
                }
            }
            SearchPrev => {
                if self.search_query.is_empty() {
                    self.goto_change(false);
                } else {
                    self.search_jump(false);
                }
            }
            VisualChar => self.toggle_visual(false),
            VisualLine => self.toggle_visual(true),
            Yank => self.yank_selection(),
            YankLocation => self.yank_location(),
            CancelSelection => self.selection = None,
            _ => {}
        }
    }

    fn on_key_finder(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.finder.close(),
            KeyCode::Enter => {
                if let Some(path) = self.finder.selected_path() {
                    self.finder.close();
                    self.open_file(&path);
                    self.focus = Focus::Content;
                }
            }
            KeyCode::Backspace => self.finder.pop_char(),
            KeyCode::Down => self.finder.move_down(),
            KeyCode::Up => self.finder.move_up(),
            KeyCode::Char('n') if ctrl => self.finder.move_down(),
            KeyCode::Char('p') if ctrl => self.finder.move_up(),
            KeyCode::Char(c) if !ctrl => self.finder.push_char(c),
            _ => {}
        }
    }

    fn on_key_grep(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.grep.close(),
            KeyCode::Enter => {
                if let Some((path, line)) = self.grep.selected_target() {
                    let query = self.grep.query.clone();
                    self.grep.close();
                    self.record_jump_origin(); // 飛ぶ前（今のファイル）を履歴へ
                    self.open_file(&path);
                    // 検索語を / にも引き継ぎ、開いた先で n/N で追えるようにする。
                    self.search_query = query;
                    self.jump_to_code_line(line); // 該当行を中央表示
                }
            }
            KeyCode::Backspace => self.grep.pop_char(),
            KeyCode::Down => self.grep.move_down(),
            KeyCode::Up => self.grep.move_up(),
            KeyCode::Char('n') if ctrl => self.grep.move_down(),
            KeyCode::Char('p') if ctrl => self.grep.move_up(),
            KeyCode::Char(c) if !ctrl => self.grep.push_char(c),
            _ => {}
        }
    }

    fn on_key_search_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search_input = None,
            KeyCode::Enter => {
                if let Some(buf) = self.search_input.take() {
                    self.search_query = buf;
                    self.search_jump_inclusive();
                }
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.search_input.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(buf) = self.search_input.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
    }

    // --- ペイン内インラインフィルタ ---

    fn open_tree_filter(&mut self) {
        let (labels, targets, statuses) = if self.view_mode == ViewMode::Diff {
            (
                self.changed.iter().map(|c| c.rel.clone()).collect(),
                self.changed
                    .iter()
                    .map(|c| FilterTarget::File(c.abs.clone()))
                    .collect(),
                self.changed.iter().map(|c| Some(c.status)).collect(),
            )
        } else {
            (
                self.all_files.iter().map(|e| e.rel.clone()).collect(),
                self.all_files
                    .iter()
                    .map(|e| FilterTarget::File(e.abs.clone()))
                    .collect(),
                self.all_files
                    .iter()
                    .map(|e| self.statuses.get(&e.abs).copied())
                    .collect(),
            )
        };
        self.tree_filter = Some(FilterState {
            query: String::new(),
            selected: 0,
            labels,
            targets,
            statuses,
        });
    }

    fn open_outline_filter(&mut self) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        if open.outline.is_empty() {
            return;
        }
        self.outline_filter = Some(FilterState {
            query: String::new(),
            selected: 0,
            labels: open.outline.iter().map(|s| s.name.clone()).collect(),
            targets: open
                .outline
                .iter()
                .map(|s| FilterTarget::Line(s.line))
                .collect(),
            statuses: open.outline.iter().map(|_| None).collect(),
        });
    }

    fn on_key_tree_filter(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.tree_filter = None,
            KeyCode::Enter => {
                let target = self.filter_selected(true);
                self.tree_filter = None;
                if let Some(FilterTarget::File(path)) = target {
                    self.open_file(&path);
                    self.focus = Focus::Content;
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = self.tree_filter.as_mut() {
                    f.query.pop();
                    f.selected = 0;
                }
            }
            KeyCode::Up => self.move_filter_sel(true, false),
            KeyCode::Down => self.move_filter_sel(true, true),
            KeyCode::Char('n') if ctrl => self.move_filter_sel(true, true),
            KeyCode::Char('p') if ctrl => self.move_filter_sel(true, false),
            KeyCode::Char(c) if !ctrl => {
                if let Some(f) = self.tree_filter.as_mut() {
                    f.query.push(c);
                    f.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn on_key_outline_filter(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.outline_filter = None,
            KeyCode::Enter => {
                let target = self.filter_selected(false);
                self.outline_filter = None;
                if let Some(FilterTarget::Line(line)) = target {
                    self.record_jump_origin();
                    self.jump_to_code_line(line);
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = self.outline_filter.as_mut() {
                    f.query.pop();
                    f.selected = 0;
                }
            }
            KeyCode::Up => self.move_filter_sel(false, false),
            KeyCode::Down => self.move_filter_sel(false, true),
            KeyCode::Char('n') if ctrl => self.move_filter_sel(false, true),
            KeyCode::Char('p') if ctrl => self.move_filter_sel(false, false),
            KeyCode::Char(c) if !ctrl => {
                if let Some(f) = self.outline_filter.as_mut() {
                    f.query.push(c);
                    f.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn move_filter_sel(&mut self, tree: bool, down: bool) {
        let App {
            tree_filter,
            outline_filter,
            fuzzy,
            ..
        } = self;
        let slot = if tree { tree_filter } else { outline_filter };
        if let Some(f) = slot.as_mut() {
            let n = filtered_indices(fuzzy, f).len();
            if down {
                if f.selected + 1 < n {
                    f.selected += 1;
                }
            } else {
                f.selected = f.selected.saturating_sub(1);
            }
        }
    }

    /// 現在のフィルタ選択行のアクション先を返す（クローン）。
    fn filter_selected(&mut self, tree: bool) -> Option<FilterTarget> {
        let App {
            tree_filter,
            outline_filter,
            fuzzy,
            ..
        } = self;
        let f = if tree {
            tree_filter.as_ref()?
        } else {
            outline_filter.as_ref()?
        };
        let idx = filtered_indices(fuzzy, f);
        let sel = idx.get(f.selected).copied()?;
        Some(match &f.targets[sel] {
            FilterTarget::File(p) => FilterTarget::File(p.clone()),
            FilterTarget::Line(l) => FilterTarget::Line(*l),
        })
    }

    // --- ui へ渡す表示データ ---

    /// 左上ペインの描画指示。
    pub fn left_pane(&mut self, limit: usize) -> LeftPane {
        if self.tree_filter.is_some() {
            let App {
                tree_filter, fuzzy, ..
            } = self;
            let f = tree_filter.as_ref().unwrap();
            let idx = filtered_indices(fuzzy, f);
            let offset = list_offset(f.selected, limit);
            let rows = idx
                .iter()
                .skip(offset)
                .take(limit)
                .enumerate()
                .map(|(i, &j)| ListRow {
                    label: f.labels[j].clone(),
                    status: f.statuses[j],
                    selected: offset + i == f.selected,
                })
                .collect();
            return LeftPane::List {
                title: "Filter".into(),
                query: Some(f.query.clone()),
                rows,
            };
        }

        if self.view_mode == ViewMode::Diff {
            let offset = list_offset(self.changed_selected, limit);
            let rows = self
                .changed
                .iter()
                .enumerate()
                .skip(offset)
                .take(limit)
                .map(|(i, c)| ListRow {
                    label: c.rel.clone(),
                    status: Some(c.status),
                    selected: i == self.changed_selected,
                })
                .collect();
            return LeftPane::List {
                title: format!("Changed ({})", self.changed.len()),
                query: None,
                rows,
            };
        }

        if self.view_mode == ViewMode::Branch {
            let offset = list_offset(self.branch_file_selected, limit);
            let rows = self
                .branch_files
                .iter()
                .enumerate()
                .skip(offset)
                .take(limit)
                .map(|(i, f)| ListRow {
                    label: f.rel.clone(),
                    status: Some(f.status),
                    selected: i == self.branch_file_selected,
                })
                .collect();
            return LeftPane::List {
                title: format!("PR vs {} ({})", self.branch_default_label, self.branch_files.len()),
                query: None,
                rows,
            };
        }

        LeftPane::Tree
    }

    /// アウトラインペインの描画指示。
    pub fn outline_pane(&mut self, limit: usize) -> OutlinePane {
        // 抽出元ラベル（フィルタ表示でも開いているファイルのものを引き継ぐ）。
        let source = self.open.as_ref().map(|o| o.outline_source.label());
        if self.outline_filter.is_some() {
            let App {
                outline_filter,
                fuzzy,
                ..
            } = self;
            let f = outline_filter.as_ref().unwrap();
            let idx = filtered_indices(fuzzy, f);
            let offset = list_offset(f.selected, limit);
            let rows = idx
                .iter()
                .skip(offset)
                .take(limit)
                .enumerate()
                .map(|(i, &j)| OutlineRow {
                    kind: String::new(),
                    name: f.labels[j].clone(),
                    selected: offset + i == f.selected,
                })
                .collect();
            return OutlinePane::List {
                query: Some(f.query.clone()),
                rows,
                source,
            };
        }

        let Some(open) = &self.open else {
            return OutlinePane::Empty("(no file open)");
        };
        if open.outline.is_empty() {
            return OutlinePane::Empty("(no symbols / unsupported language)");
        }
        let offset = list_offset(open.outline_selected, limit);
        let rows = open
            .outline
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, s)| OutlineRow {
                kind: s.kind.clone(),
                name: s.name.clone(),
                selected: i == open.outline_selected,
            })
            .collect();
        OutlinePane::List {
            query: None,
            rows,
            source,
        }
    }

    // --- カーソル / ジャンプ補助 ---

    fn move_v(&mut self, delta: isize) {
        if let Some(o) = self.open.as_mut() {
            o.move_cursor_v(delta);
        }
    }

    fn move_word(&mut self, forward: bool) {
        let Some(o) = self.open.as_mut() else { return };
        let Some(line) = o.raw_lines.get(o.cursor_line) else {
            return;
        };
        let chars: Vec<char> = line.chars().collect();
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut col = o.cursor_col;
        if forward {
            // 現在語の終わりまで進み、次の語頭へ。
            while col < chars.len() && is_ident(chars[col]) {
                col += 1;
            }
            while col < chars.len() && !is_ident(chars[col]) {
                col += 1;
            }
            o.cursor_col = col.min(chars.len().saturating_sub(1));
        } else {
            col = col.saturating_sub(1);
            while col > 0 && !is_ident(chars[col]) {
                col -= 1;
            }
            while col > 0 && is_ident(chars[col - 1]) {
                col -= 1;
            }
            o.cursor_col = col;
        }
    }

    fn cursor_to(&mut self, line: usize) {
        if let Some(o) = self.open.as_mut() {
            o.cursor_line = line.min(o.last_line());
            o.cursor_col = o.cursor_col.min(o.line_len(o.cursor_line).saturating_sub(1));
        }
    }

    /// 定義/シンボルへジャンプする。対象行を次の描画で画面中央に置く。
    fn jump_to_line(&mut self, line: usize) {
        self.cursor_to(line);
        self.recenter = true;
    }

    /// コード表示に切り替えて指定行へジャンプ（中央表示）。ジャンプ系で共用。
    fn jump_to_code_line(&mut self, line: usize) {
        // 履歴/ブランチ内容（HEAD blob 等）を表示中にコードへ飛ぶ場合は、ディスクの
        // 実ファイルを開き直す（Branch アウトライン経由など。stale な内容を Code 表示しない）。
        if matches!(self.view_mode, ViewMode::Commits | ViewMode::Branch)
            && let Some(rel) = self.open.as_ref().map(|o| o.path.clone())
        {
            let abs = self.root.join(&rel);
            self.open_file(&abs);
        }
        self.view_mode = ViewMode::Code;
        self.focus = Focus::Content;
        self.jump_to_line(line);
    }

    /// 行＋列へジャンプ（カーソルをシンボル名の上に置く）。gd/gr/アウトライン用。
    fn jump_to_code_pos(&mut self, line: usize, col: usize) {
        self.jump_to_code_line(line);
        if let Some(o) = self.open.as_mut() {
            o.cursor_col = col.min(o.line_len(o.cursor_line).saturating_sub(1));
        }
    }

    /// ジャンプ履歴がまだ無いか（戻る/進む両方空）。
    pub fn jumps_empty(&self) -> bool {
        self.jump_back.is_empty() && self.jump_forward.is_empty()
    }

    /// ジャンプ履歴の経路（古い戻る → 現在地 → 進む）を ui 用に返す。
    pub fn jump_trail(&self) -> Vec<JumpRow> {
        let fmt = |jp: &JumpPos| {
            let rel = jp.path.strip_prefix(&self.root).unwrap_or(&jp.path);
            JumpRow {
                label: format!("{}:{}", rel.to_string_lossy(), jp.line + 1),
                current: false,
            }
        };
        let mut rows = Vec::new();
        for jp in &self.jump_back {
            rows.push(fmt(jp));
        }
        if let Some(cur) = self.current_pos() {
            let mut row = fmt(&cur);
            row.current = true;
            rows.push(row);
        }
        // 進む側は「次に進む先」を現在地のすぐ下に来るよう逆順で。
        for jp in self.jump_forward.iter().rev() {
            rows.push(fmt(jp));
        }
        rows
    }

    /// 現在のカーソル位置（開いているファイル）を返す。
    fn current_pos(&self) -> Option<JumpPos> {
        self.open.as_ref().map(|o| JumpPos {
            path: o.path.clone(),
            line: o.cursor_line,
            col: o.cursor_col,
        })
    }

    /// ジャンプ直前の位置を履歴に積む（進む履歴はクリア）。
    /// 各ジャンプ操作が「飛ぶ前」に呼ぶ。
    fn record_jump_origin(&mut self) {
        if let Some(pos) = self.current_pos() {
            if self.jump_back.last() != Some(&pos) {
                self.jump_back.push(pos);
            }
            self.jump_forward.clear();
        }
    }

    /// ジャンプ履歴を戻る（飛ぶ前の位置へ）。
    fn go_back(&mut self) {
        let Some(target) = self.jump_back.pop() else {
            self.flash = Some("no earlier jump".into());
            return;
        };
        if let Some(cur) = self.current_pos() {
            self.jump_forward.push(cur);
        }
        self.go_to_pos(target);
    }

    /// ジャンプ履歴を進む。
    fn go_forward(&mut self) {
        let Some(target) = self.jump_forward.pop() else {
            self.flash = Some("no later jump".into());
            return;
        };
        if let Some(cur) = self.current_pos() {
            self.jump_back.push(cur);
        }
        self.go_to_pos(target);
    }

    /// 履歴の地点へ移動する（別ファイルなら開いてから）。
    fn go_to_pos(&mut self, pos: JumpPos) {
        let same = self.open.as_ref().is_some_and(|o| o.path == pos.path);
        if !same {
            self.open_file(&pos.path);
        }
        self.view_mode = ViewMode::Code;
        self.focus = Focus::Content;
        self.cursor_to(pos.line);
        if let Some(o) = self.open.as_mut() {
            o.cursor_col = pos.col.min(o.line_len(o.cursor_line).saturating_sub(1));
        }
        self.recenter = true;
    }

    /// カーソル下の語の定義へジャンプする。LSP 優先、未対応/未準備なら tags にフォールバック。
    fn goto_definition(&mut self) {
        let Some((word, line, char_col, path, line_str, text)) = self.nav_request_cursor() else {
            return;
        };
        if let Some(lang) = self.lsp.lang_id_for(&path) {
            if self.lsp.ensure_open(&path, &lang, &text) {
                self.flash = Some(format!("LSP for {lang} unavailable (using tags)"));
            }
            if self.lsp.is_ready_for(&path) {
                let col = lsp::char_to_utf16(&line_str, char_col);
                if let Some(id) = self.lsp.request_definition(&path, line as u32, col) {
                    self.pending_nav.insert(
                        id,
                        PendingNav {
                            kind: NavKind::Def,
                            word,
                            sent: Instant::now(),
                        },
                    );
                    return;
                }
            }
        }
        self.definition_via_tags(&word);
    }

    /// `gd`/`gr` 用にカーソル下の (語, 行, char列, パス, 行文字列, ファイル全文) を集める。
    /// `word_under_cursor` が None なら何もしない。
    fn nav_request_cursor(&self) -> Option<(String, usize, usize, PathBuf, String, String)> {
        self.open.as_ref().and_then(|o| {
            o.word_under_cursor().map(|w| {
                (
                    w,
                    o.cursor_line,
                    o.cursor_col,
                    o.path.clone(),
                    o.raw_lines.get(o.cursor_line).cloned().unwrap_or_default(),
                    o.raw_lines.join("\n"),
                )
            })
        })
    }

    /// tree-sitter-tags による定義ジャンプ（LSP 非対応/未準備/空応答時のフォールバック）。
    fn definition_via_tags(&mut self, word: &str) {
        self.index.start(); // 念のため（未開始なら開始）
        match self.index.definition(word) {
            Some((path, line, col)) => {
                self.record_jump_origin(); // 飛ぶ前の位置を履歴へ
                let same = self.open.as_ref().is_some_and(|o| o.path == path);
                if !same {
                    self.open_file(&path);
                }
                self.jump_to_code_pos(line, col); // カーソルを名前の上へ
            }
            None => {
                self.flash = Some(if self.index.is_building() {
                    "indexing… (retry gd when ready)".into()
                } else {
                    format!("no definition: {word}")
                });
            }
        }
    }

    /// LSP 応答の取り込みとタイムアウト処理（毎フレーム呼ぶ）。
    fn poll_lsp(&mut self) {
        for (id, reply) in self.lsp.poll() {
            match reply {
                Reply::Locations(locs) => {
                    if let Some(p) = self.pending_nav.remove(&id) {
                        match p.kind {
                            NavKind::Def => self.apply_definition(p.word, locs),
                            NavKind::Refs => self.apply_references(p.word, locs),
                        }
                    }
                }
                Reply::Symbols(syms) => {
                    if let Some(path) = self.pending_outline.remove(&id) {
                        self.apply_outline(path, syms);
                    }
                }
            }
        }
        // サーバーが準備できたら、開いているファイルのアウトラインを LSP から取り直す。
        self.request_outline_if_ready();
        // 一定時間応答が無い要求は tags フォールバックへ（rust-analyzer 索引中対策）。
        let timed_out: Vec<i64> = self
            .pending_nav
            .iter()
            .filter(|(_, p)| p.sent.elapsed() > lsp::REQUEST_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in timed_out {
            if let Some(p) = self.pending_nav.remove(&id) {
                match p.kind {
                    NavKind::Def => self.definition_via_tags(&p.word),
                    NavKind::Refs => self.references_via_tags(&p.word),
                }
            }
        }
    }

    /// `outline_want` のファイルについて、サーバー準備済みなら documentSymbol を送る。
    /// 開いているファイルが変わっていたら要求を取り下げる。
    fn request_outline_if_ready(&mut self) {
        let Some(path) = self.outline_want.clone() else {
            return;
        };
        let still_open = self.open.as_ref().is_some_and(|o| o.path == path);
        if !still_open {
            self.outline_want = None;
            return;
        }
        if self.lsp.is_ready_for(&path)
            && let Some(id) = self.lsp.request_document_symbol(&path)
        {
            self.pending_outline.insert(id, path);
            self.outline_want = None;
        }
    }

    /// documentSymbol の結果をアウトラインへ反映する（対象ファイルがまだ開いている場合）。
    /// 空応答時は既存（tree-sitter）アウトラインを保持する。
    fn apply_outline(&mut self, path: PathBuf, syms: Vec<SymInfo>) {
        if syms.is_empty() {
            return;
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.path != path {
            return;
        }
        open.outline = syms
            .into_iter()
            .map(|s| {
                let col = open
                    .raw_lines
                    .get(s.line)
                    .map(|l| lsp::utf16_to_char(l, s.character))
                    .unwrap_or(0);
                Symbol {
                    name: s.name,
                    kind: s.kind,
                    line: s.line,
                    col,
                }
            })
            .collect();
        open.outline_source = OutlineSource::Lsp;
        if open.outline_selected >= open.outline.len() {
            open.outline_selected = 0;
        }
    }

    /// LSP の定義応答を適用してジャンプする（空なら tags フォールバック）。
    fn apply_definition(&mut self, word: String, locs: Vec<Loc>) {
        let Some(loc) = locs.into_iter().next() else {
            self.definition_via_tags(&word);
            return;
        };
        self.record_jump_origin(); // 飛ぶ前の位置を履歴へ
        let same = self.open.as_ref().is_some_and(|o| o.path == loc.path);
        if !same {
            self.open_file(&loc.path);
        }
        // UTF-16 列 → char 列（対象ファイルの該当行から）。
        let col = self
            .open
            .as_ref()
            .and_then(|o| o.raw_lines.get(loc.line))
            .map(|l| lsp::utf16_to_char(l, loc.character))
            .unwrap_or(0);
        self.jump_to_code_pos(loc.line, col);
    }

    /// LSP の参照応答を参照一覧オーバーレイへ（空なら tags フォールバック）。
    fn apply_references(&mut self, word: String, locs: Vec<Loc>) {
        if locs.is_empty() {
            self.references_via_tags(&word);
            return;
        }
        // 同じファイルは 1 回だけ読んでプレビュー行と列変換に使う。
        let mut cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut hits = Vec::new();
        for loc in locs {
            let lines = cache.entry(loc.path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&loc.path)
                    .map(|s| s.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default()
            });
            let line_str = lines.get(loc.line);
            let col = line_str.map(|l| lsp::utf16_to_char(l, loc.character)).unwrap_or(0);
            let preview = line_str.map(|l| l.trim().to_string()).unwrap_or_default();
            let rel = loc
                .path
                .strip_prefix(&self.root)
                .unwrap_or(&loc.path)
                .to_string_lossy()
                .into_owned();
            hits.push(RefHit {
                rel,
                abs: loc.path,
                line: loc.line,
                col,
                preview,
            });
        }
        hits.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
        self.refs = Some(RefList {
            name: word,
            hits,
            selected: 0,
        });
    }

    /// カーソル下の語の参照一覧をオーバーレイで開く。LSP 優先、未対応/未準備なら tags。
    fn goto_references(&mut self) {
        let Some((word, line, char_col, path, line_str, text)) = self.nav_request_cursor() else {
            return;
        };
        if let Some(lang) = self.lsp.lang_id_for(&path) {
            if self.lsp.ensure_open(&path, &lang, &text) {
                self.flash = Some(format!("LSP for {lang} unavailable (using tags)"));
            }
            if self.lsp.is_ready_for(&path) {
                let col = lsp::char_to_utf16(&line_str, char_col);
                if let Some(id) = self.lsp.request_references(&path, line as u32, col) {
                    self.pending_nav.insert(
                        id,
                        PendingNav {
                            kind: NavKind::Refs,
                            word,
                            sent: Instant::now(),
                        },
                    );
                    return;
                }
            }
        }
        self.references_via_tags(&word);
    }

    /// tree-sitter-tags による参照一覧（名前ベース近似。LSP フォールバック）。
    /// 名前ベースのため、同名シンボルも含まれる近似一覧。
    fn references_via_tags(&mut self, word: &str) {
        self.index.start();
        if self.index.is_building() {
            self.flash = Some("indexing… (retry gr when ready)".into());
            return;
        }
        let locs = self.index.references(word);
        if locs.is_empty() {
            self.flash = Some(format!("no references: {word}"));
            return;
        }
        // 同じファイルは 1 回だけ読んでプレビュー行を引く。
        let mut cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut hits = Vec::new();
        for (path, line, col) in locs {
            let lines = cache.entry(path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&path)
                    .map(|s| s.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default()
            });
            let preview = lines.get(line).map(|l| l.trim().to_string()).unwrap_or_default();
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            hits.push(RefHit {
                rel,
                abs: path,
                line,
                col,
                preview,
            });
        }
        hits.sort_by(|a, b| a.rel.cmp(&b.rel).then(a.line.cmp(&b.line)));
        self.refs = Some(RefList {
            name: word.to_string(),
            hits,
            selected: 0,
        });
    }

    fn on_key_refs(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.refs = None,
            KeyCode::Enter => {
                let target = self
                    .refs
                    .as_ref()
                    .and_then(|r| r.hits.get(r.selected))
                    .map(|h| (h.abs.clone(), h.line, h.col));
                if let Some((path, line, col)) = target {
                    self.refs = None;
                    self.record_jump_origin(); // 飛ぶ前を履歴へ
                    self.open_file(&path);
                    self.jump_to_code_pos(line, col);
                }
            }
            KeyCode::Down => {
                if let Some(r) = self.refs.as_mut() {
                    r.move_down()
                }
            }
            KeyCode::Up => {
                if let Some(r) = self.refs.as_mut() {
                    r.move_up()
                }
            }
            KeyCode::Char('n') if ctrl => {
                if let Some(r) = self.refs.as_mut() {
                    r.move_down()
                }
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(r) = self.refs.as_mut() {
                    r.move_up()
                }
            }
            _ => {}
        }
    }

    /// ブランチ切替ピッカーを開く。先に `origin` を fetch して一覧を最新化する。
    fn open_branch_list(&mut self) {
        let Some(git) = self.git.as_ref() else {
            self.flash = Some("not a git repository".into());
            return;
        };
        // fetch はネットワーク待ちでブロックする（手動操作なので許容）。失敗しても続行。
        let fetched = git.fetch_origin();
        let entries = git.branches();
        if entries.is_empty() {
            self.flash = Some("no branches".into());
            return;
        }
        self.branches = Some(BranchList::new(entries));
        if !fetched {
            self.flash = Some("origin fetch failed (showing local)".into());
        }
    }

    /// クエリでブランチ一覧を絞り込み直す（選択は先頭へ）。
    fn recompute_branch_filter(&mut self) {
        let results = {
            let Some(b) = self.branches.as_ref() else {
                return;
            };
            let labels: Vec<&str> = b.entries.iter().map(|e| e.display.as_str()).collect();
            self.fuzzy.rank(&b.query, &labels)
        };
        if let Some(b) = self.branches.as_mut() {
            b.results = results;
            b.selected = 0;
        }
    }

    fn on_key_branches(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.branches = None,
            KeyCode::Enter => {
                let target = self
                    .branches
                    .as_ref()
                    .and_then(|b| b.current())
                    .map(|e| (e.target.clone(), e.is_head));
                self.branches = None;
                if let Some((target, is_head)) = target {
                    if is_head {
                        self.flash = Some(format!("already on {target}"));
                        return;
                    }
                    match self.git.as_ref().map(|g| g.checkout(&target)) {
                        Some(Ok(())) => {
                            self.reload(); // checkout 後は自動でリロード
                            self.flash = Some(format!("switched to {target}"));
                        }
                        Some(Err(e)) => self.flash = Some(format!("checkout failed: {e}")),
                        None => {}
                    }
                }
            }
            // ナビゲーションは矢印 / Ctrl-n,p（文字はクエリ入力に使うため）。
            KeyCode::Down => {
                if let Some(b) = self.branches.as_mut() {
                    b.move_down()
                }
            }
            KeyCode::Up => {
                if let Some(b) = self.branches.as_mut() {
                    b.move_up()
                }
            }
            KeyCode::Char('n') if ctrl => {
                if let Some(b) = self.branches.as_mut() {
                    b.move_down()
                }
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(b) = self.branches.as_mut() {
                    b.move_up()
                }
            }
            // あいまい絞り込み入力。
            KeyCode::Backspace => {
                if let Some(b) = self.branches.as_mut() {
                    b.query.pop();
                }
                self.recompute_branch_filter();
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(b) = self.branches.as_mut() {
                    b.query.push(c);
                }
                self.recompute_branch_filter();
            }
            _ => {}
        }
    }

    /// 検索クエリに対し、現在行を含めず次/前のマッチへ。
    fn search_jump(&mut self, forward: bool) {
        self.do_search(forward, false);
    }

    /// 確定直後：現在行を含めて最初のマッチへ。
    fn search_jump_inclusive(&mut self) {
        self.do_search(true, true);
    }

    fn do_search(&mut self, forward: bool, include_current: bool) {
        if self.search_query.is_empty() {
            return;
        }
        let q = self.search_query.to_lowercase();
        let Some(o) = self.open.as_ref() else { return };
        let n = o.raw_lines.len();
        if n == 0 {
            return;
        }
        let start = if include_current { 0 } else { 1 };
        // start..n（=n は含めない）。含めると 1 周して現在行へ戻ってしまう。
        let cursor = o.cursor_line;
        let mut found = None;
        for step in start..n {
            let idx = if forward {
                (cursor + step) % n
            } else {
                (cursor + n - (step % n)) % n
            };
            if o.raw_lines[idx].to_lowercase().contains(&q) {
                found = Some(idx);
                break;
            }
        }
        if let Some(idx) = found {
            self.record_jump_origin(); // 飛ぶ前の位置を履歴へ（カーソルはまだ動かしていない）
            if let Some(o) = self.open.as_mut() {
                o.cursor_line = idx;
                o.cursor_col = 0;
            }
            // gd 等と同様に、マッチ行を画面中央に表示する。
            self.recenter = true;
        }
    }

    /// visual mode を開始 / 解除する。同じモードの再押下で解除（vim 風）。
    fn toggle_visual(&mut self, linewise: bool) {
        match &self.selection {
            Some(s) if s.linewise == linewise => self.selection = None,
            _ => {
                if let Some(open) = self.open.as_ref() {
                    self.selection = Some(Selection {
                        anchor_line: open.cursor_line,
                        anchor_col: open.cursor_col,
                        linewise,
                    });
                }
            }
        }
    }

    /// g プレフィックス入力待ちか（ステータス表示用）。
    pub fn pending_g(&self) -> bool {
        self.pending_g
    }

    /// 変更ファイル一覧の選択を、開いているファイルに同期する（差分モードの左ペイン）。
    fn sync_changed_to_open(&mut self) {
        let Some(path) = self.open.as_ref().map(|o| o.path.clone()) else {
            return;
        };
        if let Some(idx) = self.changed.iter().position(|c| c.abs == path) {
            self.changed_selected = idx;
        }
    }

    /// side-by-side 表示が必要なら、その行を遅延構築する（必要な瞬間にだけ作る）。
    pub fn ensure_diff_split(&mut self) {
        if !self.effective_split() {
            return;
        }
        let App {
            open, highlighter, ..
        } = self;
        if let Some(o) = open.as_mut()
            && let Some(d) = o.diff.as_mut()
        {
            d.ensure_split(&o.lines, highlighter);
        }
    }

    /// いま開いている差分を side-by-side で描画するか。
    /// split 設定が ON でも、新規/削除ファイル（single_column）は単一表示にする。
    pub fn effective_split(&self) -> bool {
        self.diff_split
            && self
                .open
                .as_ref()
                .and_then(|o| o.diff.as_ref())
                .is_some_and(|d| !d.single_column)
    }

    /// アンカーとカーソルから正規化した選択範囲を返す。
    pub fn selection_region(&self) -> Option<SelRegion> {
        let sel = self.selection.as_ref()?;
        let open = self.open.as_ref()?;
        let anchor = (sel.anchor_line, sel.anchor_col);
        let cursor = (open.cursor_line, open.cursor_col);
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some(SelRegion {
            start_line: start.0,
            start_col: start.1,
            end_line: end.0,
            end_col: end.1,
            linewise: sel.linewise,
        })
    }

    /// 選択範囲をクリップボードへコピーして解除する。
    fn yank_selection(&mut self) {
        let Some(region) = self.selection_region() else {
            return;
        };
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let text = extract_selection(&open.raw_lines, &region);
        let lines = region.end_line - region.start_line + 1;
        self.flash = Some(if copy_to_clipboard(&text) {
            format!("yanked {lines} line(s)")
        } else {
            "clipboard error".into()
        });
        self.selection = None;
    }

    /// 位置情報をクリップボードへ。
    /// - 複数行選択中: `path:開始行-終了行`
    /// - 単一行選択中: `path:行`
    /// - 選択なし: `path:行:列`（カーソル、1 始まり）
    fn yank_location(&mut self) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let rel = open
            .path
            .strip_prefix(&self.root)
            .unwrap_or(&open.path)
            .to_string_lossy();
        let loc = format_location(
            &rel,
            self.selection_region().as_ref(),
            open.cursor_line,
            open.cursor_col,
        );
        self.flash = Some(if copy_to_clipboard(&loc) {
            format!("yanked {loc}")
        } else {
            "clipboard error".into()
        });
        self.selection = None;
    }

    /// 差分表示で次/前の変更ブロックへジャンプする。端では flash で知らせる。
    /// 全行表示で `@@` が 1 つに集約されても、変更の塊ごとに辿れる。
    fn hunk(&mut self, forward: bool) {
        if !self.diff_content() {
            return;
        }
        self.ensure_diff_split();
        let split = self.effective_split();
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let Some(diff) = open.diff.as_ref() else {
            return;
        };
        let anchors = diff.change_anchors_for(split);
        let total = anchors.len();
        let target = next_hunk(anchors, open.diff_scroll, forward);
        // 移動先が何番目の変更か（1 始まり）。借用はここで終える。
        let idx = target.and_then(|t| anchors.iter().position(|&a| a == t)).map(|i| i + 1);
        match target {
            Some(t) => {
                self.open.as_mut().unwrap().diff_scroll = t;
                self.flash = Some(format!("change {}/{}", idx.unwrap_or(0), total));
            }
            None => {
                self.flash = Some(if forward {
                    "last change".into()
                } else {
                    "first change".into()
                })
            }
        }
    }

    /// 次/前の変更ブロックへジャンプする。差分表示中はスクロール、コード表示中は
    /// カーソルを移動する（`}`/`{`）。どちらのモードからでも変更箇所を辿れる。
    fn change_jump(&mut self, forward: bool) {
        if self.diff_content() {
            self.hunk(forward);
        } else {
            self.goto_change(forward);
        }
    }

    /// コードビューで、HEAD との変更行（gutter の印）ブロックの先頭へカーソルを移す。
    fn goto_change(&mut self, forward: bool) {
        // 変更ブロックの先頭行を集める（連続した変更行の塊の頭）。
        let (anchors, cur) = {
            let Some(open) = self.open.as_ref() else {
                return;
            };
            let mut anchors: Vec<usize> = Vec::new();
            let mut prev_changed = false;
            for (i, m) in open.change_marks.iter().enumerate() {
                let changed = *m != diffview::LineMark::None;
                if changed && !prev_changed {
                    anchors.push(i);
                }
                prev_changed = changed;
            }
            (anchors, open.cursor_line)
        };
        if anchors.is_empty() {
            self.flash = Some("no changes".into());
            return;
        }
        let target = if forward {
            anchors.iter().copied().find(|&a| a > cur)
        } else {
            anchors.iter().copied().rev().find(|&a| a < cur)
        };
        match target {
            Some(t) => {
                let idx = anchors.iter().position(|&a| a == t).map(|i| i + 1).unwrap_or(0);
                if let Some(o) = self.open.as_mut() {
                    o.cursor_line = t.min(o.last_line());
                    o.cursor_col = 0;
                }
                self.recenter = true; // 飛んだ先を画面中央に。
                self.flash = Some(format!("change {}/{}", idx, anchors.len()));
            }
            None => {
                self.flash =
                    Some(if forward { "last change" } else { "first change" }.into());
            }
        }
    }

    /// 現在のモードに応じた順送り対象。
    fn nav_list(&self) -> NavList {
        match self.view_mode {
            ViewMode::Commits => NavList::CommitFiles,
            ViewMode::Branch => NavList::BranchFiles,
            ViewMode::Diff => NavList::Changed,
            ViewMode::Code => NavList::AllFiles,
        }
    }

    /// 変更ファイル一覧（差分）/ 全ファイル（コード）を順送りに開く共通処理。
    /// 端では flash で知らせる。
    fn navigate_files(&mut self, list: NavList, forward: bool) {
        // コミットビューは選択中コミットの変更ファイルを順送りする。
        if let NavList::CommitFiles = list {
            let len = self.commit_files.len();
            if len == 0 {
                return;
            }
            let last = len - 1;
            let cur = self.commit_file_selected.min(last);
            let next = if forward {
                (cur + 1).min(last)
            } else {
                cur.saturating_sub(1)
            };
            if next == cur {
                self.flash = Some(if forward { "last file" } else { "first file" }.into());
            } else {
                self.open_commit_file(next);
                self.focus = Focus::Content;
            }
            return;
        }
        // PR 差分ビューはデフォルトブランチとの変更ファイルを順送りする。
        if let NavList::BranchFiles = list {
            let len = self.branch_files.len();
            if len == 0 {
                return;
            }
            let last = len - 1;
            let cur = self.branch_file_selected.min(last);
            let next = if forward {
                (cur + 1).min(last)
            } else {
                cur.saturating_sub(1)
            };
            if next == cur {
                self.flash = Some(if forward { "last file" } else { "first file" }.into());
            } else {
                self.open_branch_file(next);
                self.focus = Focus::Content;
            }
            return;
        }
        let len = match list {
            NavList::Changed => self.changed.len(),
            NavList::AllFiles => self.all_files.len(),
            NavList::CommitFiles | NavList::BranchFiles => 0,
        };
        if len == 0 {
            return;
        }
        let last = len - 1;
        // 現在位置: 変更一覧は選択行、全ファイルは開いているファイルの位置。
        let cur = match list {
            NavList::Changed => Some(self.changed_selected.min(last)),
            NavList::AllFiles => self
                .open
                .as_ref()
                .and_then(|o| self.all_files.iter().position(|e| e.abs == o.path)),
            NavList::CommitFiles | NavList::BranchFiles => unreachable!(), // 上で早期 return 済み
        };
        let next = match cur {
            Some(i) if forward => (i + 1).min(last),
            Some(i) => i.saturating_sub(1),
            None if forward => 0,
            None => last,
        };
        if Some(next) == cur {
            let (fwd, bwd) = match list {
                NavList::Changed => ("last changed file", "first changed file"),
                NavList::AllFiles => ("last file", "first file"),
                NavList::CommitFiles | NavList::BranchFiles => unreachable!(),
            };
            self.flash = Some(if forward { fwd } else { bwd }.into());
            return;
        }
        let path = match list {
            NavList::Changed => {
                self.changed_selected = next;
                self.changed[next].abs.clone()
            }
            NavList::AllFiles => self.all_files[next].abs.clone(),
            NavList::CommitFiles | NavList::BranchFiles => unreachable!(),
        };
        self.open_file(&path);
        self.focus = Focus::Content;
    }

    /// AI 編集後などに、ファイル内容・git 状態・ツリー・索引を読み直す。
    fn reload(&mut self) {
        self.statuses = self.git.as_ref().map(|g| g.statuses()).unwrap_or_default();
        self.branch = self.git.as_ref().and_then(|g| g.current_branch());
        self.changed = changed_entries(&self.statuses, &self.root);
        if self.changed_selected >= self.changed.len() {
            self.changed_selected = self.changed.len().saturating_sub(1);
        }
        {
            let git = self.git.as_ref();
            let is_ignored = |p: &Path| git.is_some_and(|g| g.is_ignored(p));
            self.tree = Tree::new(&self.root, &is_ignored);
            self.all_files = collect_files(&self.root, &is_ignored);
        }
        self.all_files.sort_by(|a, b| a.rel.cmp(&b.rel));
        self.finder = Finder::from_files(self.all_files.clone());
        self.grep = ProjectSearch::new(&self.all_files);
        self.index = ProjectIndex::new(&self.root);
        self.index.start(); // 索引もバックグラウンドで作り直す

        // コミット一覧も作り直す（次回 `c` で再構築。表示中なら即時）。
        self.commits_loaded = false;
        if self.view_mode == ViewMode::Commits {
            self.load_commits();
            if self.commits.is_empty() {
                self.open = None;
            } else {
                self.select_commit(self.commit_selected.min(self.commits.len() - 1));
            }
            self.flash = Some("reloaded".into());
            return;
        }

        // PR 差分一覧も作り直す（次回 `C` で再構築。表示中なら即時）。
        self.branch_diff_loaded = false;
        if self.view_mode == ViewMode::Branch {
            self.load_branch_diff();
            if self.branch_files.is_empty() {
                self.open = None;
            } else {
                self.open_branch_file(self.branch_file_selected.min(self.branch_files.len() - 1));
            }
            self.flash = Some("reloaded".into());
            return;
        }

        // 開いているファイルはカーソル・スクロールを保ったまま読み直す。
        if let Some(open) = self.open.as_ref() {
            let path = open.path.clone();
            let (cl, cc, scroll, diff_scroll) =
                (open.cursor_line, open.cursor_col, open.scroll, open.diff_scroll);
            self.open_file(&path);
            // 再読込後の diff で split を必要時構築してからクランプする。
            self.ensure_diff_split();
            let split = self.effective_split();
            if let Some(o) = self.open.as_mut() {
                o.cursor_line = cl.min(o.last_line());
                o.cursor_col = cc.min(o.line_len(o.cursor_line).saturating_sub(1));
                o.scroll = scroll.min(o.lines.len().saturating_sub(1));
                let diff_rows = o.diff.as_ref().map_or(0, |d| d.row_count(split));
                o.diff_scroll = diff_scroll.min(diff_rows.saturating_sub(1));
            }
        }
        self.flash = Some("reloaded".into());
    }

    /// コンテンツに差分（unified/split）を表示するモードか（Diff・Commits・Branch）。
    fn diff_content(&self) -> bool {
        matches!(
            self.view_mode,
            ViewMode::Diff | ViewMode::Commits | ViewMode::Branch
        )
    }

    /// 差分 ⇄ コードをトグルする。現在の表示行を保持して相互に対応させる。
    fn toggle_view_mode(&mut self) {
        // コミットビュー中の `d` はコミットビューを抜ける。
        if self.view_mode == ViewMode::Commits {
            self.exit_commit_view();
            return;
        }
        // PR 差分ビュー中の `d` は PR 差分ビューを抜ける。
        if self.view_mode == ViewMode::Branch {
            self.exit_branch_diff();
            return;
        }
        self.selection = None;
        let next = match self.view_mode {
            ViewMode::Commits | ViewMode::Branch => ViewMode::Code, // 到達しない（上で処理）
            ViewMode::Diff => ViewMode::Code,
            ViewMode::Code => ViewMode::Diff,
        };
        // to_code は unified 行のインデックス。split 表示中は対応づけできないので
        // 行保持は unified のときだけ行う。
        let map_lines = !self.effective_split();
        if let Some(open) = self.open.as_mut()
            && map_lines
        {
            match (self.view_mode, next) {
                (ViewMode::Diff, ViewMode::Code) => {
                    if let Some(diff) = &open.diff {
                        let line = code_line_at_or_after(&diff.to_code, open.diff_scroll)
                            .unwrap_or(open.cursor_line);
                        open.cursor_line = line.min(open.last_line());
                    }
                }
                (ViewMode::Code, ViewMode::Diff) => {
                    if let Some(diff) = &open.diff {
                        open.diff_scroll = diff_row_for_code(&diff.to_code, open.cursor_line)
                            .unwrap_or(open.diff_scroll);
                    }
                }
                _ => {}
            }
        }
        self.view_mode = next;

        // 左ペインの選択を開いているファイルに同期する。
        match next {
            ViewMode::Code => {
                if let Some(path) = self.open.as_ref().map(|o| o.path.clone()) {
                    self.tree.reveal(&path);
                }
            }
            ViewMode::Diff => self.sync_changed_to_open(),
            ViewMode::Commits | ViewMode::Branch => {} // toggle_view_mode はこれらへ遷移しない
        }
    }

    // --- コミットビュー ---

    /// コミットビューの表示/非表示を切り替える（`c`）。
    fn toggle_commit_view(&mut self) {
        if self.view_mode == ViewMode::Commits {
            self.exit_commit_view();
            return;
        }
        if self.git.is_none() {
            self.flash = Some("not a git repository".into());
            return;
        }
        if !self.commits_loaded {
            self.load_commits();
        }
        self.view_mode = ViewMode::Commits;
        self.focus = Focus::Tree;
        self.selection = None;
        if self.commits.is_empty() {
            self.flash = Some("no commits to show".into());
            self.open = None;
        } else {
            let idx = self.commit_selected.min(self.commits.len() - 1);
            self.select_commit(idx);
        }
    }

    /// コミットビューを抜けてコードモードへ戻る。
    fn exit_commit_view(&mut self) {
        self.view_mode = ViewMode::Code;
        self.focus = Focus::Tree;
        self.open = None; // 表示していたのは履歴ファイル。ツリーから選び直す。
    }

    /// コミット一覧を git から構築してキャッシュする。
    fn load_commits(&mut self) {
        if let Some(log) = self.git.as_ref().map(|g| g.commit_log()) {
            self.commits = log.commits;
            self.on_default_branch = log.on_default;
            self.default_branch_label = log.default_branch;
        }
        self.commits_loaded = true;
        self.commit_selected = 0;
    }

    /// コミットを選択し、その変更ファイル一覧を読んで先頭ファイルの差分を開く。
    fn select_commit(&mut self, idx: usize) {
        self.commit_selected = idx;
        let id = match self.commits.get(idx) {
            Some(c) => c.id,
            None => {
                self.commit_files.clear();
                self.open = None;
                return;
            }
        };
        self.commit_files = self.git.as_ref().map(|g| g.commit_files(id)).unwrap_or_default();
        self.commit_file_selected = 0;
        if self.commit_files.is_empty() {
            self.open = None;
        } else {
            self.open_commit_file(0);
        }
    }

    /// 選択中コミットの指定ファイルの差分を Content に開く（LSP は使わない）。
    fn open_commit_file(&mut self, file_idx: usize) {
        let id = match self.commits.get(self.commit_selected) {
            Some(c) => c.id,
            None => return,
        };
        let rel = match self.commit_files.get(file_idx) {
            Some(f) => f.rel.clone(),
            None => {
                self.open = None;
                return;
            }
        };
        self.commit_file_selected = file_idx;

        // git からファイル内容と差分を取得（借用を閉じてからハイライトする）。
        let (content, diff_lines) = {
            let Some(git) = self.git.as_ref() else {
                return;
            };
            (
                git.commit_file_new_content(id, &rel).replace("\r\n", "\n"),
                git.commit_file_diff(id, &rel),
            )
        };
        let syntax = self.highlighter.detect(Path::new(&rel));
        let lines = self.highlighter.highlight(syntax, &content);
        let raw_lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        let diff = (!diff_lines.is_empty())
            .then(|| diffview::build(&diff_lines, &lines, &mut self.highlighter, syntax));
        let change_marks = diff
            .as_ref()
            .map(|d| d.line_marks(lines.len()))
            .unwrap_or_default();

        self.selection = None;
        self.outline_want = None; // 履歴ファイルなので LSP/アウトラインは無し。
        self.open = Some(OpenFile {
            path: PathBuf::from(&rel),
            lines,
            raw_lines,
            diff,
            scroll: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            cursor_line: 0,
            cursor_col: 0,
            outline: Vec::new(),
            outline_source: OutlineSource::TreeSitter,
            outline_selected: 0,
            change_marks,
        });
    }

    /// コミット一覧の表示データ（タイトル, 行）。
    pub fn commit_list_rows(&self, limit: usize) -> (String, Vec<ListRow>) {
        let title = if self.on_default_branch {
            format!("Log ({}) — {}", self.commits.len(), self.default_branch_label)
        } else {
            format!("Commits ({}) vs {}", self.commits.len(), self.default_branch_label)
        };
        let offset = list_offset(self.commit_selected, limit);
        let rows = self
            .commits
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, c)| ListRow {
                label: format!("{} {} {}", c.date, c.short, c.summary),
                status: None,
                selected: i == self.commit_selected,
            })
            .collect();
        (title, rows)
    }

    /// 選択中コミットの変更ファイル一覧の表示データ（タイトル, 行）。
    pub fn commit_file_rows(&self, limit: usize) -> (String, Vec<ListRow>) {
        let title = format!("Files ({})", self.commit_files.len());
        let offset = list_offset(self.commit_file_selected, limit);
        let rows = self
            .commit_files
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, f)| ListRow {
                label: f.rel.clone(),
                status: Some(f.status),
                selected: i == self.commit_file_selected,
            })
            .collect();
        (title, rows)
    }

    // --- PR 差分ビュー（デフォルトブランチとの差分） ---

    /// PR 差分ビューの表示/非表示を切り替える（`C`）。
    fn toggle_branch_diff(&mut self) {
        if self.view_mode == ViewMode::Branch {
            self.exit_branch_diff();
            return;
        }
        if self.git.is_none() {
            self.flash = Some("not a git repository".into());
            return;
        }
        if !self.branch_diff_loaded {
            self.load_branch_diff();
        }
        self.view_mode = ViewMode::Branch;
        self.focus = Focus::Tree;
        self.selection = None;
        if self.branch_files.is_empty() {
            // 「デフォルトブランチ上（分岐していない）」と「分岐したが差分ゼロ」を区別。
            self.flash = Some(if self.branch_on_default {
                format!("on default branch ({}) — no PR diff", self.branch_default_label)
            } else {
                format!("no changes vs {}", self.branch_default_label)
            });
            self.open = None;
        } else {
            let idx = self.branch_file_selected.min(self.branch_files.len() - 1);
            self.open_branch_file(idx);
        }
    }

    /// PR 差分ビューを抜けてコードモードへ戻る。
    fn exit_branch_diff(&mut self) {
        self.view_mode = ViewMode::Code;
        self.focus = Focus::Tree;
        self.open = None; // 表示していたのは HEAD 時点の内容。ツリーから選び直す。
    }

    /// デフォルトブランチとの差分一覧を git から構築してキャッシュする。
    fn load_branch_diff(&mut self) {
        if let Some(bd) = self.git.as_ref().map(|g| g.branch_diff()) {
            self.branch_files = bd.files;
            self.branch_default_label = bd.default_branch;
            self.branch_on_default = bd.on_default;
        }
        self.branch_diff_loaded = true;
        self.branch_file_selected = 0;
    }

    /// PR 差分の指定ファイルを Content に開く（HEAD blob + デフォルトブランチとの差分）。
    /// 履歴 blob ベースなので LSP は使わないが、tree-sitter アウトラインは内容から出す。
    fn open_branch_file(&mut self, file_idx: usize) {
        let rel = match self.branch_files.get(file_idx) {
            Some(f) => f.rel.clone(),
            None => {
                self.open = None;
                return;
            }
        };
        self.branch_file_selected = file_idx;

        let (content, diff_lines) = {
            let Some(git) = self.git.as_ref() else {
                return;
            };
            (
                git.branch_file_content(&rel).replace("\r\n", "\n"),
                git.branch_file_diff(&rel),
            )
        };
        let syntax = self.highlighter.detect(Path::new(&rel));
        let lines = self.highlighter.highlight(syntax, &content);
        let raw_lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        let outline = self.lang_configs.file_symbols(Path::new(&rel), content.as_bytes());
        let diff = (!diff_lines.is_empty())
            .then(|| diffview::build(&diff_lines, &lines, &mut self.highlighter, syntax));
        let change_marks = diff
            .as_ref()
            .map(|d| d.line_marks(lines.len()))
            .unwrap_or_default();

        self.selection = None;
        self.outline_want = None; // 履歴 blob なので LSP アウトライン取得はしない。
        self.open = Some(OpenFile {
            path: PathBuf::from(&rel),
            lines,
            raw_lines,
            diff,
            scroll: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            cursor_line: 0,
            cursor_col: 0,
            outline,
            outline_source: OutlineSource::TreeSitter,
            outline_selected: 0,
            change_marks,
        });
    }

    /// ステータスバー表示用の現在ブランチ名（git リポジトリでなければ `None`）。
    pub fn branch_label(&self) -> Option<String> {
        self.git.as_ref()?;
        Some(self.branch.clone().unwrap_or_else(|| "(detached)".into()))
    }

    /// ファイルを読み込み、ハイライト・差分・アウトラインを用意して開く。
    fn open_file(&mut self, path: &Path) {
        let syntax = self.highlighter.detect(path);
        let (lines, raw_lines, outline) = match std::fs::read_to_string(path) {
            Ok(source) => {
                // CRLF を正規化（カーソル/選択/表示に \r が混ざらないように）。
                let source = source.replace("\r\n", "\n");
                let lines = self.highlighter.highlight(syntax, &source);
                let raw_lines = source.split('\n').map(|s| s.to_string()).collect();
                let outline = self.lang_configs.file_symbols(path, source.as_bytes());
                (lines, raw_lines, outline)
            }
            Err(_) => (
                vec![Line::from("(binary or unreadable file)")],
                Vec::new(),
                Vec::new(),
            ),
        };

        let diff = self
            .git
            .as_ref()
            .and_then(|g| g.diff_file(path))
            .filter(|d| !d.is_empty())
            .map(|d| diffview::build(&d, &lines, &mut self.highlighter, syntax));

        // open_file は常に作業ツリーの内容を読む。履歴/ブランチ内容を見せる
        // Commits/Branch ビューから（^P 等で）直接ファイルを開いた場合は必ず抜ける。
        // 差分があれば Diff ビューは維持、無ければ Code へ。
        if diff.is_none() || matches!(self.view_mode, ViewMode::Commits | ViewMode::Branch) {
            self.view_mode = ViewMode::Code;
        }

        // コードビューの gutter 用に、行ごとの変更印を計算しておく。
        let change_marks = diff
            .as_ref()
            .map(|d| d.line_marks(lines.len()))
            .unwrap_or_default();

        self.selection = None;
        self.open = Some(OpenFile {
            path: path.to_path_buf(),
            lines,
            raw_lines,
            diff,
            scroll: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            cursor_line: 0,
            cursor_col: 0,
            outline,
            outline_source: OutlineSource::TreeSitter,
            outline_selected: 0,
            change_marks,
        });
        // LSP 対応言語なら、サーバーを（必要なら）起動して didOpen し、準備でき次第
        // documentSymbol でアウトラインを取り直す（tree-sitter 索引は即時の暫定表示）。
        self.outline_want = None;
        if let Some((p, lang, text)) = self.open.as_ref().and_then(|o| {
            self.lsp
                .lang_id_for(&o.path)
                .map(|lang| (o.path.clone(), lang, o.raw_lines.join("\n")))
        }) {
            if self.lsp.ensure_open(&p, &lang, &text) {
                self.flash = Some(format!("LSP for {lang} unavailable (using tags)"));
            }
            self.outline_want = Some(p);
        }
        // 左ペイン（ツリー／変更ファイル一覧）の選択を開いたファイルに同期する。
        self.tree.reveal(path);
        self.sync_changed_to_open();
    }
}

/// hunk 見出し行のうち、現在位置 `cur` の次（前）に来る行インデックス。
fn next_hunk(hunk_rows: &[usize], cur: usize, forward: bool) -> Option<usize> {
    if forward {
        hunk_rows.iter().copied().find(|&r| r > cur)
    } else {
        hunk_rows.iter().copied().rev().find(|&r| r < cur)
    }
}

/// 位置情報文字列を組み立てる（1 始まり）。
/// 複数行選択=`rel:s-e` / 単一行選択=`rel:s` / 選択なし=`rel:line:col`。
fn format_location(
    rel: &str,
    region: Option<&SelRegion>,
    cursor_line: usize,
    cursor_col: usize,
) -> String {
    match region {
        Some(r) if r.end_line != r.start_line => {
            format!("{rel}:{}-{}", r.start_line + 1, r.end_line + 1)
        }
        Some(r) => format!("{rel}:{}", r.start_line + 1),
        None => format!("{rel}:{}:{}", cursor_line + 1, cursor_col + 1),
    }
}

/// 選択範囲のテキストを取り出す。
fn extract_selection(raw: &[String], r: &SelRegion) -> String {
    let line_chars = |i: usize| -> Vec<char> { raw.get(i).map(|s| s.chars().collect()).unwrap_or_default() };

    if r.linewise {
        return (r.start_line..=r.end_line)
            .filter_map(|i| raw.get(i).cloned())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if r.start_line == r.end_line {
        let chars = line_chars(r.start_line);
        let end = (r.end_col + 1).min(chars.len());
        let start = r.start_col.min(end);
        return chars[start..end].iter().collect();
    }
    let mut out = String::new();
    for i in r.start_line..=r.end_line {
        let chars = line_chars(i);
        let piece: String = if i == r.start_line {
            chars[r.start_col.min(chars.len())..].iter().collect()
        } else if i == r.end_line {
            let end = (r.end_col + 1).min(chars.len());
            chars[..end].iter().collect()
        } else {
            chars.iter().collect()
        };
        out.push_str(&piece);
        if i != r.end_line {
            out.push('\n');
        }
    }
    out
}

/// テキストをシステムクリップボードへコピーする。成否を返す。
fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_string()).is_ok(),
        Err(_) => false,
    }
}

/// statuses マップから変更ファイル一覧を作る（相対パス昇順）。
fn changed_entries(statuses: &HashMap<PathBuf, FileStatus>, root: &Path) -> Vec<ChangedEntry> {
    let mut entries: Vec<ChangedEntry> = statuses
        .iter()
        .map(|(abs, status)| ChangedEntry {
            rel: abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .to_string(),
            abs: abs.clone(),
            status: *status,
        })
        .collect();
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    entries
}

/// フィルタ状態のラベルをあいまいランキングしたインデックス列。
fn filtered_indices(fuzzy: &mut Fuzzy, f: &FilterState) -> Vec<usize> {
    let refs: Vec<&str> = f.labels.iter().map(|s| s.as_str()).collect();
    fuzzy.rank(&f.query, &refs)
}

/// 選択位置がビューポートに収まるオフセット。
fn list_offset(selected: usize, limit: usize) -> usize {
    if limit > 0 && selected >= limit {
        selected - limit + 1
    } else {
        0
    }
}

/// 差分行インデックス `from` 以降で、最初にコード行へ対応する行のコード行番号。
fn code_line_at_or_after(to_code: &[Option<usize>], from: usize) -> Option<usize> {
    to_code.iter().skip(from).find_map(|c| *c)
}

/// コード行 `code_idx` に対応する差分行インデックス（一致が無ければ直近の後続）。
fn diff_row_for_code(to_code: &[Option<usize>], code_idx: usize) -> Option<usize> {
    if let Some(pos) = to_code.iter().position(|c| *c == Some(code_idx)) {
        return Some(pos);
    }
    to_code.iter().position(|c| c.is_some_and(|c| c >= code_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &[Option<usize>] = &[None, Some(0), Some(1), None, Some(2)];

    #[test]
    fn diff_to_code_skips_non_code_rows() {
        assert_eq!(code_line_at_or_after(MAP, 0), Some(0));
        assert_eq!(code_line_at_or_after(MAP, 3), Some(2));
    }

    #[test]
    fn code_to_diff_finds_exact_then_following() {
        assert_eq!(diff_row_for_code(MAP, 1), Some(2));
        assert_eq!(diff_row_for_code(&[None, Some(2), Some(5)], 3), Some(2));
    }

    fn open_with(lines: &[&str]) -> OpenFile {
        OpenFile {
            path: PathBuf::from("x.rs"),
            lines: lines.iter().map(|l| Line::from(l.to_string())).collect(),
            raw_lines: lines.iter().map(|l| l.to_string()).collect(),
            diff: None,
            scroll: 0,
            diff_scroll: 0,
            diff_hscroll: 0,
            cursor_line: 0,
            cursor_col: 0,
            outline: Vec::new(),
            outline_source: OutlineSource::TreeSitter,
            outline_selected: 0,
            change_marks: Vec::new(),
        }
    }

    #[test]
    fn word_under_cursor_extracts_identifier() {
        let mut o = open_with(&["let value = compute();"]);
        o.cursor_col = 5; // 'a' in value
        assert_eq!(o.word_under_cursor().as_deref(), Some("value"));
        o.cursor_col = 12; // 'c' in compute
        assert_eq!(o.word_under_cursor().as_deref(), Some("compute"));
    }

    #[test]
    fn vertical_move_clamps_column() {
        let mut o = open_with(&["longest line here", "x"]);
        o.cursor_col = 10;
        o.move_cursor_v(1);
        assert_eq!(o.cursor_line, 1);
        assert_eq!(o.cursor_col, 0); // "x" は 1 文字
    }

    #[test]
    fn tree_filter_narrows_files() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.view_mode = ViewMode::Code;
        app.open_tree_filter();
        if let Some(f) = app.tree_filter.as_mut() {
            f.query = "apprs".into();
        }
        match app.left_pane(100) {
            LeftPane::List { query, rows, .. } => {
                assert_eq!(query.as_deref(), Some("apprs"));
                assert!(
                    rows.iter().any(|r| r.label.contains("app.rs")),
                    "filtered rows should include app.rs"
                );
            }
            _ => panic!("expected filtered list"),
        }
    }

    #[test]
    fn next_hunk_finds_neighbor_or_none() {
        let rows = [2usize, 10, 25];
        // 前方: cur より大きい最初
        assert_eq!(next_hunk(&rows, 0, true), Some(2));
        assert_eq!(next_hunk(&rows, 2, true), Some(10));
        assert_eq!(next_hunk(&rows, 25, true), None); // 末尾より先は無い
        // 後方: cur より小さい最後
        assert_eq!(next_hunk(&rows, 25, false), Some(10));
        assert_eq!(next_hunk(&rows, 2, false), None); // 先頭より前は無い
        // 空
        assert_eq!(next_hunk(&[], 5, true), None);
    }

    #[test]
    fn location_string_handles_selection_and_cursor() {
        let none: Option<&SelRegion> = None;
        assert_eq!(format_location("a/b.rs", none, 9, 4), "a/b.rs:10:5");

        let single = SelRegion {
            start_line: 9,
            start_col: 0,
            end_line: 9,
            end_col: 3,
            linewise: false,
        };
        assert_eq!(format_location("a/b.rs", Some(&single), 9, 3), "a/b.rs:10");

        let multi = SelRegion {
            start_line: 9,
            start_col: 2,
            end_line: 19,
            end_col: 0,
            linewise: false,
        };
        assert_eq!(format_location("a/b.rs", Some(&multi), 19, 0), "a/b.rs:10-20");
    }

    #[test]
    fn extract_selection_charwise_and_linewise() {
        let raw = vec![
            "abcdef".to_string(),
            "ghijkl".to_string(),
            "mnopqr".to_string(),
        ];
        // 文字単位・単一行: 1..=3 => "bcd"
        let r = SelRegion {
            start_line: 0,
            start_col: 1,
            end_line: 0,
            end_col: 3,
            linewise: false,
        };
        assert_eq!(extract_selection(&raw, &r), "bcd");
        // 文字単位・複数行: (0,4)..(2,1) => "ef\nghijkl\nmn"
        let r = SelRegion {
            start_line: 0,
            start_col: 4,
            end_line: 2,
            end_col: 1,
            linewise: false,
        };
        assert_eq!(extract_selection(&raw, &r), "ef\nghijkl\nmn");
        // 行単位: 0..1 => "abcdef\nghijkl"
        let r = SelRegion {
            start_line: 0,
            start_col: 0,
            end_line: 1,
            end_col: 0,
            linewise: true,
        };
        assert_eq!(extract_selection(&raw, &r), "abcdef\nghijkl");
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git").args(args).current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status().unwrap().success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn change_marker_renders_for_modified_line() {
        use crate::diffview::LineMark;
        let dir = std::env::temp_dir().join(format!("srev_mark_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("code.rs"), "fn a() {}
fn b() {}
fn c() {}
").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-q", "-m", "first"]);
        // 2行目を変更
        std::fs::write(dir.join("code.rs"), "fn a() {}
fn B() {}
fn c() {}
").unwrap();

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        let path = dir.join("code.rs");
        app.open_file(&path);
        let marks = &app.open.as_ref().unwrap().change_marks;
        assert!(marks.iter().any(|m| *m != LineMark::None), "marks not computed: {marks:?}");

        // 実際に描画してマーカー文字＋変更行の背景が出るか
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        // gutter マーカー（▌/▔）
        let has_marker = buf
            .content()
            .iter()
            .any(|c| c.symbol() == "\u{258c}" || c.symbol() == "\u{2594}");
        // 変更行（2行目=Modified）は行全体に Modified 背景色が乗る。
        let modbg = ratatui::style::Color::Rgb(26, 34, 58);
        let mut max_run = 0;
        for y in 0..buf.area.height {
            let run = (0..buf.area.width).filter(|&x| buf[(x, y)].bg == modbg).count();
            max_run = max_run.max(run);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(has_marker, "no change marker rendered");
        assert!(max_run >= 10, "changed-line background not filled (max run = {max_run})");
    }

    #[test]
    fn goto_definition_lands_cursor_on_symbol_name() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut app = App::new(root.clone());
        while app.index.is_building() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            app.index.poll();
        }
        app.open_file(&root.join("src/app.rs"));
        // ProjectIndex の使用箇所にカーソルを置く
        let pos = app
            .open
            .as_ref()
            .unwrap()
            .raw_lines
            .iter()
            .enumerate()
            .find_map(|(i, l)| l.find("ProjectIndex").map(|c| (i, c + 2)));
        let (li, col) = pos.expect("ProjectIndex used in app.rs");
        if let Some(o) = app.open.as_mut() {
            o.cursor_line = li;
            o.cursor_col = col;
        }
        app.goto_definition();
        // 定義へ飛んだ後、カーソルがシンボル名の上にある（gr がそのまま効く）
        assert_eq!(
            app.open.as_ref().unwrap().word_under_cursor().as_deref(),
            Some("ProjectIndex"),
        );
    }

    #[test]
    fn refs_overlay_enter_jumps_and_closes() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut app = App::new(root.clone());
        app.open_file(&root.join("src/main.rs"));
        let target = root.join("src/app.rs");
        app.refs = Some(RefList {
            name: "jump_to_code_line".into(),
            hits: vec![RefHit {
                rel: "src/app.rs".into(),
                abs: target.clone(),
                line: 50,
                col: 0,
                preview: "self.jump_to_code_line(line);".into(),
            }],
            selected: 0,
        });
        app.on_key_refs(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.refs.is_none(), "overlay should close");
        assert_eq!(app.open.as_ref().unwrap().path, target);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 50);
        // 飛ぶ前（main.rs）が履歴に積まれている → 戻れる
        app.go_back();
        assert!(app.open.as_ref().unwrap().path.ends_with("main.rs"));
    }

    #[test]
    fn jump_history_back_and_forward() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut app = App::new(root.clone());
        app.open_file(&root.join("src/app.rs"));
        app.cursor_to(10);
        // gd 相当：飛ぶ前(10)を記録して 100 へ
        app.record_jump_origin();
        app.jump_to_code_line(100);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 100);
        // 戻る → 10
        app.go_back();
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 10);
        // 進む → 100
        app.go_forward();
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 100);
        // これ以上は進めない（位置は変わらず、flash で通知）
        app.go_forward();
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 100);
        assert!(app.flash.is_some());
    }

    #[test]
    fn jumps_pane_renders_trail_and_toggles() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut app = App::new(root.clone());
        app.open_file(&root.join("src/app.rs"));
        app.cursor_to(10);
        app.record_jump_origin();
        app.jump_to_code_line(100);

        let buf_text = |app: &mut App| -> String {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
            term.draw(|f| crate::ui::draw(f, app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        let text = buf_text(&mut app);
        assert!(text.contains("Jumps"), "jumps pane title missing");
        assert!(text.contains("src/app.rs:101"), "current jump entry missing");

        // J でトグル → 消える
        app.show_jumps = false;
        let text2 = buf_text(&mut app);
        assert!(!text2.contains("Jumps"), "jumps pane should be hidden");
    }

    #[test]
    fn tree_marks_gitignored_entries() {
        // 実リポジトリ（.gitignore に /target）で、target が表示されつつ無視色になる。
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let app = App::new(root);
        let rows = app.tree.rows();
        let target = rows.iter().find(|r| r.name == "target");
        assert!(
            target.is_some_and(|r| r.ignored),
            "target should be listed and marked ignored"
        );
        // 追跡対象（src）は無視色ではない。
        let src = rows.iter().find(|r| r.name == "src");
        assert!(src.is_some_and(|r| !r.ignored));
    }

    #[test]
    fn flash_records_then_expires() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.flash = Some("hi".into());
        app.flash_at = None;
        app.expire_flash(); // 1回目: 表示開始時刻を記録
        assert!(app.flash.is_some() && app.flash_at.is_some());
        app.expire_flash(); // TTL 内: 保持
        assert!(app.flash.is_some());

        // 時刻を過去にして TTL 超過 → 消える
        app.flash_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        app.expire_flash();
        assert!(app.flash.is_none() && app.flash_at.is_none());
    }

    #[test]
    fn file_move_navigates_all_files_in_order() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        if app.all_files.len() < 2 {
            return;
        }
        app.navigate_files(NavList::AllFiles, true); // 未オープン → 先頭ファイル
        assert_eq!(app.open.as_ref().unwrap().path, app.all_files[0].abs);
        app.navigate_files(NavList::AllFiles, true); // 次へ
        assert_eq!(app.open.as_ref().unwrap().path, app.all_files[1].abs);
        app.navigate_files(NavList::AllFiles, false); // 前へ
        assert_eq!(app.open.as_ref().unwrap().path, app.all_files[0].abs);
    }

    #[test]
    fn navigate_changed_advances_selection() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        if app.changed.len() < 2 {
            return;
        }
        app.view_mode = ViewMode::Diff;
        app.changed_selected = 0;
        app.navigate_files(NavList::Changed, true);
        assert_eq!(app.changed_selected, 1);
        assert_eq!(app.open.as_ref().unwrap().path, app.changed[1].abs);
        app.navigate_files(NavList::Changed, false);
        assert_eq!(app.changed_selected, 0);
    }

    #[test]
    fn changed_list_syncs_to_open_file() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        // 変更ファイルがあれば、その 1 つを開くと changed_selected が一致する。
        if let Some(path) = app.changed.first().map(|c| c.abs.clone()) {
            app.open_file(&path);
            assert_eq!(app.changed[app.changed_selected].abs, path);
        }
    }

    #[test]
    fn commit_view_loads_first_file_diff() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.dispatch(Action::ToggleCommits);
        assert_eq!(app.view_mode, ViewMode::Commits);
        assert!(!app.commits.is_empty(), "repo should have commits");
        // 先頭コミットの変更ファイルが読み込まれ、先頭ファイルの差分が開く。
        assert!(!app.commit_files.is_empty(), "commit should touch files");
        let open = app.open.as_ref().expect("a commit file is opened");
        assert!(open.diff.is_some(), "commit file should have a diff");
        // 左上ペインはコミット一覧。
        let (title, rows) = app.commit_list_rows(100);
        assert!(title.contains("Log") || title.contains("Commits"), "{title}");
        assert!(!rows.is_empty());
        // `c` をもう一度でコードモードへ戻る。
        app.dispatch(Action::ToggleCommits);
        assert_eq!(app.view_mode, ViewMode::Code);
    }

    #[test]
    fn commit_view_navigates_commits_and_files() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.dispatch(Action::ToggleCommits);
        if app.commits.len() < 2 {
            return;
        }
        // コミット一覧（Focus::Tree）で下移動 → 別コミットを選択し直す。
        app.focus = Focus::Tree;
        app.tree_action(Action::Down);
        assert_eq!(app.commit_selected, 1);
        // ファイル一覧（Focus::Outline）で複数ファイルがあれば下移動で切替。
        if app.commit_files.len() >= 2 {
            app.focus = Focus::Outline;
            app.outline_action(Action::Down);
            assert_eq!(app.commit_file_selected, 1);
        }
    }

    #[test]
    fn branch_diff_view_lists_files_and_opens_diff() {
        let dir = std::env::temp_dir().join(format!("srev_bview_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-qm", "init"]);
        run_git(&dir, &["checkout", "-q", "-b", "feature"]);
        // 横スクロール検証のため、2 行目を幅を超える長い行にする。
        std::fs::write(
            dir.join("a.rs"),
            "fn a() {}\nfn B() { let _ = \"a deliberately long line to exercise horizontal scrolling\"; }\n",
        )
        .unwrap();
        run_git(&dir, &["commit", "-qam", "edit a"]);

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.dispatch(Action::ToggleBranchDiff);
        assert_eq!(app.view_mode, ViewMode::Branch);
        assert!(!app.branch_files.is_empty(), "feature branch should have a PR diff");
        assert!(
            app.open.as_ref().expect("a file is opened").diff.is_some(),
            "opened branch file should have a diff"
        );

        // 左上ペインは PR ファイル一覧（タイトルに基準ブランチ名）。
        let (title, rows) = match app.left_pane(100) {
            LeftPane::List { title, rows, .. } => (title, rows),
            _ => panic!("branch view should show a file list"),
        };
        assert!(title.contains("PR vs main"), "{title}");
        assert!(!rows.is_empty());

        // 描画してパニックしないこと（オーバービュー・バーを含む）。
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

        // 横スクロール: content にフォーカスして l で進み、0 で戻る。
        app.focus = Focus::Content;
        app.content_action(Action::Right);
        assert!(app.open.as_ref().unwrap().diff_hscroll > 0, "h-scroll should advance");
        // `$`（行末送り）は末尾付近まで送るが、最低表示桁を残し本文を空にしない。
        app.content_action(Action::LineEnd);
        let longest = app
            .open
            .as_ref()
            .unwrap()
            .raw_lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap();
        let h = app.open.as_ref().unwrap().diff_hscroll;
        assert!(h > 0 && h < longest, "$ keeps some columns visible: h={h} longest={longest}");
        app.content_action(Action::LineStart);
        assert_eq!(app.open.as_ref().unwrap().diff_hscroll, 0);

        // `C` をもう一度でコードモードへ戻る。
        app.dispatch(Action::ToggleBranchDiff);
        assert_eq!(app.view_mode, ViewMode::Code);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_file_by_path_exits_branch_view_even_when_dirty() {
        // ^P/finder 相当でパス指定オープンしたら、開いたファイルに作業ツリー差分が
        // あっても PR ビューに留まらず Code へ抜ける（差分の有無で挙動が割れない）。
        let dir = std::env::temp_dir().join(format!("srev_bexit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("other.rs"), "fn o() {}\n").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-qm", "init"]);
        run_git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run_git(&dir, &["commit", "-qam", "edit a"]);
        // other.rs を未コミットで変更（作業ツリー差分あり）。
        std::fs::write(dir.join("other.rs"), "fn o() {}\nfn dirty() {}\n").unwrap();

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.dispatch(Action::ToggleBranchDiff);
        assert_eq!(app.view_mode, ViewMode::Branch);

        // PR 一覧に無い other.rs をパス指定で開く（差分ありでも Code へ抜ける）。
        app.open_file(&dir.join("other.rs"));
        assert_eq!(app.view_mode, ViewMode::Code, "path-open must leave the PR view");
        assert!(
            app.open.as_ref().unwrap().diff.is_some(),
            "other.rs has a working-tree diff (so the old code would have stayed in Branch)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_outline_jump_loads_working_tree_file() {
        // PR ビューのアウトラインからシンボルへ飛ぶと Code へ移り、表示は HEAD blob では
        // なくディスクの実ファイル内容になる。
        let dir = std::env::temp_dir().join(format!("srev_bout_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q", "-b", "main"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-qm", "init"]);
        run_git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run_git(&dir, &["commit", "-qam", "edit a"]);
        // 作業ツリーだけ HEAD と差をつける（実ファイルにのみ存在する関数）。
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\nfn working_only() {}\n").unwrap();

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.dispatch(Action::ToggleBranchDiff);
        assert_eq!(app.view_mode, ViewMode::Branch);
        // 開いているのは HEAD blob（working_only を含まない）。
        let open = app.open.as_ref().expect("a.rs opened");
        assert!(!open.outline.is_empty(), "HEAD blob should yield a symbol outline");
        assert!(
            !open.raw_lines.iter().any(|l| l.contains("working_only")),
            "branch view shows the HEAD blob, not the working tree"
        );

        // アウトラインにフォーカスして最後のシンボルへ移動し、Enter で飛ぶ。
        app.focus = Focus::Outline;
        let last = app.open.as_ref().unwrap().outline.len() - 1;
        app.open.as_mut().unwrap().outline_selected = last;
        app.dispatch(Action::Activate);

        assert_eq!(app.view_mode, ViewMode::Code, "outline jump lands in code view");
        assert!(
            app.open.as_ref().unwrap().raw_lines.iter().any(|l| l.contains("working_only")),
            "code view must show the on-disk file, not the stale HEAD blob"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_change_moves_cursor_in_code_view() {
        let dir = std::env::temp_dir().join(format!("srev_gchg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("code.rs"), "a\nb\nc\nd\ne\n").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-qm", "first"]);
        // 4 行目(index 3)だけ変更。
        std::fs::write(dir.join("code.rs"), "a\nb\nc\nD\ne\n").unwrap();

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.open_file(&dir.join("code.rs"));
        // 差分はあるがトグルしていないので通常のコードビュー。
        assert_eq!(app.view_mode, ViewMode::Code);
        app.open.as_mut().unwrap().cursor_line = 0;
        // } で変更行（index 3）へ飛ぶ。
        app.dispatch(Action::NextChange);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 3, "jumped to changed line");
        // それ以上先に変更は無い（末尾でクランプ）。
        app.dispatch(Action::NextChange);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 3);
        // { で前方向（先頭側）には変更が無い。
        app.dispatch(Action::PrevChange);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 3, "no earlier change");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_keys_jump_changes_in_code_view_when_not_searching() {
        let dir = std::env::temp_dir().join(format!("srev_nchg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["config", "user.email", "t@e.com"]);
        run_git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("code.rs"), "a\nb\nc\nd\ne\n").unwrap();
        run_git(&dir, &["add", "."]);
        run_git(&dir, &["commit", "-qm", "first"]);
        // 4 行目(index 3)だけ変更。
        std::fs::write(dir.join("code.rs"), "a\nb\nc\nD\ne\n").unwrap();

        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut app = App::new(dir.clone());
        app.open_file(&dir.join("code.rs"));
        assert_eq!(app.view_mode, ViewMode::Code);
        app.focus = Focus::Content;
        app.open.as_mut().unwrap().cursor_line = 0;

        // 検索クエリが無いので n は変更ブロックジャンプ（} と同じ）。
        assert!(app.search_query.is_empty());
        app.dispatch(Action::SearchNext);
        assert_eq!(app.open.as_ref().unwrap().cursor_line, 3, "n jumps to the change");

        // 検索クエリがある場合は従来どおり検索移動（変更ジャンプにしない）。
        app.open.as_mut().unwrap().cursor_line = 0;
        app.search_query = "a".into(); // 1 行目 "a" にマッチ
        app.dispatch(Action::SearchNext);
        assert_eq!(
            app.open.as_ref().unwrap().cursor_line,
            0,
            "with a query, n searches (only 'a' is on line 0) and does not change-jump"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_list_navigation_clamps() {
        let entry = |name: &str, head: bool| BranchEntry {
            display: name.to_string(),
            target: name.to_string(),
            is_head: head,
            is_remote: false,
        };
        let mut bl = BranchList::new(vec![entry("main", true), entry("feature", false)]);
        assert_eq!(bl.results, vec![0, 1]);
        assert_eq!(bl.current().map(|e| e.display.as_str()), Some("main"));
        bl.move_down();
        assert_eq!(bl.selected, 1);
        assert_eq!(bl.current().map(|e| e.display.as_str()), Some("feature"));
        bl.move_down(); // 末尾でクランプ
        assert_eq!(bl.selected, 1);
        bl.move_up();
        assert_eq!(bl.selected, 0);
        bl.move_up(); // 先頭でクランプ
        assert_eq!(bl.selected, 0);
    }

    #[test]
    fn branch_filter_narrows_results() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let entry = |n: &str| BranchEntry {
            display: n.to_string(),
            target: n.to_string(),
            is_head: false,
            is_remote: false,
        };
        app.branches = Some(BranchList::new(vec![
            entry("main"),
            entry("feature-login"),
            entry("feature-logout"),
            entry("hotfix"),
        ]));
        if let Some(b) = app.branches.as_mut() {
            b.query = "log".into();
        }
        app.recompute_branch_filter();
        let b = app.branches.as_ref().unwrap();
        let shown: Vec<&str> = b
            .results
            .iter()
            .map(|&i| b.entries[i].display.as_str())
            .collect();
        assert!(shown.contains(&"feature-login"), "{shown:?}");
        assert!(shown.contains(&"feature-logout"), "{shown:?}");
        assert!(!shown.contains(&"main"), "{shown:?}");
        assert!(!shown.contains(&"hotfix"), "{shown:?}");
    }

    #[test]
    fn diff_mode_left_pane_is_changed_list() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.view_mode = ViewMode::Diff;
        match app.left_pane(100) {
            LeftPane::List { title, query, .. } => {
                assert!(title.starts_with("Changed"), "title was {title}");
                assert!(query.is_none());
            }
            _ => panic!("expected changed list in diff mode"),
        }
    }
}

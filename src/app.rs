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
use crate::git::{FileStatus, GitInfo};
use crate::grep::ProjectSearch;
use crate::highlight::CodeHighlighter;
use crate::keymap::{Action, Chord, Keymap};
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

/// `[`/`]` で順送りする対象リスト。
#[derive(Clone, Copy)]
enum NavList {
    /// 変更ファイル一覧（差分モード）。
    Changed,
    /// 全ファイル（コードモード）。
    AllFiles,
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

/// アウトラインペインの描画指示。
pub enum OutlinePane {
    Empty(&'static str),
    List {
        query: Option<String>,
        rows: Vec<OutlineRow>,
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
    /// vim ライクカーソル（コード表示時、0 始まり）。
    pub cursor_line: usize,
    pub cursor_col: usize,
    /// このファイルのシンボル定義一覧（アウトライン）。
    pub outline: Vec<Symbol>,
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
        if self.view_mode == ViewMode::Diff {
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
            match action {
                Down => open.diff_scroll = (open.diff_scroll + 1).min(last),
                Up => open.diff_scroll = open.diff_scroll.saturating_sub(1),
                Top => open.diff_scroll = 0,
                Bottom => open.diff_scroll = last,
                HalfPageDown => open.diff_scroll = (open.diff_scroll + 15).min(last),
                HalfPageUp => open.diff_scroll = open.diff_scroll.saturating_sub(15),
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
            SearchNext => self.search_jump(true),
            SearchPrev => self.search_jump(false),
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

        LeftPane::Tree
    }

    /// アウトラインペインの描画指示。
    pub fn outline_pane(&mut self, limit: usize) -> OutlinePane {
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
        OutlinePane::List { query: None, rows }
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

    /// カーソル下の語の定義へジャンプする。索引が未完了なら知らせる。
    fn goto_definition(&mut self) {
        let Some(word) = self.open.as_ref().and_then(|o| o.word_under_cursor()) else {
            return;
        };
        self.index.start(); // 念のため（未開始なら開始）
        match self.index.definition(&word) {
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

    /// カーソル下の語の参照（呼び出し元など）一覧をオーバーレイで開く。
    /// 名前ベースのため、同名シンボルも含まれる近似一覧。
    fn goto_references(&mut self) {
        let Some(word) = self.open.as_ref().and_then(|o| o.word_under_cursor()) else {
            return;
        };
        self.index.start();
        if self.index.is_building() {
            self.flash = Some("indexing… (retry gr when ready)".into());
            return;
        }
        let locs = self.index.references(&word);
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
            name: word,
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

    /// 差分表示で次/前の hunk 見出し行へジャンプする。端では flash で知らせる。
    fn hunk(&mut self, forward: bool) {
        if self.view_mode != ViewMode::Diff {
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
        let target = next_hunk(diff.hunk_rows_for(split), open.diff_scroll, forward);
        match target {
            Some(t) => self.open.as_mut().unwrap().diff_scroll = t,
            None => {
                self.flash = Some(if forward {
                    "last hunk".into()
                } else {
                    "first hunk".into()
                })
            }
        }
    }

    /// 現在のモードに応じた順送り対象。
    fn nav_list(&self) -> NavList {
        if self.view_mode == ViewMode::Diff {
            NavList::Changed
        } else {
            NavList::AllFiles
        }
    }

    /// 変更ファイル一覧（差分）/ 全ファイル（コード）を順送りに開く共通処理。
    /// 端では flash で知らせる。
    fn navigate_files(&mut self, list: NavList, forward: bool) {
        let len = match list {
            NavList::Changed => self.changed.len(),
            NavList::AllFiles => self.all_files.len(),
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
        };
        self.open_file(&path);
        self.focus = Focus::Content;
    }

    /// AI 編集後などに、ファイル内容・git 状態・ツリー・索引を読み直す。
    fn reload(&mut self) {
        self.statuses = self.git.as_ref().map(|g| g.statuses()).unwrap_or_default();
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

    /// 差分 ⇄ コードをトグルする。現在の表示行を保持して相互に対応させる。
    fn toggle_view_mode(&mut self) {
        self.selection = None;
        let next = match self.view_mode {
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
        }
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

        if diff.is_none() {
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
            cursor_line: 0,
            cursor_col: 0,
            outline,
            outline_selected: 0,
            change_marks,
        });
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
            cursor_line: 0,
            cursor_col: 0,
            outline: Vec::new(),
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

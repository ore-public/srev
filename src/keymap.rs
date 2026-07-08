//! キーバインド層。物理キー（[`Chord`]）を意味アクション（[`Action`]）へ写像する。
//!
//! 既定のマップを持ちつつ、設定ファイル `[keys]` セクションで上書きできる。
//! 設定例（`~/.config/srev/config.toml`）:
//!
//! ```toml
//! [keys]
//! "ctrl-r" = "reload"
//! "r"      = "reload"      # 別キーにも割り当て
//! "x"      = "toggle_diff"
//! "d"      = "none"        # 既定の d を無効化
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 意味アクション。各ペインが文脈に応じて解釈する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    FocusNext,
    /// files ペイン（左上）へ直接フォーカス。
    FocusTree,
    /// symbols ペイン（左下）へ直接フォーカス。
    FocusOutline,
    /// code ペイン（右）へ直接フォーカス。
    FocusContent,
    Down,
    Up,
    Left,
    Right,
    Activate,
    Top,
    Bottom,
    HalfPageDown,
    HalfPageUp,
    WordForward,
    WordBack,
    LineStart,
    LineEnd,
    ToggleDiff,
    GotoDef,
    /// 参照（呼び出し元）一覧を開く。
    GotoReferences,
    Find,
    SearchNext,
    SearchPrev,
    VisualChar,
    VisualLine,
    Yank,
    YankLocation,
    FuzzyFind,
    Reload,
    CancelSelection,
    /// 次のファイルを開く（コード=全ファイル / 差分=変更ファイル）。
    NextFile,
    /// 前のファイルを開く（コード=全ファイル / 差分=変更ファイル）。
    PrevFile,
    /// 差分の unified ⇄ side-by-side 切替。
    ToggleSplit,
    /// プロジェクト全体の本文検索。
    Grep,
    /// ジャンプ履歴を戻る（飛ぶ前の位置へ）。
    JumpBack,
    /// ジャンプ履歴を進む。
    JumpForward,
    /// 右側のジャンプ履歴ペインの表示/非表示を切替。
    ToggleJumps,
    /// コミットビュー（デフォルトブランチとの差分コミット一覧）の表示/非表示を切替。
    ToggleCommits,
    /// PR 差分ビュー（デフォルトブランチとの変更ファイル一覧＋差分）の表示/非表示を切替。
    ToggleBranchDiff,
    /// 次の変更ブロックへジャンプ（コードビュー=カーソル / 差分=スクロール）。
    NextChange,
    /// 前の変更ブロックへジャンプ。
    PrevChange,
    /// ブランチ切替ピッカーを開く（origin fetch → 一覧 → 選択で checkout）。
    SwitchBranch,
    /// merge ピッカーを開く（origin fetch → 一覧 → 選択ブランチを現在ブランチへ merge）。
    MergeBranch,
    /// レビュー既読/未読のトグル（差分ビューの変更ファイル単位）。
    ToggleReviewed,
    /// blame 列の表示/非表示を切替（`gb`。コードビュー）。
    ToggleBlame,
    /// ヘルプ（キーマップ一覧）オーバーレイの表示/非表示を切替。
    ToggleHelp,
}

impl Action {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "quit" => Self::Quit,
            "focus_next" | "focus" => Self::FocusNext,
            "focus_tree" | "focus_files" => Self::FocusTree,
            "focus_outline" | "focus_symbols" => Self::FocusOutline,
            "focus_content" | "focus_code" => Self::FocusContent,
            "down" => Self::Down,
            "up" => Self::Up,
            "left" => Self::Left,
            "right" => Self::Right,
            "activate" | "open" => Self::Activate,
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "half_page_down" => Self::HalfPageDown,
            "half_page_up" => Self::HalfPageUp,
            "word_forward" => Self::WordForward,
            "word_back" => Self::WordBack,
            "line_start" => Self::LineStart,
            "line_end" => Self::LineEnd,
            "toggle_diff" => Self::ToggleDiff,
            "goto_def" => Self::GotoDef,
            "goto_references" | "references" | "goto_refs" => Self::GotoReferences,
            "find" => Self::Find,
            "search_next" => Self::SearchNext,
            "search_prev" => Self::SearchPrev,
            "visual_char" => Self::VisualChar,
            "visual_line" => Self::VisualLine,
            "yank" => Self::Yank,
            "yank_location" => Self::YankLocation,
            "fuzzy_find" => Self::FuzzyFind,
            "reload" => Self::Reload,
            "cancel" | "cancel_selection" => Self::CancelSelection,
            "next_file" | "changed_next" => Self::NextFile,
            "prev_file" | "changed_prev" => Self::PrevFile,
            "toggle_split" => Self::ToggleSplit,
            "grep" | "find_in_project" => Self::Grep,
            "jump_back" | "back" => Self::JumpBack,
            "jump_forward" | "forward" => Self::JumpForward,
            "toggle_jumps" | "jumps" => Self::ToggleJumps,
            "toggle_commits" | "commits" => Self::ToggleCommits,
            "toggle_branch_diff" | "branch_diff" | "pr" => Self::ToggleBranchDiff,
            "next_change" | "change_next" => Self::NextChange,
            "prev_change" | "change_prev" => Self::PrevChange,
            "switch_branch" | "branch" | "branches" => Self::SwitchBranch,
            "merge_branch" | "merge" => Self::MergeBranch,
            "toggle_reviewed" | "review" | "reviewed" => Self::ToggleReviewed,
            "toggle_blame" | "blame" => Self::ToggleBlame,
            "help" | "toggle_help" => Self::ToggleHelp,
            _ => return None,
        })
    }

    /// ヘルプ表示順（カテゴリ順）。`describe` と対で全アクションを列挙する。
    /// keymap に既定束縛のないもの（gg/gd/gr=Top/GotoDef/GotoReferences, gb=ToggleBlame）は
    /// ヘルプ側で g プレフィックスとして別途明示するため、ここに含めても描画では拾われない。
    pub fn all() -> &'static [Action] {
        use Action::*;
        &[
            Quit,
            ToggleHelp,
            FocusNext,
            FocusTree,
            FocusOutline,
            FocusContent,
            ToggleDiff,
            ToggleCommits,
            ToggleBranchDiff,
            ToggleSplit,
            Down,
            Up,
            Left,
            Right,
            Activate,
            Bottom,
            HalfPageDown,
            HalfPageUp,
            WordForward,
            WordBack,
            LineStart,
            LineEnd,
            Find,
            SearchNext,
            SearchPrev,
            NextChange,
            PrevChange,
            NextFile,
            PrevFile,
            VisualChar,
            VisualLine,
            Yank,
            YankLocation,
            CancelSelection,
            FuzzyFind,
            Grep,
            JumpBack,
            JumpForward,
            ToggleJumps,
            SwitchBranch,
            MergeBranch,
            ToggleReviewed,
            Reload,
        ]
    }

    /// ヘルプ表示用の短い説明（英語。README のアクション表が原典）。
    pub fn describe(self) -> &'static str {
        use Action::*;
        match self {
            Quit => "Quit",
            FocusNext => "Cycle focus (tree → outline → content)",
            FocusTree => "Focus files pane",
            FocusOutline => "Focus symbols pane",
            FocusContent => "Focus code pane",
            Down => "Move down",
            Up => "Move up",
            Left => "Move left / collapse directory",
            Right => "Move right / open",
            Activate => "Open / confirm",
            Top => "Jump to top (gg)",
            Bottom => "Jump to bottom (G)",
            HalfPageDown => "Half-page down",
            HalfPageUp => "Half-page up",
            WordForward => "Word forward",
            WordBack => "Word back",
            LineStart => "Line start",
            LineEnd => "Line end",
            ToggleDiff => "Toggle diff ⇄ code",
            GotoDef => "Go to definition (gd)",
            GotoReferences => "List references (gr)",
            Find => "In-file search",
            SearchNext => "Next match / next change",
            SearchPrev => "Previous match / previous change",
            VisualChar => "Visual mode (character)",
            VisualLine => "Visual mode (line)",
            Yank => "Copy selection",
            YankLocation => "Copy location",
            FuzzyFind => "Fuzzy file search",
            Reload => "Pull + reload",
            CancelSelection => "Cancel / close",
            NextFile => "Next file",
            PrevFile => "Previous file",
            ToggleSplit => "Toggle unified ⇄ side-by-side",
            Grep => "Project-wide content search",
            JumpBack => "Jump history: back",
            JumpForward => "Jump history: forward",
            ToggleJumps => "Toggle jump-history pane",
            ToggleCommits => "Toggle commit view",
            ToggleBranchDiff => "Toggle PR diff view",
            NextChange => "Next change block",
            PrevChange => "Previous change block",
            SwitchBranch => "Switch branch",
            MergeBranch => "Merge a branch",
            ToggleReviewed => "Mark reviewed / unreviewed (diff)",
            ToggleBlame => "Toggle blame column (gb)",
            ToggleHelp => "Toggle this help",
        }
    }
}

/// 物理キー（修飾は ctrl のみ追跡。shift は文字の大小で表現）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
}

impl Chord {
    pub fn from_event(key: KeyEvent) -> Self {
        Self {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        }
    }

    /// ヘルプ表示用の人間可読なキー表記（`parse` の逆。完全な往復ではなく表示優先）。
    pub fn display(&self) -> String {
        let key = match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "del".to_string(),
            other => format!("{other:?}"),
        };
        if self.ctrl { format!("ctrl-{key}") } else { key }
    }

    fn parse(s: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut shift = false;
        let parts: Vec<&str> = s.split('-').collect();
        let (mods, token) = parts.split_at(parts.len() - 1);
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "c" => ctrl = true,
                "shift" | "s" => shift = true,
                _ => return None,
            }
        }
        let token = token[0];
        let code = match token.to_ascii_lowercase().as_str() {
            "tab" => KeyCode::Tab,
            "enter" | "cr" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "backspace" | "bs" => KeyCode::Backspace,
            "del" | "delete" => KeyCode::Delete,
            _ => {
                let mut chars = token.chars();
                let (c, rest) = (chars.next()?, chars.next());
                if rest.is_some() {
                    return None; // 不明な複数文字トークン
                }
                let c = if shift { c.to_ascii_uppercase() } else { c };
                KeyCode::Char(c)
            }
        };
        Some(Self { code, ctrl })
    }
}

pub struct Keymap {
    map: HashMap<Chord, Action>,
}

impl Keymap {
    pub fn get(&self, chord: Chord) -> Option<Action> {
        self.map.get(&chord).copied()
    }

    /// 現在の束縛一覧（ヘルプ表示用）。順序は不定なので呼び出し側で整える。
    pub fn bindings(&self) -> Vec<(Chord, Action)> {
        self.map.iter().map(|(c, a)| (*c, *a)).collect()
    }

    /// 既定マップを読み込み、設定ファイルがあれば上書きする。
    pub fn load() -> Self {
        let mut keymap = Self::defaults();
        if let Some(path) = config_path()
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            keymap.apply_config(&text);
        }
        keymap
    }

    fn defaults() -> Self {
        use Action::*;
        let ch = |c: char| Chord {
            code: KeyCode::Char(c),
            ctrl: false,
        };
        let ctrl = |c: char| Chord {
            code: KeyCode::Char(c),
            ctrl: true,
        };
        let key = |code: KeyCode| Chord { code, ctrl: false };

        let mut map = HashMap::new();
        let mut add = |chord: Chord, action: Action| {
            map.insert(chord, action);
        };

        add(ch('q'), Quit);
        add(key(KeyCode::Tab), FocusNext);
        add(ch('1'), FocusTree);
        add(ch('2'), FocusOutline);
        add(ch('3'), FocusContent);
        add(ch('d'), ToggleDiff);
        add(ctrl('p'), FuzzyFind);
        add(ctrl('r'), Reload);

        add(ch('j'), Down);
        add(key(KeyCode::Down), Down);
        add(ch('k'), Up);
        add(key(KeyCode::Up), Up);
        add(ch('h'), Left);
        add(key(KeyCode::Left), Left);
        add(ch('l'), Right);
        add(key(KeyCode::Right), Right);
        add(key(KeyCode::Enter), Activate);

        add(ch('G'), Bottom);
        add(ctrl('d'), HalfPageDown);
        add(key(KeyCode::PageDown), HalfPageDown);
        add(ctrl('u'), HalfPageUp);
        add(key(KeyCode::PageUp), HalfPageUp);
        add(ch('w'), WordForward);
        add(ch('b'), WordBack);
        add(ch('0'), LineStart);
        add(key(KeyCode::Home), LineStart);
        add(ch('$'), LineEnd);
        add(key(KeyCode::End), LineEnd);

        add(ch('/'), Find);
        add(ch('n'), SearchNext);
        add(ch('N'), SearchPrev);
        add(ch('v'), VisualChar);
        add(ch('V'), VisualLine);
        add(ch('y'), Yank);
        add(ch('Y'), YankLocation);
        add(key(KeyCode::Esc), CancelSelection);
        add(ch(']'), NextFile);
        add(ch('['), PrevFile);
        add(ch('s'), ToggleSplit);
        add(ctrl('f'), Grep);
        // ジャンプ履歴：左右対称の ( / ) と、vim 風の Ctrl-O（戻る）。
        add(ch('('), JumpBack);
        add(ch(')'), JumpForward);
        add(ctrl('o'), JumpBack);
        add(ch('J'), ToggleJumps); // 右のジャンプ履歴ペイン表示切替
        add(ch('c'), ToggleCommits); // コミットビュー（デフォルトブランチとの差分）
        add(ch('C'), ToggleBranchDiff); // PR 差分ビュー（デフォルトブランチとの変更ファイル一覧＋差分）
        // 変更ブロックへのジャンプ。] [（ファイル）と対になる } {（変更）。コード/差分の両方で効く。
        add(ch('}'), NextChange);
        add(ch('{'), PrevChange);
        add(ctrl('v'), SwitchBranch); // ブランチ切替（origin fetch → 一覧 → checkout）
        add(ch('m'), MergeBranch); // merge ピッカー（origin fetch → 一覧 → 現在ブランチへ merge）
        add(ch(' '), ToggleReviewed); // 変更ファイルの既読/未読トグル（差分ビュー）
        add(ch('?'), ToggleHelp); // キーマップ一覧オーバーレイ
        // 注: blame は `gb`（g プレフィックス、keymap 外で pending_g が解決）。

        Self { map }
    }

    fn apply_config(&mut self, text: &str) {
        let Ok(table) = toml::from_str::<toml::Table>(text) else {
            return;
        };
        let Some(keys) = table.get("keys").and_then(|v| v.as_table()) else {
            return;
        };
        for (chord_str, value) in keys {
            let Some(chord) = Chord::parse(chord_str) else {
                continue;
            };
            let Some(action_name) = value.as_str() else {
                continue;
            };
            if action_name == "none" {
                self.map.remove(&chord);
            } else if let Some(action) = Action::from_name(action_name) {
                self.map.insert(chord, action);
            }
        }
    }
}

/// 設定ファイルのパスを解決する。`SREV_CONFIG` を最優先。
fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SREV_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("srev").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn parses_chords() {
        assert_eq!(
            Chord::parse("ctrl-p"),
            Some(Chord {
                code: KeyCode::Char('p'),
                ctrl: true
            })
        );
        assert_eq!(
            Chord::parse("tab"),
            Some(Chord {
                code: KeyCode::Tab,
                ctrl: false
            })
        );
        assert_eq!(
            Chord::parse("Y"),
            Some(Chord {
                code: KeyCode::Char('Y'),
                ctrl: false
            })
        );
    }

    #[test]
    fn config_overrides_and_unbinds() {
        let mut km = Keymap::defaults();
        km.apply_config("[keys]\n\"ctrl-r\" = \"reload\"\n\"x\" = \"toggle_diff\"\n\"d\" = \"none\"\n");
        assert_eq!(
            km.get(Chord {
                code: KeyCode::Char('x'),
                ctrl: false
            }),
            Some(Action::ToggleDiff)
        );
        // d は無効化された
        assert_eq!(
            km.get(Chord {
                code: KeyCode::Char('d'),
                ctrl: false
            }),
            None
        );
    }

    #[test]
    fn chord_display_and_describe_cover_all() {
        let d = |code, ctrl| Chord { code, ctrl }.display();
        assert_eq!(d(KeyCode::Char('q'), false), "q");
        assert_eq!(d(KeyCode::Char('p'), true), "ctrl-p");
        assert_eq!(d(KeyCode::Char(' '), false), "space");
        assert_eq!(d(KeyCode::Tab, false), "tab");
        // all() の各アクションに非空の説明がある（match の網羅性は型で保証）。
        for &a in Action::all() {
            assert!(!a.describe().is_empty(), "empty describe for {a:?}");
        }
        // 既定束縛: ? = ToggleHelp、space = ToggleReviewed。
        let km = Keymap::defaults();
        assert_eq!(
            km.get(Chord {
                code: KeyCode::Char('?'),
                ctrl: false
            }),
            Some(Action::ToggleHelp)
        );
        assert_eq!(
            km.get(Chord {
                code: KeyCode::Char(' '),
                ctrl: false
            }),
            Some(Action::ToggleReviewed)
        );
    }

    #[test]
    fn number_keys_jump_to_panes() {
        let km = Keymap::defaults();
        let ch = |c: char| Chord {
            code: KeyCode::Char(c),
            ctrl: false,
        };
        assert_eq!(km.get(ch('1')), Some(Action::FocusTree));
        assert_eq!(km.get(ch('2')), Some(Action::FocusOutline));
        assert_eq!(km.get(ch('3')), Some(Action::FocusContent));
        // 0 は vim 風の行頭移動のまま（競合させない）。
        assert_eq!(km.get(ch('0')), Some(Action::LineStart));
    }
}

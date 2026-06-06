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
}

impl Action {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "quit" => Self::Quit,
            "focus_next" | "focus" => Self::FocusNext,
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
            _ => return None,
        })
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
}

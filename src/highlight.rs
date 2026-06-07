//! syntect（Sublime 構文 + 正規表現）でソースをハイライトし、ratatui の
//! `Line`/`Span` へ変換する。
//!
//! 構文セットは `two-face`（bat 相当の拡張セット, 200+ 言語）を使う。tree-sitter
//! は使わないため、対応言語を自由に増やせる（PHP・HAML・TOML・TypeScript 等）。
//! Markdown はフェンス内コードも syntect が言語別に色付けする。

use std::path::Path;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// 解決済みの構文への軽量ハンドル（`SyntaxSet` 内のインデックス）。
#[derive(Clone, Copy)]
pub struct Syntax {
    idx: usize,
}

/// ハイライター本体。構文セットとテーマを保持し、行ごとに色付けする。
pub struct CodeHighlighter {
    ps: SyntaxSet,
    theme: Theme,
}

impl CodeHighlighter {
    pub fn new() -> Self {
        let ps = two_face::syntax::extra_newlines();
        // syntect 同梱の暗色テーマ。
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("builtin dark theme");
        Self { ps, theme }
    }

    fn index_of(&self, sr: &SyntaxReference) -> usize {
        self.ps
            .syntaxes()
            .iter()
            .position(|s| std::ptr::eq(s, sr))
            .unwrap_or(0)
    }

    /// 無装飾（プレーンテキスト）の構文ハンドル。
    #[allow(dead_code)] // 主にテスト用。
    pub fn plain(&self) -> Syntax {
        Syntax {
            idx: self.index_of(self.ps.find_syntax_plain_text()),
        }
    }

    /// パスから構文を推定する。フルファイル名（`Cargo.lock`/`Makefile` 等）→
    /// 拡張子 → プレーンの順で解決。`.blade.php` は拡張子 `php` として PHP に。
    pub fn detect(&self, path: &Path) -> Syntax {
        let fname = path.file_name().and_then(|n| n.to_str());
        let ext = path.extension().and_then(|e| e.to_str());
        let sr = fname
            .and_then(|n| self.ps.find_syntax_by_extension(n))
            .or_else(|| ext.and_then(|e| self.ps.find_syntax_by_extension(e)))
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        Syntax {
            idx: self.index_of(sr),
        }
    }

    /// ソース全体をハイライト済みの行ベクタへ変換する。
    /// 行数は `source.split('\n')` と一致させる（カーソル/差分印の添字整合のため）。
    pub fn highlight(&self, syntax: Syntax, source: &str) -> Vec<Line<'static>> {
        let sr = &self.ps.syntaxes()[syntax.idx];
        let mut h = HighlightLines::new(sr, &self.theme);
        let mut out = Vec::new();
        for line in source.split('\n') {
            // 複数行構文の状態維持のため改行を付けて渡し、表示時に取り除く。
            let with_nl = format!("{line}\n");
            let ranges = h.highlight_line(&with_nl, &self.ps).unwrap_or_default();
            let spans: Vec<Span<'static>> = ranges
                .iter()
                .map(|(st, text)| {
                    let text = text.strip_suffix('\n').unwrap_or(text);
                    Span::styled(text.to_string(), convert(*st))
                })
                .filter(|s| !s.content.is_empty())
                .collect();
            out.push(Line::from(spans));
        }
        out
    }
}

impl Default for CodeHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

/// syntect の `Style` を ratatui の `Style` へ。前景色と装飾のみ採用し、
/// 背景色は採用しない（カーソル行/変更行などの行背景と競合させないため）。
fn convert(s: SynStyle) -> Style {
    let fg = s.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if s.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn has_rgb(lines: &[Line]) -> bool {
        lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        })
    }

    #[test]
    fn line_count_matches_source_split() {
        let h = CodeHighlighter::new();
        let src = "fn main() {\n    let x = 1;\n}\n";
        let lines = h.highlight(h.detect(&PathBuf::from("a.rs")), src);
        assert_eq!(lines.len(), src.split('\n').count());
    }

    #[test]
    fn highlights_rust_with_color() {
        let h = CodeHighlighter::new();
        let lines = h.highlight(h.detect(&PathBuf::from("main.rs")), "fn main() {}\n");
        assert!(has_rgb(&lines), "expected colored spans");
    }

    #[test]
    fn detects_php_haml_toml_typescript() {
        let h = CodeHighlighter::new();
        // 解決した構文で色が付く＝その言語が構文セットに含まれる。
        for (path, sample) in [
            ("index.php", "<?php echo 1;\n"),
            ("tpl.blade.php", "<?php echo 1;\n"),
            ("legacy.phtml", "<?php echo 1;\n"),
            ("view.haml", "%p Hello\n"),
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("app.ts", "const x: number = 1;\n"),
        ] {
            let lines = h.highlight(h.detect(&PathBuf::from(path)), sample);
            assert!(has_rgb(&lines), "{path} should be highlighted");
        }
    }

    #[test]
    fn markdown_fenced_code_is_highlighted() {
        let h = CodeHighlighter::new();
        let src = "# Title\n\n```rust\nfn main() {}\n```\n";
        let lines = h.highlight(h.detect(&PathBuf::from("README.md")), src);
        assert_eq!(lines.len(), src.split('\n').count());
        assert!(has_rgb(&lines), "expected markdown + fence highlighting");
    }
}

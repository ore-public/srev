//! inkjet（tree-sitter）のハイライト結果を ratatui の `Line`/`Span` へ変換する。
//!
//! 配色は inkjet に同梱された Helix テーマ（ONEDARK）を読み込み、
//! ハイライト名 → 色 のフォールバック解決（`a.b.c` → `a.b` → `a`）で引く。

use std::path::Path;
use std::sync::LazyLock;

use inkjet::Language;
use inkjet::constants::HIGHLIGHT_NAMES;
use inkjet::theme::{self, Modifier as InkMod, Style as InkStyle, Theme};
use inkjet::tree_sitter_highlight::{
    HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

static THEME: LazyLock<Theme> =
    LazyLock::new(|| Theme::from_helix(theme::vendored::ONEDARK).expect("bundled theme is valid"));

/// Markdown の block / inline 文法（tree-sitter）。inkjet には同梱されないため自前で用意。
/// HIGHLIGHT_NAMES で configure しておくことで `style_for` がそのまま使える。
/// tree-sitter-md 同梱クエリは旧 nvim 系 `@text.*` を使うが、inkjet の
/// HIGHLIGHT_NAMES とテーマは helix 系 `@markup.*` を使う。capture 名を
/// 置換して、見出し・強調・リンク等にテーマ色が乗るようにする。
fn helixify(query: &str) -> String {
    query
        .replace("@text.title", "@markup.heading")
        .replace("@text.strong", "@markup.bold")
        .replace("@text.emphasis", "@markup.italic")
        .replace("@text.literal", "@markup.raw.inline")
        .replace("@text.uri", "@markup.link.url")
        .replace("@text.reference", "@markup.link.text")
        .replace("@text.quote", "@markup.quote")
}

/// Markdown 設定を組み立てる（失敗時は `None` で Markdown を無装飾フォールバック）。
fn md_config(
    lang: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
) -> Option<HighlightConfiguration> {
    let mut c = HighlightConfiguration::new(lang, name, &helixify(highlights), injections, "").ok()?;
    c.configure(HIGHLIGHT_NAMES);
    Some(c)
}

static MD_BLOCK: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
    md_config(
        tree_sitter_md::LANGUAGE.into(),
        "markdown",
        tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        tree_sitter_md::INJECTION_QUERY_BLOCK,
    )
});
static MD_INLINE: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
    md_config(
        tree_sitter_md::INLINE_LANGUAGE.into(),
        "markdown_inline",
        tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
        tree_sitter_md::INJECTION_QUERY_INLINE,
    )
});

/// ファイルの言語種別。inkjet が扱える言語か、専用処理する Markdown か。
#[derive(Clone, Copy, Debug)]
pub enum Syntax {
    Lang(Language),
    Markdown,
}

/// 注入された言語名を HighlightConfiguration へ解決する（Markdown のコードブロック等）。
///
/// 返す設定はすべて `'static` だが、戻り値の寿命を呼び出し側に選ばせるため
/// ライフタイムを汎用化している（そうしないと `highlight` の `'a` が `'static`
/// に固定され、`&mut self`/`source` が漏れる E0521 になる）。
fn inject_config<'a>(name: &str) -> Option<&'a HighlightConfiguration> {
    match name {
        "markdown_inline" | "markdown-inline" | "inline" => MD_INLINE.as_ref(),
        // ```rust などフェンス内コードは inkjet の同梱文法で色付け。
        other => Language::from_token(other).map(|l| l.config()),
    }
}

/// 既定（無装飾）テキストの配色。テーマの前景色を使う。
fn default_style() -> Style {
    Style::default().fg(to_rgb(THEME.fg))
}

/// ファイルパスから inkjet の言語を推定する。拡張子優先、なければファイル名。
pub(crate) fn detect_language(path: &Path) -> Language {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && let Some(lang) = Language::from_token(ext.to_ascii_lowercase())
    {
        return lang;
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && let Some(lang) = Language::from_token(name.to_lowercase())
    {
        return lang;
    }
    Language::Plaintext
}

/// ファイルパスから構文種別を判定する。Markdown は専用処理に振り分ける。
pub fn detect_syntax(path: &Path) -> Syntax {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if matches!(
            ext.to_ascii_lowercase().as_str(),
            "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdwn"
        ) {
            return Syntax::Markdown;
        }
    }
    Syntax::Lang(detect_language(path))
}

/// ハイライターのラッパ。言語ごとの設定はグローバルにキャッシュされるため使い回す。
pub struct CodeHighlighter {
    inner: inkjet::Highlighter,
    ts: TsHighlighter,
}

impl CodeHighlighter {
    pub fn new() -> Self {
        Self {
            inner: inkjet::Highlighter::new(),
            ts: TsHighlighter::new(),
        }
    }

    /// ソース全体をハイライト済みの行ベクタに変換する。
    pub fn highlight(&mut self, syntax: Syntax, source: &str) -> Vec<Line<'static>> {
        // 借用したイテレータは一旦 Vec<HighlightEvent> に集めてから描画する
        // （self/source の借用が描画関数へ漏れないようにするため）。
        let events: Option<Vec<HighlightEvent>> = match syntax {
            Syntax::Lang(lang) => self
                .inner
                .highlight_raw(lang, &source)
                .ok()
                .map(|it| it.filter_map(Result::ok).collect()),
            Syntax::Markdown => MD_BLOCK.as_ref().and_then(|cfg| {
                self.ts
                    .highlight(cfg, source.as_bytes(), None, inject_config)
                    .ok()
                    .map(|it| it.filter_map(Result::ok).collect())
            }),
        };
        match events {
            Some(events) => render_events(source, events.into_iter()),
            None => plain_lines(source),
        }
    }
}

/// HighlightEvent 列を ratatui の行ベクタへ変換する（inkjet / tree-sitter 共通）。
fn render_events(source: &str, events: impl Iterator<Item = HighlightEvent>) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for event in events {
        match event {
            HighlightEvent::HighlightStart(h) => stack.push(h.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = stack
                    .last()
                    .map(|&idx| style_for(idx))
                    .unwrap_or_else(default_style);
                push_text(&mut lines, &mut current, &source[start..end], style);
            }
        }
    }
    lines.push(Line::from(std::mem::take(&mut current)));
    lines
}

/// 改行を含むテキストを行に分割しつつ Span として積む。
fn push_text(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
) {
    let mut segments = text.split('\n');
    if let Some(first) = segments.next()
        && !first.is_empty()
    {
        current.push(Span::styled(first.to_string(), style));
    }
    for seg in segments {
        lines.push(Line::from(std::mem::take(current)));
        if !seg.is_empty() {
            current.push(Span::styled(seg.to_string(), style));
        }
    }
}

/// ハイライト不能時のフォールバック（無装飾の行分割）。
fn plain_lines(source: &str) -> Vec<Line<'static>> {
    source
        .split('\n')
        .map(|l| Line::styled(l.to_string(), default_style()))
        .collect()
}

/// ハイライトインデックス → ratatui スタイル（名前のフォールバック付き）。
fn style_for(idx: usize) -> Style {
    let mut key = HIGHLIGHT_NAMES.get(idx).copied().unwrap_or("");
    loop {
        if let Some(style) = THEME.get_style(key) {
            return convert(style);
        }
        match key.rfind('.') {
            Some(pos) => key = &key[..pos],
            None => return default_style(),
        }
    }
}

fn convert(style: &InkStyle) -> Style {
    let mut out = Style::default().fg(style.fg.map(to_rgb).unwrap_or(to_rgb(THEME.fg)));
    if let Some(bg) = style.bg {
        out = out.bg(to_rgb(bg));
    }
    for m in &style.modifiers {
        out = out.add_modifier(match m {
            InkMod::Bold => Modifier::BOLD,
            InkMod::Dim => Modifier::DIM,
            InkMod::Italic => Modifier::ITALIC,
            InkMod::Underlined => Modifier::UNDERLINED,
            InkMod::SlowBlink => Modifier::SLOW_BLINK,
            InkMod::FastBlink => Modifier::RAPID_BLINK,
            InkMod::Reversed => Modifier::REVERSED,
            InkMod::Hidden => Modifier::HIDDEN,
            InkMod::Strikethrough => Modifier::CROSSED_OUT,
            InkMod::Normal => Modifier::empty(),
        });
    }
    out
}

fn to_rgb(c: theme::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_by_extension() {
        assert!(matches!(
            detect_language(&PathBuf::from("src/main.rs")),
            Language::Rust
        ));
        assert!(matches!(
            detect_language(&PathBuf::from("a/b/script.py")),
            Language::Python
        ));
    }

    fn has_rgb(lines: &[Line]) -> bool {
        lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        })
    }

    #[test]
    fn highlights_rust_into_multiple_lines_with_color() {
        let mut h = CodeHighlighter::new();
        let src = "fn main() {\n    let x = 1;\n}\n";
        let lines = h.highlight(Syntax::Lang(Language::Rust), src);
        // 末尾改行込みで 4 行（最後は空行）
        assert!(lines.len() >= 3, "got {} lines", lines.len());
        assert!(has_rgb(&lines), "expected at least one rgb-colored span");
    }

    #[test]
    fn detect_markdown_extension() {
        assert!(matches!(
            detect_syntax(&PathBuf::from("docs/README.md")),
            Syntax::Markdown
        ));
        assert!(matches!(
            detect_syntax(&PathBuf::from("a/b.rs")),
            Syntax::Lang(Language::Rust)
        ));
    }

    #[test]
    fn highlights_markdown_with_fenced_code() {
        let mut h = CodeHighlighter::new();
        let src = "# Title\n\nSome **bold** text.\n\n```rust\nfn main() {}\n```\n";
        let lines = h.highlight(Syntax::Markdown, src);
        assert!(lines.len() >= 6, "got {} lines", lines.len());
        // 見出し・強調・フェンス内コードのいずれかが色付けされていること
        assert!(has_rgb(&lines), "expected markdown highlighting to add color");
    }

    /// 行テキストを連結して、その行を構成する Span 数を返す。
    fn span_count_of_line(lines: &[Line], needle: &str) -> Option<usize> {
        lines.iter().find_map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            (text.trim() == needle).then_some(l.spans.len())
        })
    }

    #[test]
    fn markdown_markup_itself_is_colored() {
        let mut h = CodeHighlighter::new();
        // コードフェンス無し。見出し・強調だけで色が付くこと（capture 名の置換確認）。
        let lines = h.highlight(Syntax::Markdown, "# Heading\n\nsome **bold** word\n");
        assert!(
            has_rgb(&lines),
            "expected markdown headings/emphasis to be colored"
        );
    }

    #[test]
    fn markdown_injects_language_into_code_fence() {
        let mut h = CodeHighlighter::new();
        // 既知言語(rust)のフェンスはトークンへ分割され Span が増える。
        let rust = h.highlight(Syntax::Markdown, "```rust\nfn main() {}\n```\n");
        // 未知言語のフェンスは injection されず素のまま（Span 1 個）。
        let unknown = h.highlight(Syntax::Markdown, "```zzqq\nfn main() {}\n```\n");

        let rust_spans = span_count_of_line(&rust, "fn main() {}").expect("rust code line");
        let unknown_spans = span_count_of_line(&unknown, "fn main() {}").expect("unknown code line");

        assert!(
            rust_spans > unknown_spans,
            "expected rust injection to tokenize the fence (rust={rust_spans}, unknown={unknown_spans})"
        );
    }
}

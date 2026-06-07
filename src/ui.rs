use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::{
    App, Focus, LeftPane, ListRow, OpenFile, OutlinePane, OutlineRow, RefList, SelRegion, ViewMode,
};
use crate::diffview::LineMark;
use crate::finder::Finder;
use crate::git::FileStatus;
use crate::tree::Row;

const CURSOR_LINE_BG: Color = Color::Rgb(38, 42, 54);
const MATCH_LINE_BG: Color = Color::Rgb(60, 54, 24);
const SELECTION_BG: Color = Color::Rgb(55, 60, 95);

/// 全体のレイアウトを描画する。
///
/// ┌── tree ───┬───── content ─────┐
/// │           │                   │
/// ├── outline ┤                   │
/// │           │                   │
/// ├───────────┴───────────────────┤
/// │ help / search bar             │
/// └───────────────────────────────┘
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    // 左ペインは画面幅の約 1/3（深い階層でもファイル名が見えるよう）。
    // 狭い端末では最低 32 桁、広い端末でも 70 桁までに収める。
    let left_w = (body.width / 3).clamp(32, 70);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_w), Constraint::Min(0)]).areas(body);
    // 右端にジャンプ履歴ペインを確保（表示 ON かつ十分広いとき）。残りが本文。
    let (content_area, jumps_area) = if app.show_jumps && right.width >= 60 {
        let jw = (right.width / 4).clamp(24, 36);
        let [c, j] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(jw)]).areas(right);
        (c, Some(j))
    } else {
        (right, None)
    };
    let [tree_area, outline_area] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(left);

    // 左ペインの表示データを先に組み立てる（あいまいランキングのため &mut）。
    let left_pane = app.left_pane(tree_area.height.saturating_sub(3) as usize);
    let outline_pane = app.outline_pane(outline_area.height.saturating_sub(3) as usize);

    render_tree(frame, tree_area, app, &left_pane);
    render_outline(frame, outline_area, &outline_pane, app.focus == Focus::Outline);

    // コード表示ではカーソルが見えるようスクロールを追従させる。
    // ジャンプ直後（recenter）はカーソルを画面中央に置く。
    let content_inner_h = content_area.height.saturating_sub(2) as usize;
    if app.view_mode == ViewMode::Code {
        let recenter = std::mem::take(&mut app.recenter);
        if let Some(open) = app.open.as_mut() {
            if recenter {
                center_scroll(open, content_inner_h);
            } else {
                adjust_scroll(open, content_inner_h);
            }
        }
    }

    // side-by-side で描画する場合はその行を遅延構築しておく。
    app.ensure_diff_split();

    render_content(frame, content_area, app);
    if let Some(jumps_area) = jumps_area {
        render_jumps(frame, jumps_area, app);
    }
    render_status(frame, status, app);

    if app.finder.active {
        render_finder(frame, frame.area(), &app.finder);
    }
    if app.grep.active {
        render_grep(frame, frame.area(), &app.grep);
    }
    if let Some(refs) = &app.refs {
        render_refs(frame, frame.area(), refs);
    }

    // 端末カーソルをコード上の位置に表示。
    place_cursor(frame, content_area, app);
}

/// カーソル行がビューポートに入るようスクロール位置を調整する。
fn adjust_scroll(open: &mut OpenFile, height: usize) {
    if height == 0 {
        return;
    }
    if open.cursor_line < open.scroll {
        open.scroll = open.cursor_line;
    } else if open.cursor_line >= open.scroll + height {
        open.scroll = open.cursor_line - height + 1;
    }
}

/// カーソル行を画面中央に置く（先頭/末尾付近はベストエフォート）。
fn center_scroll(open: &mut OpenFile, height: usize) {
    open.scroll = center_offset(open.lines.len(), open.cursor_line, height);
}

/// 中央化したときのスクロール先頭行。末尾側は最後のページに収める。
fn center_offset(total: usize, cursor: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let max_scroll = total.saturating_sub(height);
    cursor.saturating_sub(height / 2).min(max_scroll)
}

fn render_tree(frame: &mut Frame, area: Rect, app: &App, pane: &LeftPane) {
    let focused = app.focus == Focus::Tree;
    match pane {
        // 階層ツリー（コードモード・フィルタ非アクティブ）。
        LeftPane::Tree => {
            let block = pane_block("Files", focused);
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let rows = app.tree.rows();
            let selected = app.tree.selected;
            let height = inner.height as usize;
            let offset = scroll_offset(selected, height);
            let lines: Vec<Line> = rows
                .iter()
                .enumerate()
                .skip(offset)
                .take(height)
                .map(|(i, row)| {
                    tree_line(row, i == selected, app.statuses.get(&row.path).copied())
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
        // フラットリスト（変更ファイル / フィルタ結果）。
        LeftPane::List { title, query, rows } => {
            render_list_pane(frame, area, title, query.as_deref(), rows, focused);
        }
    }
}

/// 左ペインのフラットリストを描画する。
fn render_list_pane(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    query: Option<&str>,
    rows: &[ListRow],
    focused: bool,
) {
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let body = if let Some(q) = query {
        let [q_area, list_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let q_line = Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(q.to_string()),
        ]);
        frame.render_widget(Paragraph::new(q_line), q_area);
        list_area
    } else {
        inner
    };

    let lines: Vec<Line> = rows
        .iter()
        .map(|row| {
            let label_style = if row.selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let mut spans = Vec::new();
            if let Some(status) = row.status {
                spans.push(Span::styled(
                    format!("{} ", status.letter()),
                    Style::default().fg(status_color(status)),
                ));
            }
            spans.push(Span::styled(row.label.clone(), label_style));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

fn render_outline(frame: &mut Frame, area: Rect, pane: &OutlinePane, focused: bool) {
    let block = pane_block("Symbols", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match pane {
        OutlinePane::Empty(msg) => {
            frame.render_widget(Paragraph::new(Line::from(*msg).dim()), inner);
        }
        OutlinePane::List { query, rows } => {
            let body = if let Some(q) = query {
                let [q_area, list_area] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
                let q_line = Line::from(vec![
                    Span::styled("/", Style::default().fg(Color::Cyan)),
                    Span::raw(q.clone()),
                ]);
                frame.render_widget(Paragraph::new(q_line), q_area);
                list_area
            } else {
                inner
            };
            let lines: Vec<Line> = rows.iter().map(outline_line).collect();
            frame.render_widget(Paragraph::new(lines), body);
        }
    }
}

fn outline_line(row: &OutlineRow) -> Line<'static> {
    let name_style = if row.selected {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(vec![
        Span::styled(kind_glyph(&row.kind), Style::default().fg(Color::Yellow)),
        Span::styled(row.name.clone(), name_style),
    ])
}

/// シンボル種別を短い記号にする。
fn kind_glyph(kind: &str) -> String {
    let g = match kind {
        "function" | "method" | "function.method" => "ƒ ",
        "class" | "struct" | "interface" | "type" | "enum" | "module" => "◇ ",
        "constant" | "constructor" => "● ",
        _ => "• ",
    };
    g.to_string()
}

fn tree_line(row: &Row, selected: bool, status: Option<FileStatus>) -> Line<'static> {
    let indent = "  ".repeat(row.depth);
    let marker = if row.is_dir {
        if row.expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let label = format!("{indent}{marker}{}", row.name);

    let base = if selected {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else if row.ignored {
        // .gitignore 対象は淡色で区別。
        Style::default().fg(Color::DarkGray)
    } else if row.is_dir {
        Style::default().fg(Color::Blue)
    } else {
        Style::default()
    };

    let mut spans = vec![Span::styled(label, base)];
    if let Some(status) = status {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            status.letter().to_string(),
            Style::default().fg(status_color(status)),
        ));
    }
    Line::from(spans)
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added | FileStatus::Untracked => Color::Green,
        FileStatus::Modified | FileStatus::Renamed => Color::Yellow,
        FileStatus::Deleted => Color::Red,
    }
}

fn render_content(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Content;
    let mode = match app.view_mode {
        ViewMode::Diff if app.effective_split() => "Diff (split)",
        ViewMode::Diff => "Diff",
        ViewMode::Code => "Code",
    };
    let title = match &app.open {
        Some(open) => {
            let rel = open.path.strip_prefix(&app.root).unwrap_or(&open.path);
            format!("{mode} — {}", rel.to_string_lossy())
        }
        None => mode.to_string(),
    };
    let block = pane_block(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(open) = &app.open else {
        frame.render_widget(
            Paragraph::new(Line::from("Select a file (Enter to open)").dim()),
            inner,
        );
        return;
    };

    match app.view_mode {
        ViewMode::Diff if open.has_diff() => render_diff(frame, inner, open, app.effective_split()),
        ViewMode::Diff => {
            frame.render_widget(
                Paragraph::new(Line::from("(no diff for this file — d for code)").dim()),
                inner,
            );
        }
        ViewMode::Code => {
            let sel = focused.then(|| app.selection_region()).flatten();
            render_code(frame, inner, open, focused, &app.search_query, sel.as_ref());
        }
    }
}

/// 変更行の背景色（追加=緑系, 変更=青系）。削除印は行自体は変わらないので無し。
/// 暗い端末でもはっきり分かる濃さにする。
fn change_line_bg(mark: LineMark) -> Option<Color> {
    match mark {
        LineMark::Added => Some(Color::Rgb(20, 44, 28)),
        LineMark::Modified => Some(Color::Rgb(26, 34, 58)),
        LineMark::None | LineMark::DeletedAbove => None,
    }
}

/// 変更印の gutter マーカー（追加=緑, 変更=青, 削除あり=赤）。
fn change_marker(mark: LineMark) -> Span<'static> {
    let (ch, color) = match mark {
        LineMark::Added => ("▌", Color::Green),
        LineMark::Modified => ("▌", Color::Blue),
        LineMark::DeletedAbove => ("▔", Color::Red),
        LineMark::None => (" ", Color::Reset),
    };
    Span::styled(ch, Style::default().fg(color))
}

fn render_code(
    frame: &mut Frame,
    area: Rect,
    open: &OpenFile,
    focused: bool,
    search: &str,
    sel: Option<&SelRegion>,
) {
    let height = area.height as usize;
    let total = open.lines.len();
    let num_width = total.to_string().len().max(3);
    let search_lc = (!search.is_empty()).then(|| search.to_lowercase());

    let lines: Vec<Line> = open
        .lines
        .iter()
        .enumerate()
        .skip(open.scroll)
        .take(height)
        .map(|(i, line)| {
            // 変更印（HEAD との差分）をエディタ風に gutter 左端へ。
            let mark = open
                .change_marks
                .get(i)
                .copied()
                .unwrap_or(LineMark::None);
            let marker = change_marker(mark);
            let gutter = Span::styled(
                format!("{:>num_width$} ", i + 1),
                Style::default().fg(Color::DarkGray),
            );
            let line_len = open.raw_lines.get(i).map_or(0, |l| l.chars().count());
            let line_sel = sel.and_then(|r| line_selection(r, i, line_len));

            let mut spans = Vec::with_capacity(line.spans.len() + 2);
            spans.push(marker);
            spans.push(gutter);
            // 選択範囲があれば該当セルに選択背景を載せる。
            if let Some((s, e, _)) = line_sel {
                spans.extend(apply_selection(&line.spans, s, e));
            } else {
                spans.extend(line.spans.iter().cloned());
            }
            let mut out = Line::from(spans);

            // 行全体の背景（選択フィル > 現在行 > 検索マッチ > 変更行の薄い色）。
            let is_match = search_lc.as_ref().is_some_and(|q| {
                open.raw_lines.get(i).is_some_and(|l| l.to_lowercase().contains(q))
            });
            if matches!(line_sel, Some((_, _, true))) {
                out = out.style(Style::default().bg(SELECTION_BG));
            } else if focused && i == open.cursor_line {
                out = out.style(Style::default().bg(CURSOR_LINE_BG));
            } else if is_match {
                out = out.style(Style::default().bg(MATCH_LINE_BG));
            } else if let Some(bg) = change_line_bg(mark) {
                out = out.style(Style::default().bg(bg));
            }
            out
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// 行 `i` の選択範囲を (開始 char, 終了 char(排他), 行末まで埋めるか) で返す。
fn line_selection(r: &SelRegion, i: usize, line_len: usize) -> Option<(usize, usize, bool)> {
    if i < r.start_line || i > r.end_line {
        return None;
    }
    if r.linewise {
        return Some((0, line_len, true));
    }
    let start = if i == r.start_line { r.start_col } else { 0 };
    let end = if i == r.end_line {
        (r.end_col + 1).min(line_len)
    } else {
        line_len
    };
    let fill = i != r.end_line || end >= line_len;
    Some((start, end, fill))
}

/// コード行の Span 群に、[start, end) の char 範囲だけ選択背景を載せる。
fn apply_selection(spans: &[Span<'static>], start: usize, end: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut col = 0usize;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let mut idx = 0;
        while idx < chars.len() {
            let selected = {
                let a = col + idx;
                a >= start && a < end
            };
            let mut j = idx;
            while j < chars.len() && ((col + j >= start && col + j < end) == selected) {
                j += 1;
            }
            let text: String = chars[idx..j].iter().collect();
            let style = if selected {
                span.style.bg(SELECTION_BG)
            } else {
                span.style
            };
            out.push(Span::styled(text, style));
            idx = j;
        }
        col += chars.len();
    }
    out
}

fn render_diff(frame: &mut Frame, area: Rect, open: &OpenFile, split: bool) {
    let Some(diff) = &open.diff else { return };
    let height = area.height as usize;

    let unified = |frame: &mut Frame| {
        let lines: Vec<Line> = diff
            .rows
            .iter()
            .skip(open.diff_scroll)
            .take(height)
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    };

    // split 指定でも未構築なら unified にフォールバック（通常は draw 側で構築済み）。
    let split_rows = match (split, diff.split_rows()) {
        (true, Some(rows)) => rows,
        _ => {
            unified(frame);
            return;
        }
    };

    // side-by-side: 左=旧 / 中=区切り / 右=新。
    let [left_area, sep_area, right_area] = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    let mut lefts = Vec::with_capacity(height);
    let mut rights = Vec::with_capacity(height);
    for row in split_rows.iter().skip(open.diff_scroll).take(height) {
        lefts.push(row.left.clone());
        rights.push(row.right.clone());
    }
    let seps: Vec<Line> = (0..lefts.len())
        .map(|_| Line::styled("│", Style::default().fg(Color::DarkGray)))
        .collect();

    frame.render_widget(Paragraph::new(lefts), left_area);
    frame.render_widget(Paragraph::new(seps), sep_area);
    frame.render_widget(Paragraph::new(rights), right_area);
}

/// 端末カーソルをコード上のカーソル位置に置く。
fn place_cursor(frame: &mut Frame, content_area: Rect, app: &App) {
    if app.focus != Focus::Content || app.view_mode != ViewMode::Code || app.search_input.is_some() {
        return;
    }
    let Some(open) = &app.open else { return };
    let inner_x = content_area.x + 1;
    let inner_y = content_area.y + 1;
    let inner_h = content_area.height.saturating_sub(2) as usize;
    if open.cursor_line < open.scroll || open.cursor_line >= open.scroll + inner_h {
        return;
    }
    let num_width = open.lines.len().to_string().len().max(3);
    // 変更印マーカー(1) + 行番号(num_width) + スペース(1)。
    let gutter = num_width as u16 + 2;
    let x = inner_x + gutter + open.cursor_col.min(u16::MAX as usize) as u16;
    let y = inner_y + (open.cursor_line - open.scroll) as u16;
    let max_x = content_area.x + content_area.width.saturating_sub(2);
    frame.set_cursor_position((x.min(max_x), y));
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(buf) = &app.search_input {
        let line = Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(buf.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some(msg) = &app.flash {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Black).bg(Color::Green),
            )),
            area,
        );
        return;
    }
    // g プレフィックス入力待ちを明示（gg=先頭, gd=定義）。
    if app.pending_g() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" g-", Style::default().fg(Color::Yellow)),
                Span::styled("  g:top  d:def", Style::default().fg(Color::DarkGray)),
            ])),
            area,
        );
        return;
    }

    // `/` の意味はフォーカスで変わる（本文=検索 / ツリー・シンボル=絞り込み）。
    let find_word = if app.focus == Focus::Content {
        "search"
    } else {
        "filter"
    };
    // モード別・優先度順のヒント。端末幅に入るぶんだけ前から詰める（見切れ防止）。
    let items: Vec<(&str, &str)> = if app.view_mode == ViewMode::Diff {
        vec![
            ("q", "quit"),
            ("Tab", "pane"),
            ("] [", "Next/Prev File"),
            ("n/N", "hunk"),
            ("s", "split"),
            ("d", "code"),
            ("^P", "Find File"),
            ("^F", "Search Text"),
            ("/", find_word),
            ("^R", "reload"),
        ]
    } else {
        vec![
            ("q", "quit"),
            ("Tab", "pane"),
            ("^P", "Find File"),
            ("^F", "Search Text"),
            ("] [", "Next/Prev File"),
            ("/", find_word),
            ("gd", "def"),
            ("gr", "refs"),
            ("( )", "jump back/fwd"),
            ("J", "jumps pane"),
            ("d", "diff"),
            ("v/V/y", "yank"),
            ("Y", "loc"),
            ("^R", "reload"),
        ]
    };
    // 区切り線で項目を分け、キー（明）と説明（暗）を対比させる。
    let max = area.width as usize;
    let sep = " │ ";
    let mut used = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    for (i, (k, desc)) in items.into_iter().enumerate() {
        let sep_w = if i == 0 { 0 } else { sep.chars().count() };
        let w = sep_w + (k.len() + 2) + 1 + desc.len(); // [sep] + " k " + " desc"
        if used + w > max {
            break;
        }
        if i != 0 {
            spans.push(Span::styled(sep, Style::default().fg(Color::DarkGray)));
        }
        spans.push(key(k));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(Color::Gray),
        ));
        used += w;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 画面中央にあいまい検索オーバーレイを描画する。
fn render_finder(frame: &mut Frame, area: Rect, finder: &Finder) {
    let popup = centered_rect(area, 70, 60);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" Open file by name (^P) ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let query = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Magenta)),
        Span::raw(finder.query.clone()),
    ]);
    frame.render_widget(Paragraph::new(query), query_area);

    let rows: Vec<Line> = finder
        .visible(list_area.height as usize)
        .into_iter()
        .map(|(path, selected, ignored)| {
            let style = if selected {
                Style::default().fg(Color::Black).bg(Color::Magenta)
            } else if ignored {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::styled(path.to_string(), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);
}

/// プロジェクト全体検索のオーバーレイ。
fn render_grep(frame: &mut Frame, area: Rect, grep: &crate::grep::ProjectSearch) {
    let popup = centered_rect(area, 80, 70);
    frame.render_widget(Clear, popup);

    let count = grep.result_count();
    let title = if grep.truncated() {
        format!(" Search in project (^F) — {count}+ matches ")
    } else {
        format!(" Search in project (^F) — {count} matches ")
    };
    let needle = grep.query.to_lowercase();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let query = Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::raw(grep.query.clone()),
    ]);
    frame.render_widget(Paragraph::new(query), query_area);

    let rows: Vec<Line> = grep
        .visible(list_area.height as usize)
        .into_iter()
        .map(|(hit, selected)| {
            let loc_style = if selected {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let mut spans = vec![
                Span::styled(format!("{}:{}", hit.rel, hit.line + 1), loc_style),
                Span::raw("  "),
            ];
            spans.extend(highlight_match(&hit.preview, &needle));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);
}

/// 参照（呼び出し元）一覧オーバーレイ。`rel:line  preview` を並べ、名前を強調。
fn render_refs(frame: &mut Frame, area: Rect, refs: &RefList) {
    let popup = centered_rect(area, 80, 70);
    frame.render_widget(Clear, popup);

    let title = format!(" References to `{}` — {} ", refs.name, refs.hits.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green))
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let h = inner.height as usize;
    let max_start = refs.hits.len().saturating_sub(h);
    let start = refs.selected.saturating_sub(h / 2).min(max_start);
    let needle = refs.name.to_lowercase();

    let rows: Vec<Line> = refs
        .hits
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, hit)| {
            let loc_style = if i == refs.selected {
                Style::default().fg(Color::Black).bg(Color::Green)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let mut spans = vec![
                Span::styled(format!("{}:{}", hit.rel, hit.line + 1), loc_style),
                Span::raw("  "),
            ];
            spans.extend(highlight_match(&hit.preview, &needle));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);
}

/// プレビュー行内のマッチ部分（大文字小文字無視）を強調した Span 列を返す。
fn highlight_match(text: &str, needle_lower: &str) -> Vec<Span<'static>> {
    let base = Style::default().fg(Color::Gray);
    let hit = Style::default().fg(Color::Yellow);
    if needle_lower.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_lowercase();
    let mut spans = Vec::new();
    let mut start = 0;
    // バイト位置でスキャン（lower と text は同じ長さの ASCII 化を仮定せず、
    // 小文字化で長さが変わりうるが、ここでは素朴に char 境界で扱う）。
    let chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let needle: Vec<char> = needle_lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let end = i + needle.len();
        if end <= chars.len() && end <= lower_chars.len() && lower_chars[i..end] == needle[..] {
            if start < i {
                spans.push(Span::styled(chars[start..i].iter().collect::<String>(), base));
            }
            spans.push(Span::styled(
                chars[i..i + needle.len()].iter().collect::<String>(),
                hit,
            ));
            i += needle.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start < chars.len() {
        spans.push(Span::styled(chars[start..].iter().collect::<String>(), base));
    }
    spans
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(h);
    v
}

/// 選択位置がビューポートに収まるスクロールオフセット。
fn scroll_offset(selected: usize, height: usize) -> usize {
    if height > 0 && selected >= height {
        selected - height + 1
    } else {
        0
    }
}

fn key(label: &str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default().bg(Color::DarkGray).fg(Color::White),
    )
}

/// フォーカス時に枠を強調するブロックを作る。
/// 右端のジャンプ履歴ペイン。古い「戻る」先 → 現在地（▶）→「進む」先を縦に並べる。
fn render_jumps(frame: &mut Frame, area: Rect, app: &App) {
    let block = pane_block(" Jumps ", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.jumps_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("(no jumps yet)").dim()),
            inner,
        );
        return;
    }

    let rows = app.jump_trail();
    let h = inner.height as usize;
    // 現在地が中央付近に来るようスクロール。
    let cur = rows.iter().position(|r| r.current).unwrap_or(0);
    let max_start = rows.len().saturating_sub(h);
    let start = cur.saturating_sub(h / 2).min(max_start);

    let lines: Vec<Line> = rows
        .iter()
        .skip(start)
        .take(h)
        .map(|r| {
            if r.current {
                Line::from(vec!["▶ ".cyan(), r.label.clone().cyan().bold()])
            } else {
                Line::from(vec!["  ".into(), r.label.clone().gray()])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn render_to_string(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_three_panes_and_help() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let text = render_to_string(&mut app);
        // 左上は code モードなら "Files"、diff モードなら "Changed"。
        assert!(
            text.contains("Files") || text.contains("Changed"),
            "tree/changed pane missing"
        );
        assert!(text.contains("Symbols"), "outline pane missing");
        // 既定はコードモード。コード向けヒントが出る。
        assert!(text.contains("quit"), "help bar missing");
        assert!(text.contains("def"), "help missing code-mode gd hint");
    }

    #[test]
    fn highlight_match_marks_case_insensitive_occurrences() {
        let spans = highlight_match("Foo BAR foo", "foo");
        let yellow: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(yellow, vec!["Foo", "foo"]);
        let full: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(full, "Foo BAR foo");
    }

    #[test]
    fn highlight_match_empty_needle_is_single_span() {
        assert_eq!(highlight_match("abc", "").len(), 1);
    }

    #[test]
    fn center_offset_centers_and_clamps() {
        // 中央: カーソルが画面中央（height/2 行目）に来る
        assert_eq!(center_offset(100, 50, 20), 40); // 50-10
        // 先頭付近: スクロール 0
        assert_eq!(center_offset(100, 3, 20), 0);
        // 末尾付近: 最後のページに収める（max_scroll=80）
        assert_eq!(center_offset(100, 98, 20), 80);
        // ファイルがビューより短い: スクロールなし
        assert_eq!(center_offset(10, 5, 20), 0);
    }

    #[test]
    fn renders_finder_overlay() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.finder.open();
        let text = render_to_string(&mut app);
        assert!(text.contains("Open file"), "finder overlay missing");
    }

    #[test]
    fn renders_grep_overlay() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.grep.open();
        for c in "fn ".chars() {
            app.grep.push_char(c);
        }
        let text = render_to_string(&mut app);
        assert!(text.contains("Search in project"), "grep overlay missing");
    }
}

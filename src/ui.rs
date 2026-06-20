use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::{
    App, BranchList, Focus, LeftPane, ListRow, OpenFile, OutlinePane, OutlineRow, PickerMode,
    RefList, SelRegion, ViewMode,
};
use crate::diffview::{LineMark, RowChange};
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

    // コミットビューでは左上=コミット一覧、左下=変更ファイル一覧。
    if app.view_mode == ViewMode::Commits {
        let (ctitle, crows) = app.commit_list_rows(tree_area.height.saturating_sub(2) as usize);
        render_list_pane(
            frame,
            tree_area,
            &ctitle,
            None,
            &crows,
            app.focus == Focus::Tree,
        );
        let (ftitle, frows) =
            app.commit_file_rows(outline_area.height.saturating_sub(2) as usize);
        render_list_pane(
            frame,
            outline_area,
            &ftitle,
            None,
            &frows,
            app.focus == Focus::Outline,
        );
    } else {
        // 左ペインの表示データを先に組み立てる（あいまいランキングのため &mut）。
        let left_pane = app.left_pane(tree_area.height.saturating_sub(3) as usize);
        let outline_pane = app.outline_pane(outline_area.height.saturating_sub(3) as usize);

        render_tree(frame, tree_area, app, &left_pane);
        render_outline(frame, outline_area, &outline_pane, app.focus == Focus::Outline);
    }

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
    if let Some(branches) = &app.branches {
        render_branches(frame, frame.area(), branches);
    }
    if app.show_help {
        render_help(frame, frame.area(), app);
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
            } else if row.reviewed == Some(true) {
                Style::default().fg(Color::DarkGray) // 既読は淡色
            } else {
                Style::default()
            };
            let mut spans = Vec::new();
            if let Some(reviewed) = row.reviewed {
                let (glyph, color) = if reviewed {
                    ("✓ ", Color::Green)
                } else {
                    ("○ ", Color::DarkGray)
                };
                spans.push(Span::styled(glyph, Style::default().fg(color)));
            }
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
    // 抽出元（LSP / tree-sitter）をタイトルに併記する。
    let title = match pane {
        OutlinePane::List {
            source: Some(s), ..
        } => format!("Symbols · {s}"),
        _ => "Symbols".to_string(),
    };
    let block = pane_block(&title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match pane {
        OutlinePane::Empty(msg) => {
            frame.render_widget(Paragraph::new(Line::from(*msg).dim()), inner);
        }
        OutlinePane::List { query, rows, .. } => {
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
        ViewMode::Commits if app.effective_split() => "Commit (split)",
        ViewMode::Commits => "Commit",
        ViewMode::Branch if app.effective_split() => "PR (split)",
        ViewMode::Branch => "PR",
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
        ViewMode::Diff | ViewMode::Commits | ViewMode::Branch if open.has_diff() => {
            render_diff(frame, inner, open, app.effective_split())
        }
        ViewMode::Diff => {
            frame.render_widget(
                Paragraph::new(Line::from("(no diff for this file — d for code)").dim()),
                inner,
            );
        }
        ViewMode::Commits | ViewMode::Branch => {
            frame.render_widget(
                Paragraph::new(Line::from("(no textual diff for this file)").dim()),
                inner,
            );
        }
        ViewMode::Code => {
            let sel = focused.then(|| app.selection_region()).flatten();
            render_code(
                frame,
                inner,
                open,
                focused,
                &app.search_query,
                sel.as_ref(),
                app.show_blame,
            );
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

/// blame 列の表示幅（セル）。`{短SHA} {日付} {著者}` を表示幅で切り詰め・右詰めする。
const BLAME_W: usize = 30;

fn render_code(
    frame: &mut Frame,
    area: Rect,
    open: &OpenFile,
    focused: bool,
    search: &str,
    sel: Option<&SelRegion>,
    show_blame: bool,
) {
    // 右端 1 桁を変更オーバービュー・バーに割り当て、残りをコード本文にする。
    let (area, bar_area) = if area.width > 2 {
        let [c, b] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        (c, Some(b))
    } else {
        (area, None)
    };
    let height = area.height as usize;
    let total = open.lines.len();
    let num_width = total.to_string().len().max(3);
    let search_lc = (!search.is_empty()).then(|| search.to_lowercase());
    // blame 列（表示中かつ計算済みのときだけ）。各行に 1 要素対応。
    let blame = show_blame.then(|| open.blame.as_deref()).flatten();

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

            let mut spans = Vec::with_capacity(line.spans.len() + 3);
            spans.push(marker);
            spans.push(gutter);
            // blame 列（表示中は常に固定幅を確保。未コミット行など対応が無い行は空白で詰める）。
            if let Some(bl) = blame {
                let cell = match bl.get(i) {
                    Some(b) => {
                        let content = format!("{} {} {}", b.short, b.date, b.author);
                        crate::width::pad_display(&content, BLAME_W - 1)
                    }
                    None => " ".repeat(BLAME_W - 1),
                };
                spans.push(Span::styled(
                    format!("{cell} "),
                    Style::default().fg(Color::DarkGray),
                ));
            }
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

    // 変更行の在処を右端のミニマップで可視化（gutter 印と同じ色分け）。
    if let Some(bar_area) = bar_area {
        let changes: Vec<RowChange> = (0..total)
            .map(|i| match open.change_marks.get(i).copied().unwrap_or(LineMark::None) {
                LineMark::Added => RowChange::Add,
                LineMark::Modified => RowChange::Mod,
                LineMark::DeletedAbove => RowChange::Del,
                LineMark::None => RowChange::None,
            })
            .collect();
        render_overview_bar(frame, bar_area, &changes, total, open.scroll, height);
    }
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

    // 右端 1 桁を変更オーバービュー・バーに割り当て、残りを差分本文にする。
    let (content_area, bar_area) = if area.width > 2 {
        let [c, b] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        (c, Some(b))
    } else {
        (area, None)
    };
    let height = content_area.height as usize;
    // 長い行が幅に収まらないときの横スクロール量（桁）。
    let hscroll = open.diff_hscroll as u16;

    // split 指定でも未構築なら unified にフォールバック（通常は draw 側で構築済み）。
    match (split, diff.split_rows()) {
        (true, Some(split_rows)) => {
            // side-by-side: 左=旧 / 中=区切り / 右=新。
            let [left_area, sep_area, right_area] = Layout::horizontal([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(content_area);

            let mut lefts = Vec::with_capacity(height);
            let mut rights = Vec::with_capacity(height);
            for row in split_rows.iter().skip(open.diff_scroll).take(height) {
                lefts.push(row.left.clone());
                rights.push(row.right.clone());
            }
            let seps: Vec<Line> = (0..lefts.len())
                .map(|_| Line::styled("│", Style::default().fg(Color::DarkGray)))
                .collect();

            frame.render_widget(Paragraph::new(lefts).scroll((0, hscroll)), left_area);
            frame.render_widget(Paragraph::new(seps), sep_area);
            frame.render_widget(Paragraph::new(rights).scroll((0, hscroll)), right_area);
        }
        _ => {
            let lines: Vec<Line> = diff
                .rows
                .iter()
                .skip(open.diff_scroll)
                .take(height)
                .cloned()
                .collect();
            frame.render_widget(Paragraph::new(lines).scroll((0, hscroll)), content_area);
        }
    }

    if let Some(bar_area) = bar_area {
        render_overview_bar(
            frame,
            bar_area,
            diff.row_changes_for(split),
            diff.row_count(split),
            open.diff_scroll,
            height,
        );
    }
}

/// 右端の変更オーバービュー・バー（ミニマップ）。ファイル全体を縦に圧縮し、
/// 変更行の位置を色で示す。現在の表示範囲（ビューポート）は背景で強調する。
fn render_overview_bar(
    frame: &mut Frame,
    area: Rect,
    changes: &[RowChange],
    total: usize,
    scroll: usize,
    viewport: usize,
) {
    let h = area.height as usize;
    if h == 0 || total == 0 {
        return;
    }
    let view_start = scroll;
    let view_end = (scroll + viewport).min(total);
    // 現在表示範囲（スクロールバーのつまみ）の見せ方。変更が無い行でも実体のある
    // 明るいブロックで描き、現在位置がはっきり見えるようにする。
    const THUMB: Color = Color::Rgb(132, 144, 178); // つまみ（変更色と混ざらない明るい灰）
    const VIEW_BG: Color = Color::Rgb(64, 70, 92); // 変更マーカーが表示範囲内であることの背景

    // 種別の強さ（範囲内で最も目立つ変更を採用）。
    let rank = |c: RowChange| match c {
        RowChange::None => 0,
        RowChange::Add => 1,
        RowChange::Del => 2,
        RowChange::Mod => 3,
    };

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for y in 0..h {
        // このバー行が対応する diff 表示行範囲 [lo, hi)。
        let lo = y * total / h;
        let hi = ((y + 1) * total / h).max(lo + 1).min(total);
        let mut mark = RowChange::None;
        for &c in &changes[lo.min(changes.len())..hi.min(changes.len())] {
            if rank(c) > rank(mark) {
                mark = c;
            }
        }
        let in_view = lo < view_end && hi > view_start;
        let (glyph, fg) = match mark {
            RowChange::Add => ("█", Color::Green),
            RowChange::Del => ("█", Color::Red),
            RowChange::Mod => ("█", Color::Blue),
            // 変更なし: 表示範囲内はつまみ（明るいブロック）、範囲外は空の溝。
            RowChange::None if in_view => ("█", THUMB),
            RowChange::None => (" ", Color::DarkGray),
        };
        let mut style = Style::default().fg(fg);
        if in_view {
            style = style.bg(VIEW_BG);
        }
        lines.push(Line::styled(glyph, style));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
    // blame 列を表示中なら、そのぶんカーソルを右へずらす。
    let blame_off = if app.show_blame && open.blame.is_some() {
        BLAME_W as u16
    } else {
        0
    };
    // char インデックスのカーソル列を表示セル列へ（CJK 全角でズレないように）。
    let col_cells = open
        .raw_lines
        .get(open.cursor_line)
        .map(|l| crate::width::display_col(l, open.cursor_col))
        .unwrap_or(open.cursor_col);
    let x = inner_x + gutter + blame_off + col_cells.min(u16::MAX as usize) as u16;
    let y = inner_y + (open.cursor_line - open.scroll) as u16;
    let max_x = content_area.x + content_area.width.saturating_sub(2);
    frame.set_cursor_position((x.min(max_x), y));
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    // 現在ブランチを右端のチップとして常時表示し、残りを以降の描画に使う。
    let area = render_branch_chip(frame, area, app);
    // Diff ビューではレビュー進捗チップ（✓ n/m）をその左に出す。
    let area = render_review_chip(frame, area, app);
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
                Span::styled(
                    "  g:top  d:def  r:refs  b:blame",
                    Style::default().fg(Color::DarkGray),
                ),
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
    let items: Vec<(&str, &str)> = if app.view_mode == ViewMode::Commits {
        vec![
            ("q", "quit"),
            ("Tab", "pane"),
            ("j/k", "commit/file"),
            ("] [", "Next/Prev File"),
            ("n/N", "change"),
            ("h/l", "scroll x"),
            ("s", "split"),
            ("d", "code"),
            ("c", "exit commits"),
            ("^R", "pull/reload"),
        ]
    } else if app.view_mode == ViewMode::Branch {
        vec![
            ("q", "quit"),
            ("Tab", "pane"),
            ("j/k", "file"),
            ("] [", "Next/Prev File"),
            ("n/N", "change"),
            ("h/l", "scroll x"),
            ("s", "split"),
            ("d", "code"),
            ("C", "exit PR"),
            ("^R", "pull/reload"),
        ]
    } else if app.view_mode == ViewMode::Diff {
        vec![
            ("q", "quit"),
            ("Tab", "pane"),
            ("space", "review"),
            ("] [", "Next/Prev File"),
            ("n/N", "change"),
            ("h/l", "scroll x"),
            ("s", "split"),
            ("d", "code"),
            ("c", "commits"),
            ("C", "PR diff"),
            ("^F", "Search Text"),
            ("/", find_word),
            ("^R", "pull/reload"),
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
            ("n/N", "search/change"),
            ("} {", "change"),
            ("( )", "jump back/fwd"),
            ("J", "jumps pane"),
            ("d", "diff"),
            ("c", "commits"),
            ("C", "PR diff"),
            ("^V", "branch"),
            ("m", "merge"),
            ("v/V/y", "yank"),
            ("Y", "loc"),
            ("^R", "pull/reload"),
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

/// ステータスバー右端に現在ブランチのチップを描画し、左側の残り領域を返す。
fn render_branch_chip(frame: &mut Frame, area: Rect, app: &App) -> Rect {
    let Some(branch) = app.branch_label() else {
        return area;
    };
    let text = format!(" ⎇ {branch} ");
    let w = text.chars().count() as u16;
    // 狭い端末では出さない（ヒントを優先）。
    if area.width <= w + 8 {
        return area;
    }
    let [left, chip] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            text,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )),
        chip,
    );
    left
}

/// ステータスバー右端（ブランチチップの左）にレビュー進捗チップ（`✓ n/m`）を描画し、
/// 残り領域を返す。Diff ビューで変更ファイルがあるときだけ表示する。
fn render_review_chip(frame: &mut Frame, area: Rect, app: &App) -> Rect {
    let Some((done, total)) = app.review_progress() else {
        return area;
    };
    let text = format!(" ✓ {done}/{total} ");
    let w = text.chars().count() as u16;
    if area.width <= w + 8 {
        return area; // 狭いときはヒントを優先。
    }
    let style = if done == total {
        Style::default().fg(Color::Black).bg(Color::Green)
    } else {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    };
    let [left, chip] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w)]).areas(area);
    frame.render_widget(Paragraph::new(Line::styled(text, style)), chip);
    left
}

/// 画面中央にあいまい検索オーバーレイを描画する。
/// キーマップ一覧オーバーレイ（`?`）。2 段組で現在の束縛と g プレフィックスを表示する。
fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(area, 86, 84);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green))
        .title(" Keys — ? or Esc to close ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = app.help_rows();
    let mid = rows.len().div_ceil(2);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    for (col_area, slice) in [(left, &rows[..mid]), (right, &rows[mid..])] {
        let lines: Vec<Line> = slice
            .iter()
            .map(|(keys, desc)| {
                Line::from(vec![
                    Span::styled(format!("{keys:>12}  "), Style::default().fg(Color::Cyan)),
                    Span::raw(desc.clone()),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), col_area);
    }
}

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

fn render_branches(frame: &mut Frame, area: Rect, branches: &BranchList) {
    let popup = centered_rect(area, 60, 70);
    frame.render_widget(Clear, popup);

    let (title, border) = match branches.mode {
        PickerMode::Merge => (
            format!(
                " Merge into {} — {}/{} (type to filter, Enter: merge) ",
                branches.into,
                branches.results.len(),
                branches.entries.len()
            ),
            Color::Yellow,
        ),
        PickerMode::Switch => (
            format!(
                " Switch branch — {}/{} (type to filter, Enter: checkout) ",
                branches.results.len(),
                branches.entries.len()
            ),
            Color::Magenta,
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // 先頭に絞り込みクエリ行、その下に結果リスト。
    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(branches.query.clone()),
        ])),
        query_area,
    );

    let h = list_area.height as usize;
    let max_start = branches.results.len().saturating_sub(h);
    let start = branches.selected.saturating_sub(h / 2).min(max_start);

    let rows: Vec<Line> = branches
        .results
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .filter_map(|(ri, &ei)| {
            let e = branches.entries.get(ei)?;
            let selected = ri == branches.selected;
            // 先頭マーカー: 現在ブランチ `*`、それ以外は空白。
            let marker = if e.is_head { "* " } else { "  " };
            let name_style = if selected {
                Style::default().fg(Color::Black).bg(Color::Magenta)
            } else if e.is_head {
                Style::default().fg(Color::Green)
            } else if e.is_remote {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };
            Some(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Green)),
                Span::styled(e.display.clone(), name_style),
            ]))
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);
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
    // char 単位でスキャンする。小文字化で char 数が変わりうる文字（例: 'İ'）では
    // text と lower の char 添字がずれうるが、コードのプレビューは実質 ASCII なので
    // ここでは素朴に char 境界で対応付ける。
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

    #[test]
    fn renders_help_overlay() {
        let mut app = App::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        app.show_help = true;
        let text = render_to_string(&mut app);
        assert!(text.contains("Keys"), "help overlay title missing");
        assert!(text.contains("Quit"), "help overlay action missing");
        assert!(text.contains("gd"), "help overlay g-prefix missing");
    }
}

//! git の差分行を ratatui の表示行へ組み立てる。
//!
//! 追加・文脈行はフルファイルのハイライト済み行を再利用して配色を一致させ、
//! 削除行のみ単体でハイライトする。

use std::ops::Range;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::git::{DiffKind, DiffLine};
use crate::highlight::{CodeHighlighter, Syntax};

// 行全体の地色（変更行のうち「変わっていない文字」の背景）。
const ADD_BG: Color = Color::Rgb(28, 58, 38);
const DEL_BG: Color = Color::Rgb(64, 32, 36);
// 行内で実際に変わった文字を強調する背景（地色より一段明るい）。
const ADD_EMPH_BG: Color = Color::Rgb(46, 102, 58);
const DEL_EMPH_BG: Color = Color::Rgb(120, 50, 56);

pub struct DiffRender {
    /// 表示用の行（unified）。
    pub rows: Vec<Line<'static>>,
    /// 各行が対応するフルコードの行インデックス（0 始まり）。トグル時の行保持に使う。
    pub to_code: Vec<Option<usize>>,
    /// unified 各表示行の変更種別（オーバービュー・バー用、`rows` と同長）。
    row_changes: Vec<RowChange>,
    /// unified の変更ブロック先頭行（`n`/`N` ジャンプ用）。全行表示で `@@` が 1 つに
    /// 集約されても、変更の塊ごとに辿れる。
    change_anchors: Vec<usize>,
    /// 新規/削除ファイル（文脈なし・片側のみ）。split 既定でも単一表示にする。
    pub single_column: bool,
    /// side-by-side は遅延構築（実際に左右表示するまで作らない）。
    split: Option<Vec<SplitRow>>,
    split_row_changes: Option<Vec<RowChange>>,
    split_change_anchors: Option<Vec<usize>>,
    /// 遅延構築のために元データを保持。
    raw: Vec<DiffLine>,
    syntax: Syntax,
}

/// side-by-side の 1 行（左右）。
pub struct SplitRow {
    pub left: Line<'static>,
    pub right: Line<'static>,
}

/// 差分の表示行 1 行ぶんの変更種別（右端オーバービュー・バーの色付け用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowChange {
    None,
    Add,
    Del,
    /// 削除と追加が対になった行（side-by-side の左右が埋まる行）。
    Mod,
}

/// 変更行の連なり（ブロック）の先頭インデックス列を求める（`n`/`N` ジャンプ用）。
fn anchors_from(changes: &[RowChange]) -> Vec<usize> {
    let mut anchors = Vec::new();
    let mut prev_changed = false;
    for (i, c) in changes.iter().enumerate() {
        let changed = *c != RowChange::None;
        if changed && !prev_changed {
            anchors.push(i);
        }
        prev_changed = changed;
    }
    anchors
}

/// コードビューの gutter に出す行ごとの変更印（エディタ風）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMark {
    None,
    Added,
    Modified,
    /// この行の直前で行が削除された。
    DeletedAbove,
}

impl DiffRender {
    /// 新ファイル各行の変更印を返す（コードビューの gutter 用）。
    /// `total` は新ファイルの行数。
    pub fn line_marks(&self, total: usize) -> Vec<LineMark> {
        let mut marks = vec![LineMark::None; total];
        let mut pending_del = 0usize; // まだ追加と対にしていない削除数
        let set = |marks: &mut [LineMark], lineno: Option<u32>, m: LineMark| {
            if let Some(n) = lineno {
                let idx = (n as usize).saturating_sub(1);
                if idx < marks.len() && marks[idx] == LineMark::None {
                    marks[idx] = m;
                }
            }
        };
        for dl in &self.raw {
            match dl.kind {
                DiffKind::Hunk => pending_del = 0,
                DiffKind::Del => pending_del += 1,
                DiffKind::Add => {
                    let m = if pending_del > 0 {
                        LineMark::Modified // 削除を伴う追加＝変更
                    } else {
                        LineMark::Added
                    };
                    if let Some(n) = dl.new_lineno {
                        let idx = (n as usize).saturating_sub(1);
                        if idx < marks.len() {
                            marks[idx] = m;
                        }
                    }
                    pending_del = pending_del.saturating_sub(1);
                }
                DiffKind::Context => {
                    if pending_del > 0 {
                        set(&mut marks, dl.new_lineno, LineMark::DeletedAbove);
                        pending_del = 0;
                    }
                }
            }
        }
        // EOF での純削除は最終行に印を付ける。
        if pending_del > 0 && total > 0 && marks[total - 1] == LineMark::None {
            marks[total - 1] = LineMark::DeletedAbove;
        }
        marks
    }

    /// side-by-side 行を必要時に構築してキャッシュする。
    pub fn ensure_split(
        &mut self,
        code_lines: &[Line<'static>],
        highlighter: &mut CodeHighlighter,
    ) {
        if self.split.is_some() {
            return;
        }
        let (split, changes) = build_split(&self.raw, code_lines, highlighter, self.syntax);
        self.split_change_anchors = Some(anchors_from(&changes));
        self.split_row_changes = Some(changes);
        self.split = Some(split);
    }

    /// 構築済みなら side-by-side 行を返す。
    pub fn split_rows(&self) -> Option<&[SplitRow]> {
        self.split.as_deref()
    }

    /// 表示中の表現（split/unified）の行数。
    pub fn row_count(&self, split: bool) -> usize {
        if split {
            self.split.as_ref().map_or(0, |s| s.len())
        } else {
            self.rows.len()
        }
    }

    /// 表示中の表現の変更ブロック先頭行インデックス（`n`/`N` ジャンプ用）。
    pub fn change_anchors_for(&self, split: bool) -> &[usize] {
        if split {
            self.split_change_anchors.as_deref().unwrap_or(&[])
        } else {
            &self.change_anchors
        }
    }

    /// 表示中の表現の各行の変更種別（オーバービュー・バー用）。
    pub fn row_changes_for(&self, split: bool) -> &[RowChange] {
        if split {
            self.split_row_changes.as_deref().unwrap_or(&[])
        } else {
            &self.row_changes
        }
    }
}

/// 差分行・ハイライト済みコード行から表示行を組み立てる（unified のみ。split は遅延）。
pub fn build(
    diff: &[DiffLine],
    code_lines: &[Line<'static>],
    highlighter: &mut CodeHighlighter,
    syntax: Syntax,
) -> DiffRender {
    let mut rows = Vec::with_capacity(diff.len());
    let mut to_code = Vec::with_capacity(diff.len());
    let mut row_changes = Vec::with_capacity(diff.len());
    // 連続する削除/追加を溜め、文脈/見出し/末尾でまとめて流す（行内強調のため対にする）。
    let mut pdel: Vec<&DiffLine> = Vec::new();
    let mut padd: Vec<&DiffLine> = Vec::new();

    for dl in diff {
        match dl.kind {
            DiffKind::Del => pdel.push(dl),
            DiffKind::Add => padd.push(dl),
            DiffKind::Hunk => {
                flush_unified(
                    &mut rows, &mut to_code, &mut row_changes, &mut pdel, &mut padd,
                    code_lines, highlighter, syntax,
                );
                rows.push(Line::styled(dl.content.clone(), Style::default().fg(Color::Cyan)));
                to_code.push(None);
                row_changes.push(RowChange::None);
            }
            DiffKind::Context => {
                flush_unified(
                    &mut rows, &mut to_code, &mut row_changes, &mut pdel, &mut padd,
                    code_lines, highlighter, syntax,
                );
                let code_idx = dl.new_lineno.map(|n| (n as usize).saturating_sub(1));
                rows.push(side_line(dl.new_lineno, ' ', None, code_line_for(dl.new_lineno, code_lines)));
                to_code.push(code_idx);
                row_changes.push(RowChange::None);
            }
        }
    }
    flush_unified(
        &mut rows, &mut to_code, &mut row_changes, &mut pdel, &mut padd,
        code_lines, highlighter, syntax,
    );

    let change_anchors = anchors_from(&row_changes);
    DiffRender {
        rows,
        to_code,
        row_changes,
        change_anchors,
        single_column: is_whole_file_change(diff),
        split: None,
        split_row_changes: None,
        split_change_anchors: None,
        raw: diff.to_vec(),
        syntax,
    }
}

/// 溜まった削除/追加を unified の行へ流す。削除→追加の順に出し、対になる行は
/// 行内で実際に変わった文字だけを強調する（共通接頭辞/接尾辞を除いた範囲）。
#[allow(clippy::too_many_arguments)]
fn flush_unified(
    rows: &mut Vec<Line<'static>>,
    to_code: &mut Vec<Option<usize>>,
    row_changes: &mut Vec<RowChange>,
    pdel: &mut Vec<&DiffLine>,
    padd: &mut Vec<&DiffLine>,
    code_lines: &[Line<'static>],
    highlighter: &mut CodeHighlighter,
    syntax: Syntax,
) {
    // 対になる削除/追加行は変わった文字範囲を一度だけ求め、両側で使い回す。
    // 対応行が無い行は range が空になり、行内強調なし（地色のみ）で描画される。
    let ranges = paired_ranges(pdel, padd);

    for (i, dl) in pdel.iter().enumerate() {
        let hl = highlighter.highlight(syntax, &dl.content);
        let body = hl.first().map(|l| l.spans.as_slice()).unwrap_or(&[]);
        rows.push(change_row('-', dl.old_lineno, DEL_BG, DEL_EMPH_BG, body, &ranges[i].0));
        to_code.push(None);
        row_changes.push(RowChange::Del);
    }
    for (i, dl) in padd.iter().enumerate() {
        let code_idx = dl.new_lineno.map(|n| (n as usize).saturating_sub(1));
        let body: Vec<Span<'static>> = code_line_for(dl.new_lineno, code_lines)
            .map(|l| l.spans.clone())
            .unwrap_or_default();
        rows.push(change_row('+', dl.new_lineno, ADD_BG, ADD_EMPH_BG, &body, &ranges[i].1));
        to_code.push(code_idx);
        row_changes.push(RowChange::Add);
    }
    pdel.clear();
    padd.clear();
}

/// 2 行の「変わった文字」の char 範囲を、共通接頭辞/接尾辞を除いて求める。
/// 返り値 `(old 側, new 側)`。完全一致なら両方 `pre..pre`（空）。
fn changed_ranges(old: &str, new: &str) -> (Range<usize>, Range<usize>) {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    let mut pre = 0;
    while pre < o.len() && pre < n.len() && o[pre] == n[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < o.len() - pre && suf < n.len() - pre && o[o.len() - 1 - suf] == n[n.len() - 1 - suf]
    {
        suf += 1;
    }
    (pre..o.len() - suf, pre..n.len() - suf)
}

/// 溜まった削除/追加を位置で対にし、各ペアの変更範囲 `(old 側, new 側)` を求める。
/// 長さは `max(pdel, padd)`。片側しか無い行は空範囲（行内強調なし）。
fn paired_ranges(pdel: &[&DiffLine], padd: &[&DiffLine]) -> Vec<(Range<usize>, Range<usize>)> {
    (0..pdel.len().max(padd.len()))
        .map(|i| match (pdel.get(i), padd.get(i)) {
            (Some(d), Some(a)) => changed_ranges(&d.content, &a.content),
            _ => (0..0, 0..0),
        })
        .collect()
}

/// 変更行（add/del）1 行ぶんを組み立てる。gutter + 行内強調済み本文。
fn change_row(
    sign: char,
    lineno: Option<u32>,
    base_bg: Color,
    emph_bg: Color,
    body: &[Span<'static>],
    range: &Range<usize>,
) -> Line<'static> {
    let mut spans = vec![gutter(sign, lineno, Some(base_bg))];
    spans.extend(emphasized(body, base_bg, emph_bg, range));
    styled_line(spans, Some(base_bg))
}

/// ハイライト済み spans に背景を付ける。`range`（char 単位）内は強調 bg、その他は地色 bg。
/// 文字境界で span を分割するので、シンタックスの前景色は維持される。
fn emphasized(
    src: &[Span<'static>],
    base_bg: Color,
    emph_bg: Color,
    range: &Range<usize>,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut col = 0usize; // 行頭からの char index
    for s in src {
        let chars: Vec<char> = s.content.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let inside = range.contains(&(col + i));
            let start = i;
            while i < chars.len() && range.contains(&(col + i)) == inside {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let bg = if inside { emph_bg } else { base_bg };
            out.push(Span::styled(text, s.style.bg(bg)));
        }
        col += chars.len();
    }
    out
}

/// 新規ファイル（追加のみ）/ 削除ファイル（削除のみ）か。
/// どちらも文脈行が無く片側だけなので side-by-side では片側が空になる。
fn is_whole_file_change(diff: &[DiffLine]) -> bool {
    let (mut add, mut del, mut ctx) = (false, false, false);
    for dl in diff {
        match dl.kind {
            DiffKind::Add => add = true,
            DiffKind::Del => del = true,
            DiffKind::Context => ctx = true,
            DiffKind::Hunk => {}
        }
    }
    !ctx && (add ^ del)
}

/// side-by-side 行を組み立てる。削除/追加の連続ブロックを左右に並べ、
/// 数が合わない分は空行で埋める。返り値は (split 行, 各行の変更種別)。
fn build_split(
    diff: &[DiffLine],
    code_lines: &[Line<'static>],
    highlighter: &mut CodeHighlighter,
    syntax: Syntax,
) -> (Vec<SplitRow>, Vec<RowChange>) {
    let mut split: Vec<SplitRow> = Vec::new();
    let mut changes: Vec<RowChange> = Vec::new();
    let mut pdel: Vec<&DiffLine> = Vec::new();
    let mut padd: Vec<&DiffLine> = Vec::new();

    for dl in diff {
        match dl.kind {
            DiffKind::Del => pdel.push(dl),
            DiffKind::Add => padd.push(dl),
            DiffKind::Hunk => {
                drain_changes(&mut split, &mut changes, &mut pdel, &mut padd, code_lines, highlighter, syntax);
                split.push(SplitRow {
                    left: Line::styled(dl.content.clone(), Style::default().fg(Color::Cyan)),
                    right: Line::from(""),
                });
                changes.push(RowChange::None);
            }
            DiffKind::Context => {
                drain_changes(&mut split, &mut changes, &mut pdel, &mut padd, code_lines, highlighter, syntax);
                let content = code_line_for(dl.new_lineno, code_lines);
                split.push(SplitRow {
                    left: side_line(dl.old_lineno, ' ', None, content),
                    right: side_line(dl.new_lineno, ' ', None, content),
                });
                changes.push(RowChange::None);
            }
        }
    }
    drain_changes(&mut split, &mut changes, &mut pdel, &mut padd, code_lines, highlighter, syntax);
    (split, changes)
}

/// 溜まった削除/追加を左右ペアにして split へ流し込む。各行の変更種別も併記する。
fn drain_changes(
    split: &mut Vec<SplitRow>,
    changes: &mut Vec<RowChange>,
    pdel: &mut Vec<&DiffLine>,
    padd: &mut Vec<&DiffLine>,
    code_lines: &[Line<'static>],
    highlighter: &mut CodeHighlighter,
    syntax: Syntax,
) {
    // 左右が対になる行は変わった文字範囲を一度だけ求め、両側で強調する。
    let ranges = paired_ranges(pdel, padd);
    for (i, (del_range, add_range)) in ranges.iter().enumerate() {
        let left = match pdel.get(i) {
            Some(dl) => {
                let hl = highlighter.highlight(syntax, &dl.content);
                let body = hl.first().map(|l| l.spans.as_slice()).unwrap_or(&[]);
                change_row('-', dl.old_lineno, DEL_BG, DEL_EMPH_BG, body, del_range)
            }
            None => Line::from(""),
        };
        let right = match padd.get(i) {
            Some(dl) => {
                let body: Vec<Span<'static>> = code_line_for(dl.new_lineno, code_lines)
                    .map(|l| l.spans.clone())
                    .unwrap_or_default();
                change_row('+', dl.new_lineno, ADD_BG, ADD_EMPH_BG, &body, add_range)
            }
            None => Line::from(""),
        };
        // 左右の埋まり方で種別を決める（両方=変更 / 右のみ=追加 / 左のみ=削除）。
        changes.push(match (pdel.get(i).is_some(), padd.get(i).is_some()) {
            (true, true) => RowChange::Mod,
            (false, true) => RowChange::Add,
            _ => RowChange::Del,
        });
        split.push(SplitRow { left, right });
    }
    pdel.clear();
    padd.clear();
}

fn code_line_for<'a>(lineno: Option<u32>, code_lines: &'a [Line<'static>]) -> Option<&'a Line<'static>> {
    lineno.and_then(|n| code_lines.get((n as usize).saturating_sub(1)))
}

/// 片側 1 行（gutter + 内容）。文脈/追加に使う。
fn side_line(
    lineno: Option<u32>,
    sign: char,
    bg: Option<Color>,
    content: Option<&Line<'static>>,
) -> Line<'static> {
    let mut spans = vec![gutter(sign, lineno, bg)];
    if let Some(cl) = content {
        for s in &cl.spans {
            let style = match bg {
                Some(bg) => s.style.bg(bg),
                None => s.style,
            };
            spans.push(Span::styled(s.content.clone(), style));
        }
    }
    styled_line(spans, bg)
}

fn gutter(sign: char, lineno: Option<u32>, bg: Option<Color>) -> Span<'static> {
    let n = lineno.map(|n| n.to_string()).unwrap_or_default();
    let mut style = Style::default().fg(Color::DarkGray);
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    Span::styled(format!("{n:>4}{sign} "), style)
}

fn styled_line(spans: Vec<Span<'static>>, bg: Option<Color>) -> Line<'static> {
    let line = Line::from(spans);
    match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(kind: DiffKind, old: Option<u32>, new: Option<u32>, content: &str) -> DiffLine {
        DiffLine {
            kind,
            old_lineno: old,
            new_lineno: new,
            content: content.to_string(),
        }
    }

    #[test]
    fn split_pairs_changes_and_marks_block() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code_lines = vec![Line::from("ctx"), Line::from("new1")];
        let diff = vec![
            dl(DiffKind::Hunk, None, None, "@@ -1,2 +1,2 @@"),
            dl(DiffKind::Context, Some(1), Some(1), "ctx"),
            dl(DiffKind::Del, Some(2), None, "old"),
            dl(DiffKind::Add, None, Some(2), "new1"),
        ];
        let (split, changes) = build_split(&diff, &code_lines, &mut h, plain);
        // hunk(1) + context(1) + del/add ペア(1) = 3 行
        assert_eq!(split.len(), 3, "split rows");
        // 先頭2行は変更なし、3行目が削除を伴う追加＝変更。
        assert_eq!(changes, vec![RowChange::None, RowChange::None, RowChange::Mod]);
        // 変更ブロックの先頭は 3 行目（index 2）。
        assert_eq!(anchors_from(&changes), vec![2]);
    }

    #[test]
    fn split_is_lazy_until_ensured() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code = vec![Line::from("ctx"), Line::from("new1")];
        let diff = vec![
            dl(DiffKind::Context, Some(1), Some(1), "ctx"),
            dl(DiffKind::Del, Some(2), None, "old"),
            dl(DiffKind::Add, None, Some(2), "new1"),
        ];
        let mut r = build(&diff, &code, &mut h, plain);
        // build() では split を作らない。
        assert!(r.split_rows().is_none(), "split must be lazy");
        assert_eq!(r.row_count(true), 0);
        // 必要時に構築される。
        r.ensure_split(&code, &mut h);
        assert!(r.split_rows().is_some());
        assert_eq!(r.row_count(true), r.split_rows().unwrap().len());
    }

    #[test]
    fn line_marks_classify_add_modify_delete() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code = vec![
            Line::from("ctx"),
            Line::from("added"),
            Line::from("modified"),
            Line::from("ctx2"),
        ];
        let diff = vec![
            dl(DiffKind::Context, Some(1), Some(1), "ctx"),
            dl(DiffKind::Add, None, Some(2), "added"), // 純追加
            dl(DiffKind::Del, Some(2), None, "old"),
            dl(DiffKind::Add, None, Some(3), "modified"), // 削除を伴う追加=変更
            dl(DiffKind::Del, Some(3), None, "removed"),  // 純削除（4行目の上）
            dl(DiffKind::Context, Some(4), Some(4), "ctx2"),
        ];
        let r = build(&diff, &code, &mut h, plain);
        let marks = r.line_marks(4);
        assert_eq!(marks[0], LineMark::None);
        assert_eq!(marks[1], LineMark::Added);
        assert_eq!(marks[2], LineMark::Modified);
        assert_eq!(marks[3], LineMark::DeletedAbove);
    }

    #[test]
    fn single_column_for_new_and_deleted_files() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code = vec![Line::from("x"), Line::from("y")];
        // 新規ファイル: 追加のみ・文脈なし
        let new_file = vec![
            dl(DiffKind::Hunk, None, None, "@@ -0,0 +1,2 @@"),
            dl(DiffKind::Add, None, Some(1), "x"),
            dl(DiffKind::Add, None, Some(2), "y"),
        ];
        assert!(build(&new_file, &code, &mut h, plain).single_column);
        // 削除ファイル: 削除のみ
        let del_file = vec![dl(DiffKind::Del, Some(1), None, "x")];
        assert!(build(&del_file, &code, &mut h, plain).single_column);
        // 変更: 文脈あり → 単一にしない
        let modified = vec![
            dl(DiffKind::Context, Some(1), Some(1), "x"),
            dl(DiffKind::Del, Some(2), None, "old"),
            dl(DiffKind::Add, None, Some(2), "y"),
        ];
        assert!(!build(&modified, &code, &mut h, plain).single_column);
    }

    #[test]
    fn split_pads_unequal_del_add() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code_lines = vec![Line::from("a"), Line::from("b")];
        // 削除2 / 追加1 → max=2 行に揃う（不足側は空行）
        let diff = vec![
            dl(DiffKind::Del, Some(1), None, "d1"),
            dl(DiffKind::Del, Some(2), None, "d2"),
            dl(DiffKind::Add, None, Some(1), "a"),
        ];
        let (split, _) = build_split(&diff, &code_lines, &mut h, plain);
        assert_eq!(split.len(), 2, "padded to max(del,add)");
    }

    #[test]
    fn unified_row_changes_and_anchors_two_blocks() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code = vec![Line::from("a"), Line::from("b"), Line::from("c"), Line::from("d")];
        // ctx / 追加(塊1) / ctx / 削除(塊2) / ctx
        let diff = vec![
            dl(DiffKind::Context, Some(1), Some(1), "a"),
            dl(DiffKind::Add, None, Some(2), "b"),
            dl(DiffKind::Context, Some(2), Some(3), "c"),
            dl(DiffKind::Del, Some(3), None, "x"),
            dl(DiffKind::Context, Some(4), Some(4), "d"),
        ];
        let r = build(&diff, &code, &mut h, plain);
        assert_eq!(
            r.row_changes_for(false),
            &[
                RowChange::None,
                RowChange::Add,
                RowChange::None,
                RowChange::Del,
                RowChange::None
            ]
        );
        // 変更ブロックは 2 つ（index 1 と 3）。
        assert_eq!(r.change_anchors_for(false), &[1, 3]);
    }

    #[test]
    fn changed_ranges_trims_common_prefix_and_suffix() {
        // "let a = 1;" → "let a = 2;" は 9 文字目（index 8）だけ変わる。
        assert_eq!(changed_ranges("let a = 1;", "let a = 2;"), (8..9, 8..9));
        // 完全一致は空範囲。
        assert_eq!(changed_ranges("same", "same"), (4..4, 4..4));
        // 末尾追記は old 側が空、new 側だけ範囲を持つ。
        assert_eq!(changed_ranges("foo", "foobar"), (3..3, 3..6));
    }

    #[test]
    fn intraline_emphasis_marks_only_changed_chars() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        // 追加側の新内容（new_lineno=1 → code_lines[0]）。
        let code = vec![Line::from("let a = 2;")];
        let diff = vec![
            dl(DiffKind::Del, Some(1), None, "let a = 1;"),
            dl(DiffKind::Add, None, Some(1), "let a = 2;"),
        ];
        let (split, _) = build_split(&diff, &code, &mut h, plain);
        let row = &split[0];
        // 変わった文字（"1"/"2"）だけが強調 bg、変わらない部分は地色 bg。
        assert!(
            row.right.spans.iter().any(|s| s.style.bg == Some(ADD_EMPH_BG)),
            "changed char emphasized on add side"
        );
        assert!(
            row.right.spans.iter().any(|s| s.style.bg == Some(ADD_BG)),
            "unchanged part keeps the base bg"
        );
        assert!(
            row.left.spans.iter().any(|s| s.style.bg == Some(DEL_EMPH_BG)),
            "changed char emphasized on del side"
        );
        // 強調されるのは 1 文字分だけ（"2" の span がちょうど "2"）。
        let emph: String = row
            .right
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(ADD_EMPH_BG))
            .map(|s| s.content.clone())
            .collect();
        assert_eq!(emph, "2");
    }

    #[test]
    fn unified_emphasizes_changed_chars_for_paired_lines() {
        let mut h = CodeHighlighter::new();
        let plain = h.plain();
        let code = vec![Line::from("value = 99;")];
        let diff = vec![
            dl(DiffKind::Del, Some(1), None, "value = 10;"),
            dl(DiffKind::Add, None, Some(1), "value = 99;"),
        ];
        let r = build(&diff, &code, &mut h, plain);
        // unified でも対になる行は強調 bg を持つ（del 行と add 行の両方）。
        let any_bg = |line: &Line<'static>, c: Color| line.spans.iter().any(|s| s.style.bg == Some(c));
        assert!(r.rows.iter().any(|l| any_bg(l, DEL_EMPH_BG)), "del line emphasized");
        assert!(r.rows.iter().any(|l| any_bg(l, ADD_EMPH_BG)), "add line emphasized");
    }
}

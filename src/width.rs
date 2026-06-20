//! 表示幅（端末セル数）ユーティリティ。CJK 全角や絵文字は 2 セル幅になるため、
//! 「char 数」ではなく「表示セル数」で桁を扱いたい箇所で使う（カーソル位置・blame 列など）。

use unicode_width::UnicodeWidthChar;

/// 文字列の表示セル幅。制御文字は 0、全角・絵文字は 2 として数える。
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// 行頭から char インデックス `char_idx` までの表示セル列を返す（カーソル位置算出用）。
/// CJK 全角は 2 セル。タブ・制御文字は端末依存のため 1 セルとして扱う（厳密なタブストップは未対応）。
pub fn display_col(line: &str, char_idx: usize) -> usize {
    line.chars()
        .take(char_idx)
        .map(|c| c.width().unwrap_or(1))
        .sum()
}

/// `s` を表示幅 `width` ちょうどに整える。長ければ表示幅で切り詰め、短ければ空白で右詰めする。
/// 全角文字が境界を跨ぐ場合はその文字を落とすため、結果は `width` 以下にはならず常に `width`。
pub fn pad_display(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    for _ in w..width {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_counts_fullwidth_and_tabs() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("あい"), 4); // 全角 2 つ = 4 セル
        assert_eq!(display_width("a あ b"), 1 + 1 + 2 + 1 + 1);
    }

    #[test]
    fn display_col_maps_char_index_to_cells() {
        assert_eq!(display_col("abc", 2), 2);
        assert_eq!(display_col("あいu", 1), 2); // 全角 1 つ = 2 セル
        assert_eq!(display_col("あいu", 2), 4); // 全角 2 つ = 4 セル
        assert_eq!(display_col("あいu", 3), 5); // +ASCII 1
        assert_eq!(display_col("ab", 9), 2); // 行長超過でも panic しない
    }

    #[test]
    fn pad_display_truncates_and_pads_by_cells() {
        assert_eq!(pad_display("abc", 5), "abc  "); // 右に空白 2
        assert_eq!(pad_display("abcdef", 4), "abcd"); // 切り詰め
        // 全角は 2 セル。幅 3 には「あ」(2) のみ入り、残り 1 を空白埋め。
        assert_eq!(pad_display("ああ", 3), "あ ");
        assert_eq!(display_width(&pad_display("ああ", 3)), 3);
    }
}

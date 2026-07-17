//! snapshot と現在それぞれのレンダリング済み行を突き合わせ、両層を同じ行数に
//! 整列した [`DiffLayers`] を作る。行の対応は plain text の Patience diff で取り、
//! 置換ブロック内は入力順に行をペアにして word 単位の変更 byte range を求める。
//! 片側にしかない行の位置には相手層へ Filler 行を入れ、共通行が両層で同じ index
//! (= 同じ画面位置) に並ぶことを保証する。色と描画はこの層では持たず、
//! [`RowKind`] と byte range だけを返す。

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};
use unicode_segmentation::UnicodeSegmentation;

use crate::style::StyledLine;

/// 整列済みの両層。`old_rows` と `new_rows` は常に同じ長さで、同じ index が
/// 画面上の同じ行位置に対応する。
pub(crate) struct DiffLayers {
    pub(crate) old_rows: Vec<DiffRow>,
    pub(crate) new_rows: Vec<DiffRow>,
}

pub(crate) struct DiffRow {
    /// 表示する行。Filler では空行。
    pub(crate) line: StyledLine,
    pub(crate) kind: RowKind,
}

pub(crate) enum RowKind {
    /// 両層で plain text が一致した行。
    Common,
    /// この層で変わった行。`emphasis` は plain text への変更 byte range で、
    /// 相手層と行ペアが取れない追加・削除行では空になり行全体が変更を表す。
    Changed { emphasis: Vec<Range<usize>> },
    /// 相手層にだけ行がある位置を埋める行。
    Filler,
}

/// 旧層 (snapshot 側) と新層 (現在) のレンダリング済み行から整列結果を作る。
///
/// 両入力は同じ表示幅でレンダリングされている前提。行の同一性は plain text
/// のみで判定するため、スタイルだけが違う行は Common になる。
pub(crate) fn compute(old_lines: &[StyledLine], new_lines: &[StyledLine]) -> DiffLayers {
    let old_texts: Vec<String> = old_lines.iter().map(StyledLine::plain_text).collect();
    let new_texts: Vec<String> = new_lines.iter().map(StyledLine::plain_text).collect();
    let operations = capture_diff_slices(Algorithm::Patience, &old_texts, &new_texts);

    let mut layers = DiffLayers {
        old_rows: Vec::new(),
        new_rows: Vec::new(),
    };

    for operation in operations {
        match operation {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for offset in 0..len {
                    layers.push(
                        DiffRow {
                            line: old_lines[old_index + offset].clone(),
                            kind: RowKind::Common,
                        },
                        DiffRow {
                            line: new_lines[new_index + offset].clone(),
                            kind: RowKind::Common,
                        },
                    );
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => layers.push_removed(&old_lines[old_index..old_index + old_len]),
            DiffOp::Insert {
                new_index, new_len, ..
            } => layers.push_added(&new_lines[new_index..new_index + new_len]),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                // 置換ブロック内は入力順に行をペアへ。余った行は片側だけの
                // 追加・削除として扱う。
                let paired = old_len.min(new_len);
                for offset in 0..paired {
                    let (old_emphasis, new_emphasis) = intraline(
                        &old_texts[old_index + offset],
                        &new_texts[new_index + offset],
                    );
                    layers.push(
                        DiffRow {
                            line: old_lines[old_index + offset].clone(),
                            kind: RowKind::Changed {
                                emphasis: old_emphasis,
                            },
                        },
                        DiffRow {
                            line: new_lines[new_index + offset].clone(),
                            kind: RowKind::Changed {
                                emphasis: new_emphasis,
                            },
                        },
                    );
                }
                layers.push_removed(&old_lines[old_index + paired..old_index + old_len]);
                layers.push_added(&new_lines[new_index + paired..new_index + new_len]);
            }
        }
    }

    layers
}

impl DiffLayers {
    fn push(&mut self, old: DiffRow, new: DiffRow) {
        self.old_rows.push(old);
        self.new_rows.push(new);
    }

    fn push_removed(&mut self, lines: &[StyledLine]) {
        for line in lines {
            self.push(
                DiffRow {
                    line: line.clone(),
                    kind: RowKind::Changed {
                        emphasis: Vec::new(),
                    },
                },
                DiffRow {
                    line: StyledLine::empty(),
                    kind: RowKind::Filler,
                },
            );
        }
    }

    fn push_added(&mut self, lines: &[StyledLine]) {
        for line in lines {
            self.push(
                DiffRow {
                    line: StyledLine::empty(),
                    kind: RowKind::Filler,
                },
                DiffRow {
                    line: line.clone(),
                    kind: RowKind::Changed {
                        emphasis: Vec::new(),
                    },
                },
            );
        }
    }
}

/// 行ペアの word 単位 diff。(旧, 新) それぞれの plain text への変更 byte range
/// を返す。隣接する range は結合済み。
fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }

    let (old_units, old_ranges) = diff_units(old);
    let (new_units, new_ranges) = diff_units(new);
    let operations = capture_diff_slices(Algorithm::Myers, &old_units, &new_units);
    let mut old_changed = Vec::new();
    let mut new_changed = Vec::new();

    for operation in operations {
        match operation {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => push_unit_span(&mut old_changed, &old_ranges, old_index, old_len),
            DiffOp::Insert {
                new_index, new_len, ..
            } => push_unit_span(&mut new_changed, &new_ranges, new_index, new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                push_unit_span(&mut old_changed, &old_ranges, old_index, old_len);
                push_unit_span(&mut new_changed, &new_ranges, new_index, new_len);
            }
        }
    }

    (merge_adjacent(old_changed), merge_adjacent(new_changed))
}

fn push_unit_span(
    output: &mut Vec<Range<usize>>,
    ranges: &[Range<usize>],
    index: usize,
    len: usize,
) {
    if len == 0 {
        return;
    }
    output.push(ranges[index].start..ranges[index + len - 1].end);
}

fn merge_adjacent(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && last.end == range.start
        {
            last.end = range.end;
            continue;
        }
        merged.push(range);
    }
    merged
}

/// diff の比較単位。同じ [`WordClass`] の書記素の連続を1単位にまとめ、それ以外
/// (空白・記号・他 script) は書記素1つを1単位とする。一語の中の1文字だけの
/// 変更を語全体の変更として拾うための粒度で、単位の text と元 text への
/// byte range を並行した Vec で返す。
fn diff_units(text: &str) -> (Vec<&str>, Vec<Range<usize>>) {
    let mut units = Vec::new();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut pending: Option<(WordClass, Range<usize>)> = None;

    for (start, grapheme) in text.grapheme_indices(true) {
        let end = start + grapheme.len();
        match word_class(grapheme) {
            Some(class) => match &mut pending {
                Some((pending_class, range)) if *pending_class == class => range.end = end,
                _ => {
                    flush_pending(&mut pending, text, &mut units, &mut ranges);
                    pending = Some((class, start..end));
                }
            },
            None => {
                flush_pending(&mut pending, text, &mut units, &mut ranges);
                units.push(grapheme);
                ranges.push(start..end);
            }
        }
    }
    flush_pending(&mut pending, text, &mut units, &mut ranges);

    (units, ranges)
}

fn flush_pending<'a>(
    pending: &mut Option<(WordClass, Range<usize>)>,
    text: &'a str,
    units: &mut Vec<&'a str>,
    ranges: &mut Vec<Range<usize>>,
) {
    if let Some((_, range)) = pending.take() {
        units.push(&text[range.clone()]);
        ranges.push(range);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordClass {
    AsciiIdentifier,
    Hiragana,
    Katakana,
    Han,
}

/// 書記素の先頭 scalar で単位の種類を決める。None は単独単位を意味する。
/// 長音符 (U+30FC) は Katakana 範囲なので、ひらがな中の「ー」は run を切る。
fn word_class(grapheme: &str) -> Option<WordClass> {
    let ch = grapheme.chars().next()?;
    match ch {
        'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => Some(WordClass::AsciiIdentifier),
        '\u{3041}'..='\u{309F}' => Some(WordClass::Hiragana),
        '\u{30A0}'..='\u{30FF}' => Some(WordClass::Katakana),
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' => Some(WordClass::Han),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[&str]) -> Vec<StyledLine> {
        lines.iter().map(|line| StyledLine::plain(*line)).collect()
    }

    #[test]
    fn aligns_layers_with_fillers() {
        let old = plain(&["a", "b", "c"]);
        let new = plain(&["a", "c", "d"]);

        let layers = compute(&old, &new);

        assert_eq!(layers.old_rows.len(), 4);
        assert_eq!(layers.new_rows.len(), 4);
        assert!(matches!(layers.old_rows[0].kind, RowKind::Common));
        assert!(matches!(layers.old_rows[1].kind, RowKind::Changed { .. }));
        assert!(matches!(layers.new_rows[1].kind, RowKind::Filler));
        assert!(matches!(layers.old_rows[2].kind, RowKind::Common));
        assert!(matches!(layers.old_rows[3].kind, RowKind::Filler));
        assert!(matches!(layers.new_rows[3].kind, RowKind::Changed { .. }));
        assert_eq!(layers.old_rows[1].line.plain_text(), "b");
        assert_eq!(layers.new_rows[1].line.plain_text(), "");
        assert_eq!(layers.new_rows[3].line.plain_text(), "d");
    }

    #[test]
    fn pairs_replaced_lines_with_word_ranges() {
        let old = plain(&["りんごは赤い"]);
        let new = plain(&["りんごは青い"]);

        let layers = compute(&old, &new);

        assert_eq!(layers.old_rows.len(), 1);
        let RowKind::Changed { emphasis } = &layers.old_rows[0].kind else {
            panic!("old row should be Changed");
        };
        assert_eq!(emphasis.len(), 1);
        assert_eq!(emphasis[0], 12..15);
        let RowKind::Changed { emphasis } = &layers.new_rows[0].kind else {
            panic!("new row should be Changed");
        };
        assert_eq!(emphasis.len(), 1);
        assert_eq!(emphasis[0], 12..15);
    }

    #[test]
    fn ascii_identifiers_change_as_whole_words() {
        let (old_changed, new_changed) = intraline("let count = 1;", "let total = 1;");

        assert_eq!(old_changed.len(), 1);
        assert_eq!(old_changed[0], 4..9);
        assert_eq!(new_changed.len(), 1);
        assert_eq!(new_changed[0], 4..9);
    }

    #[test]
    fn unpaired_replace_lines_become_fillers() {
        let old = plain(&["shared", "one"]);
        let new = plain(&["shared", "uno", "extra"]);

        let layers = compute(&old, &new);

        assert_eq!(layers.old_rows.len(), 3);
        assert!(matches!(layers.old_rows[1].kind, RowKind::Changed { .. }));
        assert!(matches!(layers.new_rows[1].kind, RowKind::Changed { .. }));
        assert!(matches!(layers.old_rows[2].kind, RowKind::Filler));
        assert!(matches!(layers.new_rows[2].kind, RowKind::Changed { .. }));
    }

    #[test]
    fn insert_into_empty_side_marks_whole_line() {
        let (old_changed, new_changed) = intraline("", "全部あたらしい");

        assert!(old_changed.is_empty());
        assert_eq!(new_changed.len(), 1);
        assert_eq!(new_changed[0], 0.."全部あたらしい".len());
    }
}

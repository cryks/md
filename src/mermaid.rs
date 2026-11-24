//! ` ```mermaid ` ブロックの中身を図として描き、色を付ける。
//!
//! mermaid-text が読めない書き方は、渡す前に読める書き方へ直す。
//!
//! 色は 3 段で決める。書き手が `style` / `classDef` / `linkStyle` で指定した
//! 色と pie の系列色は、mermaid-text が ANSI エスケープで返すのでそのまま写す。
//! 色のないセルのうち、描き上がった図から見つけた箱の中は、ラベルごとに決めた
//! 箱の色で塗る。残りのセルには、文字の種類から線・矢じり・棒の色を当てる。

use crossterm::style::Color;
use mermaid_text::{
    RenderOptions,
    detect::{DiagramKind, detect},
};

use unicode_width::UnicodeWidthChar;

use crate::style::{StyledLine, TextStyle};

const ESC: char = '\u{1b}';

pub(crate) fn render(source: &str, width: usize) -> Result<Vec<StyledLine>, mermaid_text::Error> {
    let rendered = mermaid_text::render_with_options(
        &compatible_source(source),
        &RenderOptions {
            max_width: Some(width),
            color: true,
            ..RenderOptions::default()
        },
    )?;

    Ok(Canvas::parse(&rendered).paint())
}

/// mermaid-text 0.58 が読めない書き方を、読める書き方へ直す。
fn compatible_source(source: &str) -> String {
    let rewrite: fn(&str) -> Option<String> = match detect(source) {
        Ok(DiagramKind::Sequence) => line_with_supported_arrow,
        Ok(DiagramKind::Flowchart) => line_with_unquoted_title,
        _ => return source.to_owned(),
    };
    source
        .lines()
        .map(|line| rewrite(line).unwrap_or_else(|| line.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// mermaid-text 0.58 が読めない sequenceDiagram の矢印と、その読み替え先。
/// 1 本でもあると図全体が描けないので、× や開いた矢じりの形は失うが、
/// 実線と破線の区別は残して `->>` `-->>` に読み替える。
const UNSUPPORTED_SEQUENCE_ARROWS: [(&str, &str); 4] = [
    ("--x", "-->>"),
    ("--)", "-->>"),
    ("-x", "->>"),
    ("-)", "->>"),
];

/// メッセージ行 `送り手<矢印>受け手: 本文` の矢印が mermaid-text の読めない
/// ものなら、読める矢印に置き換えた行を返す。本文 (`:` より後) にある
/// `-x` などは文字列なので触らない。
fn line_with_supported_arrow(line: &str) -> Option<String> {
    let head = &line[..line.find(':').unwrap_or(line.len())];
    // mermaid-text が読める矢印 (`->` `->>` `-->` `-->>`) はどれも `->` を含む。
    // それがある行は読み替えなくても描けるので、`svc-xray` の `-x` のような
    // ID の一部を矢印と取り違えないよう、手を付けない。
    if head.contains("->") {
        return None;
    }
    let (start, arrow, replacement) = head.match_indices('-').find_map(|(start, _)| {
        UNSUPPORTED_SEQUENCE_ARROWS
            .iter()
            .find(|(arrow, _)| head[start..].starts_with(arrow))
            .map(|&(arrow, replacement)| (start, arrow, replacement))
    })?;
    // participant の別名や loop の見出しのような自由記述の行では、キーワードと
    // 本文の間に空白が入る。矢印の手前に空白を含む行はメッセージ行ではない。
    if head[..start].trim().contains(char::is_whitespace) {
        return None;
    }

    Some(format!(
        "{}{replacement}{}",
        &line[..start],
        &line[start + arrow.len()..]
    ))
}

/// `subgraph ID["タイトル"]` と `subgraph "タイトル"` の引用符を外した行を返す。
/// mermaid-text 0.58 は subgraph のタイトルを引用符ごと表示する。
fn line_with_unquoted_title(line: &str) -> Option<String> {
    let header = line.trim().strip_prefix("subgraph ")?;
    let (id, title) = match header.split_once('[') {
        Some((id, bracketed)) => (id.trim(), bracketed.strip_suffix(']')?),
        None => ("", header),
    };
    let title = title.trim().strip_prefix('"')?.strip_suffix('"')?;

    Some(format!("subgraph {id}[{title}]"))
}

/// mermaid-text の出力を表示桁ごとのセルに並べ直したもの。全角文字の 2 桁目は
/// `None` にする。箱の縁は表示桁でそろうので、全角のラベルを含む行でも縦の
/// 辺を同じ列で拾える。
struct Canvas {
    rows: Vec<Vec<Option<Cell>>>,
}

/// 1 文字ぶんのセル。`marks` は `ch` の後ろに続く幅 0 の文字 (結合文字、
/// 異体字セレクタ、ZWJ) で、桁を取らずに `ch` と重ねて表示される。
#[derive(Clone)]
struct Cell {
    ch: char,
    marks: String,
    colors: SourceColors,
}

impl Cell {
    fn text(&self) -> String {
        format!("{}{}", self.ch, self.marks)
    }
}

/// 箱の外枠の位置 (行と表示桁、両端を含む)。
#[derive(Clone, Copy)]
struct Rect {
    top: usize,
    left: usize,
    bottom: usize,
    right: usize,
}

impl Rect {
    fn contains(self, other: Rect) -> bool {
        self.top < other.top
            && other.bottom < self.bottom
            && self.left < other.left
            && other.right < self.right
    }
}

impl Canvas {
    fn parse(rendered: &str) -> Self {
        Self {
            rows: rendered.lines().map(parse_row).collect(),
        }
    }

    fn glyph(&self, row: usize, column: usize) -> char {
        match self.rows.get(row).and_then(|cells| cells.get(column)) {
            Some(Some(cell)) => cell.ch,
            _ => ' ',
        }
    }

    fn paint(&self) -> Vec<StyledLine> {
        let box_colors = self.box_colors();
        self.rows
            .iter()
            .zip(&box_colors)
            .map(|(cells, colors)| {
                let mut line = StyledLine::empty();
                for (cell, box_color) in cells.iter().zip(colors) {
                    let Some(cell) = cell else { continue };
                    let fallback = match box_color {
                        Some(index) => TextStyle::diagram_box(*index),
                        None => glyph_style(cell.ch),
                    };
                    line.push(cell.text(), cell.colors.style().unwrap_or(fallback));
                }
                line
            })
            .collect()
    }

    /// セルごとの箱の色番号。同じラベルの箱には同じ色を当てるので、
    /// sequenceDiagram の上下に並ぶ同じ参加者は同じ色になる。別の箱を囲む箱
    /// (二重丸の外側など) は中身ごと 1 色に塗ると内側の箱が見分けられないので、
    /// 塗らずに線の色のままにする。
    fn box_colors(&self) -> Vec<Vec<Option<usize>>> {
        let mut colors: Vec<Vec<Option<usize>>> = self
            .rows
            .iter()
            .map(|cells| vec![None; cells.len()])
            .collect();
        let mut labels: Vec<String> = Vec::new();
        let boxes = self.boxes();
        let innermost = boxes
            .iter()
            .filter(|rect| !boxes.iter().any(|other| rect.contains(*other)));

        for &rect in innermost {
            let label = self.label(rect);
            let index = match labels.iter().position(|known| *known == label) {
                Some(index) => index,
                None => {
                    labels.push(label);
                    labels.len() - 1
                }
            };
            for row in &mut colors[rect.top..=rect.bottom] {
                row[rect.left..=rect.right].fill(Some(index));
            }
        }

        colors
    }

    fn boxes(&self) -> Vec<Rect> {
        self.rows
            .iter()
            .enumerate()
            .flat_map(|(top, cells)| (0..cells.len()).map(move |left| (top, left)))
            .filter_map(|(top, left)| self.box_at(top, left))
            .collect()
    }

    /// `(top, left)` を左上の角とする箱。線の曲がり角を箱と取り違えないよう、
    /// 4 辺が縁の文字だけでつながり、内側に 1 行 1 桁以上あるものに限る。
    fn box_at(&self, top: usize, left: usize) -> Option<Rect> {
        if !TOP_LEFT.contains(&self.glyph(top, left)) {
            return None;
        }
        let right = (left + 1..).find(|&column| !is_horizontal_edge(self.glyph(top, column)))?;
        let bottom = (top + 1..).find(|&row| !is_vertical_edge(self.glyph(row, left)))?;

        let closed = right > left + 1
            && bottom > top + 1
            && TOP_RIGHT.contains(&self.glyph(top, right))
            && BOTTOM_LEFT.contains(&self.glyph(bottom, left))
            && BOTTOM_RIGHT.contains(&self.glyph(bottom, right))
            && (left + 1..right).all(|column| is_horizontal_edge(self.glyph(bottom, column)))
            && (top + 1..bottom).all(|row| is_vertical_edge(self.glyph(row, right)));
        closed.then_some(Rect {
            top,
            left,
            bottom,
            right,
        })
    }

    /// 箱の内側の文字列。空白の量は箱の幅で変わるので、連続する空白を 1 個にまとめる。
    fn label(&self, rect: Rect) -> String {
        let mut text = String::new();
        for cells in &self.rows[rect.top + 1..rect.bottom] {
            text.extend(
                cells[rect.left + 1..rect.right]
                    .iter()
                    .flatten()
                    .map(Cell::text),
            );
            text.push(' ');
        }
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

// 箱の四隅。╱ ╲ はひし形・六角形・平行四辺形・台形の角で、形によってどの隅にも来る。
const TOP_LEFT: [char; 4] = ['┌', '╭', '╱', '╲'];
const TOP_RIGHT: [char; 4] = ['┐', '╮', '╱', '╲'];
const BOTTOM_LEFT: [char; 4] = ['└', '╰', '╱', '╲'];
const BOTTOM_RIGHT: [char; 4] = ['┘', '╯', '╱', '╲'];

/// 箱の上下の辺。線が出入りする位置には分岐や矢じりが入る。
fn is_horizontal_edge(ch: char) -> bool {
    matches!(ch, '─' | '┬' | '┴' | '┼') || is_arrow_tip(ch)
}

/// 箱の左右の辺。線が出入りする位置には分岐や矢じりが入る。丸・六角形・旗の
/// 形では、ラベルの行の辺が ( ) < > ⟩ になる。
fn is_vertical_edge(ch: char) -> bool {
    matches!(ch, '│' | '├' | '┤' | '┼' | '(' | ')' | '<' | '>' | '⟩') || is_arrow_tip(ch)
}

/// 線の端の記号。Geometric Shapes の三角 (▲ から ◅ まで) と、端点の × ○ ◆ ◇。
fn is_arrow_tip(ch: char) -> bool {
    matches!(ch, '\u{25B2}'..='\u{25C5}' | '×' | '○' | '◆' | '◇')
}

/// 行を ESC で区切ると、2 個目以降の断片は先頭の 1 文字でエスケープの種類が
/// 決まる。mermaid-text が出すのは、SGR の色指定 (`ESC [ … m`)、`click` が
/// 付ける OSC 8 ハイパーリンク (`ESC ] 8 ; ; URL`)、その終端の ST (`ESC \`)
/// の 3 種類。
///
/// 桁の数え方は mermaid-text の grid にそろえる。幅 0 の文字は桁を取らずに
/// 直前の文字へ付け、幅の決まらない制御文字は 1 桁と数える。
fn parse_row(line: &str) -> Vec<Option<Cell>> {
    let mut cells: Vec<Option<Cell>> = Vec::new();
    let mut colors = SourceColors::default();

    for (index, piece) in line.split(ESC).enumerate() {
        let text = if index == 0 {
            piece
        } else {
            colors.text_after_escape(piece)
        };
        for ch in text.chars() {
            match UnicodeWidthChar::width(ch).unwrap_or(1) {
                0 => {
                    if let Some(cell) = cells.iter_mut().rev().find_map(Option::as_mut) {
                        cell.marks.push(ch);
                    }
                }
                width => {
                    cells.push(Some(Cell {
                        ch,
                        marks: String::new(),
                        colors,
                    }));
                    cells.extend(std::iter::repeat_n(None, width - 1));
                }
            }
        }
    }

    cells
}

#[derive(Clone, Copy, Default)]
struct SourceColors {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
}

impl SourceColors {
    fn text_after_escape<'a>(&mut self, piece: &'a str) -> &'a str {
        if let Some((params, text)) = piece.strip_prefix('[').and_then(|sgr| sgr.split_once('m')) {
            self.apply_sgr(params);
            text
        } else {
            // OSC 8 の断片は URL だけなので表示しない。ST の後ろは表示文字列に戻る。
            piece.strip_prefix('\\').unwrap_or("")
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        let numbers: Vec<u8> = params
            .split(';')
            .filter_map(|number| number.parse().ok())
            .collect();
        match numbers[..] {
            [0] => *self = Self::default(),
            [38, 2, r, g, b] => self.fg = Some(Rgb { r, g, b }),
            [48, 2, r, g, b] => self.bg = Some(Rgb { r, g, b }),
            _ => {}
        }
    }

    /// 書き手の色を写したスタイル。色の指定がないセルと、ダーク背景に溶ける
    /// 線の色だけの指定は None にして、`md` の配色に任せる。
    fn style(self) -> Option<TextStyle> {
        match (self.fg, self.bg) {
            // fill だけの指定では文字色が決まらない。明るい塗りには暗い文字を
            // 載せ、暗い塗りでは本文と同じ既定色に任せる。
            (None, Some(fill)) if fill.is_light() => Some(TextStyle::on_light_fill(fill.color())),
            (fg, Some(fill)) => Some(TextStyle {
                fg: fg.map(Rgb::color),
                bg: Some(fill.color()),
                ..TextStyle::normal()
            }),
            (Some(fg), None) if fg.is_visible_on_dark() => Some(TextStyle {
                fg: Some(fg.color()),
                ..TextStyle::normal()
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    fn color(self) -> Color {
        Color::Rgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }

    /// WCAG 2 の相対輝度 (0.0 が黒、1.0 が白)。
    fn luminance(self) -> f32 {
        let linear = |channel: u8| {
            let value = f32::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// 0.179 は、WCAG のコントラスト比で黒い文字と白い文字の読みやすさが
    /// 入れ替わる輝度。
    fn is_light(self) -> bool {
        self.luminance() > 0.179
    }

    /// ダーク背景の上で線や文字として見分けられるか。Mermaid の作例は白い紙を
    /// 前提に `stroke:#333` のような暗い色を指定することが多く、そのまま使う
    /// と線が背景に溶けて消える。0.08 は、配色が前提にするダーク背景
    /// (#1E1E2E) に対するコントラスト比がおよそ 2:1 になる輝度。
    fn is_visible_on_dark(self) -> bool {
        self.luminance() >= 0.08
    }
}

/// 箱の外にあるセルの配色。mermaid-text は図の構造を返さないので、文字の
/// Unicode ブロックから矢じり・線・棒・ラベルを見分ける。
fn glyph_style(ch: char) -> TextStyle {
    match ch {
        // Block Elements の網掛け 3 種と、journey の空の星。
        '░'..='▓' | '☆' => TextStyle::diagram_bar_track(),
        // 残りの Block Elements と、journey の塗られた星。
        '\u{2580}'..='\u{259F}' | '★' => TextStyle::diagram_bar(),
        ch if is_arrow_tip(ch) => TextStyle::diagram_arrow(),
        // Box Drawing と、残りの Geometric Shapes (state の始点 ● など)。
        '\u{2500}'..='\u{257F}' | '\u{25A0}'..='\u{25FF}' => TextStyle::diagram_line(),
        _ => TextStyle::normal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb { r, g, b }
    }

    fn plain_text(lines: &[StyledLine]) -> String {
        lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn style_of(lines: &[StyledLine], text: &str) -> TextStyle {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.text.contains(text))
            .map(|span| span.style)
            .unwrap()
    }

    /// 図の 2 行目に並ぶ、上側の参加者の箱のラベル。
    fn participant_names(lines: &[StyledLine]) -> Vec<String> {
        lines[1]
            .plain_text()
            .split(['│', ' '])
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// メッセージの本文は矢印の 1 行上に描かれる。
    fn arrow_below(lines: &[StyledLine], message: &str) -> String {
        let label_row = lines
            .iter()
            .position(|line| line.plain_text().contains(message))
            .unwrap();
        lines[label_row + 1].plain_text()
    }

    #[test]
    fn cross_and_open_arrows_keep_solid_or_dashed_lines() {
        for (arrow, dashed) in [("--x", true), ("--)", true), ("-x", false), ("-)", false)] {
            let lines = render(&format!("sequenceDiagram\nA{arrow}B: ping"), 80).unwrap();
            let line = arrow_below(&lines, "ping");

            assert_eq!(line.contains('┄'), dashed, "{arrow}");
            assert_eq!(line.contains('─'), !dashed, "{arrow}");
        }
    }

    #[test]
    fn arrow_like_text_in_a_message_is_kept() {
        let lines = render("sequenceDiagram\nA--xB: retry -x or -) later", 80).unwrap();

        assert!(plain_text(&lines).contains("retry -x or -) later"));
    }

    #[test]
    fn deactivation_mark_before_an_id_starting_with_x_is_not_an_arrow() {
        let lines = render("sequenceDiagram\nxavier->>+B: call\nB->>-xavier: done", 80).unwrap();

        assert_eq!(participant_names(&lines), ["xavier", "B"]);
    }

    #[test]
    fn id_containing_a_cross_or_open_arrow_is_kept_beside_a_supported_arrow() {
        for (source, sender) in [
            ("svc-xray->>B: hi\nB-->>svc-xray: ok", "svc-xray"),
            ("api-)x->>B: hi", "api-)x"),
        ] {
            let lines = render(&format!("sequenceDiagram\n{source}"), 80).unwrap();

            assert_eq!(participant_names(&lines), [sender, "B"], "{source}");
        }
    }

    #[test]
    fn keyword_line_with_an_arrow_like_word_is_kept() {
        let lines = render(
            "sequenceDiagram\nparticipant A as non-xml\nloop re-)try\nA->>B: hi\nend",
            80,
        )
        .unwrap();
        let text = plain_text(&lines);

        assert!(text.contains("non-xml"));
        assert!(text.contains("re-)try"));
    }

    #[test]
    fn flowchart_cross_edge_keeps_its_cross() {
        let lines = render("graph LR\nA --x B", 80).unwrap();

        assert!(plain_text(&lines).contains('×'));
    }

    #[test]
    fn quoted_subgraph_titles_lose_their_quotes() {
        for header in [
            "subgraph S1[\"Stack: staging\"]",
            "subgraph S1 [\"Stack: staging\"]",
            "subgraph [\"Stack: staging\"]",
            "subgraph \"Stack: staging\"",
        ] {
            let source = format!("flowchart LR\n{header}\nA --> B\nend");
            let text = plain_text(&render(&source, 80).unwrap());

            assert!(text.contains("Stack: staging"), "{header}");
            assert!(!text.contains('"'), "{header}");
        }
    }

    #[test]
    fn click_link_leaves_no_escape_in_the_text() {
        let lines = render(
            "graph LR\nA[Build] --> B[Test]\nclick B \"https://example.com\"",
            80,
        )
        .unwrap();
        let text = plain_text(&lines);

        assert!(text.contains("Test"));
        assert!(!text.contains(ESC));
        assert!(!text.contains("example.com"));
    }

    #[test]
    fn source_stroke_colors_the_node_border() {
        let lines = render("graph LR\nA[Build]\nstyle A stroke:#ff0000", 80).unwrap();

        assert_eq!(style_of(&lines, "┌").fg, Some(rgb(255, 0, 0)));
        assert_eq!(style_of(&lines, "Build"), TextStyle::diagram_box(0));
        assert!(!plain_text(&lines).contains(ESC));
    }

    #[test]
    fn light_fill_gets_a_dark_label() {
        let lines = render("graph LR\nA[Build]\nstyle A fill:#ff99ff", 80).unwrap();

        assert_eq!(
            style_of(&lines, "Build"),
            TextStyle::on_light_fill(rgb(255, 153, 255))
        );
    }

    #[test]
    fn dark_fill_keeps_the_default_label_color() {
        let lines = render("graph LR\nA[Build]\nstyle A fill:#333366", 80).unwrap();
        let label = style_of(&lines, "Build");

        assert_eq!(label.bg, Some(rgb(51, 51, 102)));
        assert_eq!(label.fg, None);
    }

    #[test]
    fn each_box_gets_its_own_color_and_edges_keep_the_line_color() {
        let lines = render("graph LR\nA[Build] --> B[Test]", 80).unwrap();

        assert_eq!(style_of(&lines, "┌"), TextStyle::diagram_box(0));
        assert_eq!(style_of(&lines, "Build"), TextStyle::diagram_box(0));
        assert_eq!(style_of(&lines, "Test"), TextStyle::diagram_box(1));
        assert_eq!(style_of(&lines[1..2], "─"), TextStyle::diagram_line());
        assert_eq!(style_of(&lines, "▸"), TextStyle::diagram_arrow());
    }

    #[test]
    fn participant_boxes_are_found_by_display_column() {
        let lines = render("sequenceDiagram\nparticipant A as 転送先\nA->>B: hi", 80).unwrap();
        let (top, bottom) = (&lines[..3], &lines[lines.len() - 3..]);

        assert_eq!(style_of(top, "┐"), TextStyle::diagram_box(0));
        assert_eq!(style_of(top, "転送先"), TextStyle::diagram_box(0));
        assert_eq!(style_of(bottom, "転送先"), TextStyle::diagram_box(0));
        assert_eq!(style_of(top, "B"), TextStyle::diagram_box(1));
        assert_eq!(style_of(bottom, "B"), TextStyle::diagram_box(1));
    }

    #[test]
    fn zero_width_characters_do_not_shift_the_boxes_after_them() {
        for label in ["⚠\u{FE0F} 注意", "テ\u{3099}ータ", "Cafe\u{301}"] {
            let lines = render(&format!("graph LR\nA[{label}] --> B[ok]"), 80).unwrap();

            assert_eq!(
                style_of(&lines, label),
                TextStyle::diagram_box(0),
                "{label}"
            );
            assert_eq!(style_of(&lines, "ok"), TextStyle::diagram_box(1), "{label}");
        }
    }

    #[test]
    fn zero_width_character_in_a_participant_keeps_both_ends_colored() {
        let lines = render(
            "sequenceDiagram\nparticipant A as Cafe\u{301}\nA->>B: hi",
            80,
        )
        .unwrap();

        for end in [&lines[..3], &lines[lines.len() - 3..]] {
            assert_eq!(style_of(end, "Cafe\u{301}"), TextStyle::diagram_box(0));
            assert_eq!(style_of(end, "B"), TextStyle::diagram_box(1));
        }
    }

    #[test]
    fn decision_and_round_shapes_are_boxes_too() {
        let lines = render("graph LR\nA{Check} --> B{{Hex}} --> C((Retry))", 80).unwrap();

        assert_eq!(style_of(&lines, "Check"), TextStyle::diagram_box(0));
        assert_eq!(style_of(&lines, "Hex"), TextStyle::diagram_box(1));
        assert_eq!(style_of(&lines, "Retry"), TextStyle::diagram_box(2));
    }

    #[test]
    fn arrow_tip_on_a_box_edge_takes_the_box_color() {
        let lines = render("graph TD\nA[Top] --> B[Bottom]", 80).unwrap();

        assert_eq!(style_of(&lines, "▾"), TextStyle::diagram_box(1));
    }

    #[test]
    fn lines_closing_on_three_sides_are_not_a_box() {
        let lines = render(
            "gitGraph\ncommit\nbranch dev\ncheckout dev\ncommit\ncheckout main\nmerge dev",
            80,
        )
        .unwrap();

        assert_eq!(style_of(&lines[2..3], "*"), TextStyle::normal());
    }

    #[test]
    fn a_box_around_another_box_keeps_the_line_color() {
        let lines = render("graph LR\nA(((Done)))", 80).unwrap();

        assert_eq!(style_of(&lines, "╭"), TextStyle::diagram_line());
        assert_eq!(style_of(&lines, "Done"), TextStyle::diagram_box(0));
    }

    #[test]
    fn stroke_too_dark_to_see_falls_back_to_the_palette() {
        let lines = render("graph LR\nA[Build]\nstyle A stroke:#333333", 80).unwrap();

        assert_eq!(style_of(&lines, "┌"), TextStyle::diagram_box(0));
    }

    #[test]
    fn bars_and_their_empty_track_get_separate_colors() {
        let lines = render(
            "gantt\ndateFormat YYYY-MM-DD\nsection A\nFirst :a1, 2026-01-01, 5d\nSecond :after a1, 5d",
            80,
        )
        .unwrap();

        assert_eq!(style_of(&lines, "█"), TextStyle::diagram_bar());
        assert_eq!(style_of(&lines, "░"), TextStyle::diagram_bar_track());
    }
}

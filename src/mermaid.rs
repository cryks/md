//! ` ```mermaid ` ブロックの中身を図として描き、色を付ける。
//!
//! 色の出どころは 2 つある。書き手が `style` / `classDef` / `linkStyle` で
//! 指定した色と pie の系列色は、mermaid-text が ANSI エスケープで返すので span
//! へ写す。色のないセルには、文字の種類から `md` の配色を当てる。

use crossterm::style::Color;
use mermaid_text::RenderOptions;

use crate::style::{StyledLine, TextStyle};

const ESC: char = '\u{1b}';

pub(crate) fn render(source: &str, width: usize) -> Result<Vec<StyledLine>, mermaid_text::Error> {
    let rendered = mermaid_text::render_with_options(
        source,
        &RenderOptions {
            max_width: Some(width),
            color: true,
            ..RenderOptions::default()
        },
    )?;

    Ok(rendered.lines().map(styled_line).collect())
}

/// 行を ESC で区切ると、2 個目以降の断片は先頭の 1 文字でエスケープの種類が
/// 決まる。mermaid-text が出すのは、SGR の色指定 (`ESC [ … m`)、`click` が
/// 付ける OSC 8 ハイパーリンク (`ESC ] 8 ; ; URL`)、その終端の ST (`ESC \`)
/// の 3 種類。
fn styled_line(line: &str) -> StyledLine {
    let mut output = StyledLine::empty();
    let mut colors = SourceColors::default();

    for (index, piece) in line.split(ESC).enumerate() {
        let text = if index == 0 {
            piece
        } else {
            colors.text_after_escape(piece)
        };
        for ch in text.chars() {
            output.push(ch.to_string(), colors.style_for(ch));
        }
    }

    output
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

    fn style_for(self, ch: char) -> TextStyle {
        match (self.fg, self.bg) {
            // fill だけの指定では文字色が決まらない。明るい塗りには暗い文字を
            // 載せ、暗い塗りでは本文と同じ既定色に任せる。
            (None, Some(fill)) if fill.is_light() => TextStyle::on_light_fill(fill.color()),
            (fg, Some(fill)) => TextStyle {
                fg: fg.map(Rgb::color),
                bg: Some(fill.color()),
                ..TextStyle::normal()
            },
            (Some(fg), None) if fg.is_visible_on_dark() => TextStyle {
                fg: Some(fg.color()),
                ..TextStyle::normal()
            },
            _ => glyph_style(ch),
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

/// 色指定のないセルの配色。mermaid-text は図の構造を返さないので、文字の
/// Unicode ブロックから線・棒・ラベルを見分ける。
fn glyph_style(ch: char) -> TextStyle {
    match ch {
        // Block Elements の網掛け 3 種と、journey の空の星。
        '░'..='▓' | '☆' => TextStyle::diagram_bar_track(),
        // 残りの Block Elements と、journey の塗られた星。
        '\u{2580}'..='\u{259F}' | '★' => TextStyle::diagram_bar(),
        // Box Drawing と Geometric Shapes (矢じり、端点の ◆ ○ ●)。
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
    fn unstyled_diagram_colors_lines_and_leaves_labels() {
        let lines = render("graph LR\nA[Build] --> B[Test]", 80).unwrap();

        assert_eq!(style_of(&lines, "┌"), TextStyle::diagram_line());
        assert_eq!(style_of(&lines, "▸"), TextStyle::diagram_line());
        assert_eq!(style_of(&lines, "Build"), TextStyle::normal());
    }

    #[test]
    fn stroke_too_dark_to_see_falls_back_to_the_palette() {
        let lines = render("graph LR\nA[Build]\nstyle A stroke:#333333", 80).unwrap();

        assert_eq!(style_of(&lines, "┌"), TextStyle::diagram_line());
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

use pulldown_cmark::{Options, Parser};
use syntect::{easy::HighlightLines, parsing::SyntaxSet};

use crate::{
    inline::render_inline,
    mermaid,
    style::{StyledLine, TextStyle, display_width, wrap_lines},
    table,
};

pub(crate) fn render_markdown(source: &str, width: usize) -> RenderedDoc {
    let mut renderer = MarkdownRenderer::new(width);
    renderer.render(source)
}

/// 描画行と、pager の sticky 表示が参照する見出し位置。
pub(crate) struct RenderedDoc {
    pub(crate) lines: Vec<StyledLine>,
    /// h1-h3 の位置 (line 昇順)。h4 以下は sticky 対象外なので記録しない。
    /// h3 までという境界は `TextStyle::heading` の「色付きは h3 まで」と同じ。
    pub(crate) headings: Vec<Heading>,
}

/// `RenderedDoc::lines` 内の見出し 1 個。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Heading {
    /// 見出しの行 index。テキストが長く折り返された場合は先頭行を指す。
    pub(crate) line: usize,
    /// ATX 見出しレベル (1..=3)。
    pub(crate) level: usize,
    /// `#` マーカーと右へ伸びる罫線を除いた見出しの文字列。セクション一覧
    /// のように、行をそのまま出すと罫線が邪魔になる場所で使う。
    pub(crate) text: String,
}

struct MarkdownRenderer {
    width: usize,
    syntax_set: SyntaxSet,
    theme: syntect::highlighting::Theme,
}

impl MarkdownRenderer {
    fn new(width: usize) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = syntect::highlighting::ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| theme_set.themes.values().next().cloned())
            .unwrap_or_default();

        Self {
            width: width.max(1),
            syntax_set,
            theme,
        }
    }

    fn render(&mut self, source: &str) -> RenderedDoc {
        let _ = Parser::new_ext(source, markdown_options()).count();

        let lines = source.lines().collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut headings = Vec::new();
        let mut index = 0usize;

        while index < lines.len() {
            if let Some((next, block)) = self.frontmatter_at(&lines, index) {
                output.extend(block);
                index = next;
                continue;
            }

            if let Some((next, block)) = self.code_block_at(&lines, index) {
                output.extend(block);
                index = next;
                continue;
            }

            if let Some(end) = table::table_at(&lines, index) {
                output.extend(table::render_table(&lines[index..end], self.width));
                index = end;
                continue;
            }

            if let Some((next, block)) = self.alert_at(&lines, index) {
                output.extend(block);
                index = next;
                continue;
            }

            if let Some((level, prefix)) = heading_at(lines[index]) {
                if level <= 2 {
                    pad_section_gap(&mut output);
                }
                // pad_section_gap の後に記録し、挿入された空行込みの位置を指す。
                if level <= 3 {
                    headings.push(Heading {
                        line: output.len(),
                        level,
                        text: lines[index][prefix..].trim_end().to_owned(),
                    });
                }
            }

            output.extend(wrap_lines(
                &[self.render_markdown_line(lines[index])],
                self.width,
            ));
            index += 1;
        }

        if source.ends_with('\n') {
            output.push(StyledLine::empty());
        }

        RenderedDoc {
            lines: output,
            headings,
        }
    }

    fn frontmatter_at(&self, lines: &[&str], start: usize) -> Option<(usize, Vec<StyledLine>)> {
        if start != 0 || lines.first().copied()? != "---" {
            return None;
        }

        let end = lines
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, line)| matches!(line.trim(), "---" | "...").then_some(index))?;

        let mut output = Vec::new();
        output.push(StyledLine::styled(lines[0], TextStyle::marker()));
        output.extend(self.highlight_code("yaml", &lines[1..end]));
        output.push(StyledLine::styled(lines[end], TextStyle::marker()));
        Some((end + 1, output))
    }

    fn code_block_at(&mut self, lines: &[&str], start: usize) -> Option<(usize, Vec<StyledLine>)> {
        let line = lines.get(start)?;
        let trimmed = line.trim_start();
        let fence = if trimmed.starts_with("```") {
            "```"
        } else if trimmed.starts_with("~~~") {
            "~~~"
        } else {
            return None;
        };

        let language = trimmed
            .trim_start_matches(fence)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();

        let mut body = Vec::new();
        let mut end = start + 1;
        while end < lines.len() {
            let candidate = lines[end].trim_start();
            if candidate.starts_with(fence) {
                break;
            }
            body.push(lines[end]);
            end += 1;
        }

        let has_closing_fence = end < lines.len();
        let next = if has_closing_fence { end + 1 } else { end };
        let mut output = Vec::new();

        if language.eq_ignore_ascii_case("mermaid") {
            let source = body.join("\n");
            match mermaid::render(&source, self.width) {
                Ok(rendered_lines) => {
                    output.push(StyledLine::styled(*line, TextStyle::marker()));
                    output.extend(rendered_lines);
                    if has_closing_fence {
                        output.push(StyledLine::styled(lines[end], TextStyle::marker()));
                    }
                }
                Err(error) => {
                    output.push(StyledLine::styled(
                        format!("Mermaid render error: {error}"),
                        TextStyle::error(),
                    ));
                    output.push(StyledLine::styled(*line, TextStyle::marker()));
                    output.extend(self.highlight_code(&language, &body));
                    if has_closing_fence {
                        output.push(StyledLine::styled(lines[end], TextStyle::marker()));
                    }
                }
            }
        } else {
            output.push(StyledLine::styled(*line, TextStyle::marker()));
            output.extend(self.highlight_code(&language, &body));
            if has_closing_fence {
                output.push(StyledLine::styled(lines[end], TextStyle::marker()));
            }
        }

        Some((next, output))
    }

    /// 行頭から始まる引用だけを alert とみなす。alert の棒は 0 桁目から引くので、
    /// リスト項目の中の引用まで alert にすると、リストのインデントが消える。
    fn alert_at(&self, lines: &[&str], start: usize) -> Option<(usize, Vec<StyledLine>)> {
        let continues_quote = start > 0 && quote_text(lines[start - 1]).is_some();
        if continues_quote || !lines[start].starts_with('>') {
            return None;
        }
        let (kind, first_body) = AlertKind::parse_marker(quote_text(lines[start])?)?;

        let mut body = Vec::new();
        if !first_body.trim().is_empty() {
            body.push(first_body.trim_start());
        }
        let mut end = start + 1;
        while let Some(text) = lines.get(end).and_then(|line| quote_text(line)) {
            body.push(text);
            end += 1;
        }

        let bar = TextStyle::alert_bar(kind);
        let mut title = StyledLine::styled(ALERT_BAR, bar);
        title.push(kind.label(), TextStyle::alert_title(kind));
        let mut output = vec![title];
        let body_width = self.width.saturating_sub(display_width(ALERT_BAR));
        for text in body {
            for wrapped in wrap_lines(&[render_inline(text)], body_width) {
                let mut row = StyledLine::styled(ALERT_BAR, bar);
                row.spans.extend(wrapped.spans);
                output.push(row);
            }
        }

        Some((end, output))
    }

    fn highlight_code(&self, language: &str, body: &[&str]) -> Vec<StyledLine> {
        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut output = Vec::new();

        for raw_line in body {
            match highlighter.highlight_line(raw_line, &self.syntax_set) {
                Ok(ranges) => {
                    let mut line = StyledLine::empty();
                    for (style, text) in ranges {
                        line.push(text, TextStyle::syntect(style.foreground));
                    }
                    output.push(line);
                }
                Err(_) => output.push(StyledLine::styled(*raw_line, TextStyle::code())),
            }
        }

        output
    }

    fn render_markdown_line(&self, line: &str) -> StyledLine {
        if line.trim().is_empty() {
            return StyledLine::empty();
        }

        if let Some((level, prefix)) = heading_at(line) {
            let mut output = StyledLine::empty();
            output.push(&line[..prefix], TextStyle::marker());
            output.push(line[prefix..].trim_end(), TextStyle::heading(level));

            // 見出しの右に罫線を伸ばし、届く位置と太さでレベルの段差を作る:
            // h1 は画面幅までの太線、h2 は画面幅までの細線、h3 は画面幅の
            // 半分までの細線、h4 以下はなし。罫線の右端が揃うので、テキスト
            // 長に関係なくレベルを見比べられる。目標位置までに余白が
            // 4 セル未満 (区切り空白 1 + 罫線 3 が入らない) のときは
            // 引かない。折り返し行に罫線が割り込んで崩れるのを避けるため。
            if level <= 3 {
                let target = if level <= 2 {
                    self.width
                } else {
                    self.width / 2
                };
                let used = display_width(&output.plain_text());
                if used + 4 <= target {
                    let glyph = if level == 1 { "━" } else { "─" };
                    output.push(" ", TextStyle::normal());
                    output.push(glyph.repeat(target - used - 1), TextStyle::marker());
                }
            }
            return output;
        }

        let trimmed_start = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix('>') {
            let mut output = StyledLine::empty();
            output.push(&line[..trimmed_start], TextStyle::normal());
            output.push(">", TextStyle::marker());
            output.push(rest, TextStyle::quote());
            return output;
        }

        if let Some(marker_len) = list_marker_len(trimmed) {
            let mut output = StyledLine::empty();
            output.push(&line[..trimmed_start], TextStyle::normal());
            output.push(&trimmed[..marker_len], TextStyle::list_marker());
            output
                .spans
                .extend(render_inline(&trimmed[marker_len..]).spans);
            return output;
        }

        if is_rule(trimmed) {
            return StyledLine::styled(line, TextStyle::marker());
        }

        render_inline(line)
    }
}

fn markdown_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_HEADING_ATTRIBUTES
}

/// ATX 見出しなら (レベル, テキスト開始 byte 位置) を返す。
fn heading_at(line: &str) -> Option<(usize, usize)> {
    let trimmed_start = line.len() - line.trim_start().len();
    let trimmed = line.trim_start();
    let count = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&count) && trimmed.as_bytes().get(count) == Some(&b' ') {
        Some((count, trimmed_start + count + 1))
    } else {
        None
    }
}

/// h1/h2 の直前を空行 2 行にそろえる。h3 以下は原文どおり (慣習的に 1 行)
/// なので、縦の余白の差が大セクションの切れ目になる。原文に既に 2 行以上の
/// 空行があれば足さない。文書冒頭の見出し (まだ本文が出力されていない) にも
/// 足さない。
fn pad_section_gap(output: &mut Vec<StyledLine>) {
    if !output.iter().any(|line| !line.spans.is_empty()) {
        return;
    }

    let trailing_blanks = output
        .iter()
        .rev()
        .take_while(|line| line.spans.is_empty())
        .count();
    for _ in trailing_blanks..2 {
        output.push(StyledLine::empty());
    }
}

fn list_marker_len(trimmed: &str) -> Option<usize> {
    for marker in ["- ", "* ", "+ "] {
        if trimmed.starts_with(marker) {
            return Some(marker.len());
        }
    }

    let dot = trimmed.find(". ")?;
    if dot > 0 && trimmed[..dot].chars().all(|ch| ch.is_ascii_digit()) {
        Some(dot + 2)
    } else {
        None
    }
}

fn is_rule(trimmed: &str) -> bool {
    let compact = trimmed.split_whitespace().collect::<String>();
    compact.len() >= 3
        && (compact.chars().all(|ch| ch == '-')
            || compact.chars().all(|ch| ch == '*')
            || compact.chars().all(|ch| ch == '_'))
}

fn quote_text(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

const ALERT_BAR: &str = "┃ ";

/// GitHub の alert (1 行目が `[!NOTE]` などの引用) の種類。
#[derive(Clone, Copy)]
pub(crate) enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl AlertKind {
    fn parse_marker(text: &str) -> Option<(Self, &str)> {
        let (name, rest) = text.trim_start().strip_prefix("[!")?.split_once(']')?;
        let kind = match name.to_ascii_uppercase().as_str() {
            "NOTE" => Self::Note,
            "TIP" => Self::Tip,
            "IMPORTANT" => Self::Important,
            "WARNING" => Self::Warning,
            "CAUTION" => Self::Caution,
            _ => return None,
        };
        Some((kind, rest))
    }

    fn label(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Tip => "Tip",
            Self::Important => "Important",
            Self::Warning => "Warning",
            Self::Caution => "Caution",
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Option<Color> {
        Some(Color::Rgb { r, g, b })
    }

    fn row_bar_color(line: &StyledLine) -> Option<Color> {
        line.spans.first().and_then(|span| span.style.fg)
    }

    fn assert_renders_as_plain_quote(source: &str) {
        let lines = render_markdown(source, 40).lines;

        assert_eq!(lines.len(), source.lines().count());
        for (line, source_line) in lines.iter().zip(source.lines()) {
            assert_eq!(line.plain_text(), source_line);
            assert_eq!(line.spans[0].text, ">");
            assert_eq!(line.spans[0].style, TextStyle::marker());
        }
    }

    #[test]
    fn alert_draws_every_row_in_its_level_color() {
        let levels = [
            ("NOTE", rgb(0x89, 0xB4, 0xFA)),
            ("TIP", rgb(0xA6, 0xE3, 0xA1)),
            ("IMPORTANT", rgb(0xCB, 0xA6, 0xF7)),
            ("WARNING", rgb(0xF9, 0xE2, 0xAF)),
            ("CAUTION", rgb(0xF3, 0x8B, 0xA8)),
        ];

        for (marker, color) in levels {
            let lines = render_markdown(&format!("> [!{marker}]\n> body"), 40).lines;

            assert_eq!(lines.len(), 2, "{marker}");
            for line in &lines {
                assert_eq!(row_bar_color(line), color, "{marker}");
            }
        }
    }

    #[test]
    fn alert_title_row_is_bold_in_its_level_color() {
        let lines = render_markdown("> [!WARNING]\n> body", 40).lines;
        let yellow = rgb(0xF9, 0xE2, 0xAF);

        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.bold && span.style.fg == yellow)
        );
        assert!(!lines[1].spans.iter().any(|span| span.style.bold));
    }

    #[test]
    fn alert_bar_continues_on_wrapped_rows() {
        let lines = render_markdown("> [!NOTE]\n> abcdefghijklmnopqrst", 12).lines;
        let blue = rgb(0x89, 0xB4, 0xFA);

        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(row_bar_color(line), blue);
            assert!(display_width(&line.plain_text()) <= 12);
        }
    }

    #[test]
    fn alert_marker_inside_a_quote_stays_a_plain_quote() {
        assert_renders_as_plain_quote("> text\n> [!NOTE]\n> more");
    }

    #[test]
    fn alert_marker_is_case_insensitive() {
        let yellow = rgb(0xF9, 0xE2, 0xAF);

        for marker in ["[!warning]", "[!Warning]"] {
            let lines = render_markdown(&format!("> {marker}\n> body"), 40).lines;

            assert!(
                lines.iter().all(|line| row_bar_color(line) == yellow),
                "{marker}"
            );
        }
    }

    #[test]
    fn indented_alert_marker_stays_a_plain_quote() {
        let source = "- item\n  > [!NOTE]\n  > text";
        let lines = render_markdown(source, 40).lines;

        let texts: Vec<String> = lines.iter().map(StyledLine::plain_text).collect();
        assert_eq!(texts, source.lines().collect::<Vec<_>>());
    }

    #[test]
    fn unknown_alert_marker_stays_a_plain_quote() {
        assert_renders_as_plain_quote("> [!UNKNOWN]\n> text");
    }

    #[test]
    fn alert_consumes_its_marker_and_keeps_the_text_after_it() {
        let texts = render_markdown("> [!NOTE] first\n> second", 40)
            .lines
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>();

        assert_eq!(texts.len(), 3);
        assert!(!texts.iter().any(|text| text.contains("[!")));
        assert!(texts[1].ends_with("first"));
        assert!(texts[2].ends_with("second"));
    }

    #[test]
    fn blank_quote_row_inside_alert_keeps_the_bar() {
        let lines = render_markdown("> [!TIP]\n> a\n>\n> b", 40).lines;
        let green = rgb(0xA6, 0xE3, 0xA1);

        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| row_bar_color(line) == green));
    }

    #[test]
    fn alert_ends_at_the_first_line_outside_the_quote() {
        let lines = render_markdown("> [!NOTE]\n> a\n\nafter", 40).lines;

        assert_eq!(row_bar_color(&lines[1]), rgb(0x89, 0xB4, 0xFA));
        assert!(lines[2].spans.is_empty());
        assert_eq!(lines[3].plain_text(), "after");
        assert_eq!(row_bar_color(&lines[3]), None);
    }

    #[test]
    fn alert_body_keeps_inline_styles() {
        let lines = render_markdown("> [!NOTE]\n> run `deploy.sh` now", 40).lines;

        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.text == "deploy.sh" && span.style == TextStyle::code())
        );
    }

    #[test]
    fn renders_table_as_box() {
        let source = "| name | value |\n| --- | ---: |\n| alpha | 10 |";
        let output = render_markdown(source, 80)
            .lines
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("┌"));
        assert!(output.contains("alpha"));
    }

    #[test]
    fn renders_mermaid_block_as_diagram() {
        let source = "```mermaid\ngraph LR; A[Build] --> B[Test]\n```";
        let output = render_markdown(source, 80)
            .lines
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("Build"));
        assert!(output.contains("Test"));
        assert!(output.contains("```mermaid"));
    }

    #[test]
    fn highlights_yaml_frontmatter() {
        let lines = render_markdown("---\ntitle: Hello\n---\n# Body", 80).lines;

        assert_eq!(lines[0].plain_text(), "---");
        assert_eq!(lines[2].plain_text(), "---");
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.text.contains("title") && span.style != TextStyle::normal())
        );
    }

    #[test]
    fn renders_list_item_with_subtle_marker() {
        let lines = render_markdown("- alpha", 80).lines;

        assert_eq!(lines[0].plain_text(), "- alpha");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.text == "- " && span.style.fg == TextStyle::list_marker().fg)
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.text.contains("alpha") && !span.style.bold)
        );
    }

    #[test]
    fn h1_and_h2_extend_rule_to_terminal_width() {
        let lines = render_markdown("# Title\n\ntext\n\n## Section", 40).lines;

        let h1 = &lines[0];
        assert_eq!(display_width(&h1.plain_text()), 40);
        assert!(h1.plain_text().ends_with('━'));

        let h2 = lines
            .iter()
            .find(|line| line.plain_text().starts_with("## Section"))
            .unwrap();
        assert_eq!(display_width(&h2.plain_text()), 40);
        assert!(h2.plain_text().ends_with('─'));
    }

    #[test]
    fn h3_rule_extends_to_half_width() {
        let lines = render_markdown("### Section", 40).lines;

        assert_eq!(display_width(&lines[0].plain_text()), 20);
        assert!(lines[0].plain_text().ends_with('─'));
    }

    #[test]
    fn h4_has_no_rule() {
        let lines = render_markdown("#### Section", 40).lines;

        assert_eq!(lines[0].plain_text(), "#### Section");
    }

    #[test]
    fn skips_rule_when_heading_leaves_no_room() {
        // 区切り空白 1 + 罫線 3 セルが入らない幅では罫線を引かない
        let lines = render_markdown("## abcdef", 12).lines;

        assert!(!lines[0].plain_text().contains('─'));
    }

    #[test]
    fn h2_gets_two_blank_lines_before_it() {
        let texts = render_markdown("body\n\n## Section", 40)
            .lines
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>();

        assert_eq!(texts[0], "body");
        assert!(texts[1].is_empty());
        assert!(texts[2].is_empty());
        assert!(texts[3].starts_with("## Section"));
    }

    #[test]
    fn leading_heading_gets_no_extra_blank() {
        let lines = render_markdown("# Title", 40).lines;

        assert!(lines[0].plain_text().starts_with("# Title"));
    }

    #[test]
    fn heading_positions_point_at_their_lines() {
        // pad_section_gap が h2 の前へ挿入する空行込みで line が実位置を指し、
        // h4 は headings に入らない。
        let doc = render_markdown("# A\n\nbody\n\n## B\n\ntext\n\n#### deep\n\n### C", 40);

        let described = doc
            .headings
            .iter()
            .map(|heading| (heading.level, doc.lines[heading.line].plain_text()))
            .collect::<Vec<_>>();
        assert_eq!(described.len(), 3);
        assert_eq!(described[0].0, 1);
        assert!(described[0].1.starts_with("# A"));
        assert_eq!(described[1].0, 2);
        assert!(described[1].1.starts_with("## B"));
        assert_eq!(described[2].0, 3);
        assert!(described[2].1.starts_with("### C"));
    }

    #[test]
    fn preserves_code_block_overflow_for_horizontal_scroll() {
        let source = "```text\nabcdefghijklmnopqrstuvwxyz\n```";
        let lines = render_markdown(source, 10).lines;

        assert!(
            lines
                .iter()
                .any(|line| line.plain_text() == "abcdefghijklmnopqrstuvwxyz")
        );
    }
}

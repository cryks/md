use mermaid_text::RenderOptions;
use pulldown_cmark::{Options, Parser};
use syntect::{easy::HighlightLines, parsing::SyntaxSet};

use crate::{
    inline::render_inline,
    style::{StyledLine, TextStyle, display_width, wrap_lines},
    table,
};

pub(crate) fn render_markdown(source: &str, width: usize) -> Vec<StyledLine> {
    let mut renderer = MarkdownRenderer::new(width);
    renderer.render(source)
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

    fn render(&mut self, source: &str) -> Vec<StyledLine> {
        let _ = Parser::new_ext(source, markdown_options()).count();

        let lines = source.lines().collect::<Vec<_>>();
        let mut output = Vec::new();
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

            if let Some((level, _)) = heading_at(lines[index])
                && level <= 2
            {
                pad_section_gap(&mut output);
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

        output
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
            .trim()
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
            match self.render_mermaid(&source) {
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

    fn render_mermaid(&self, source: &str) -> Result<Vec<StyledLine>, mermaid_text::Error> {
        let rendered = mermaid_text::render_with_options(
            source,
            &RenderOptions {
                max_width: Some(self.width),
                ascii: false,
                color: false,
                ..RenderOptions::default()
            },
        )?;

        Ok(rendered.lines().map(StyledLine::plain).collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_table_as_box() {
        let source = "| name | value |\n| --- | ---: |\n| alpha | 10 |";
        let output = render_markdown(source, 80)
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
        let lines = render_markdown("---\ntitle: Hello\n---\n# Body", 80);

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
        let lines = render_markdown("- alpha", 80);

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
        let lines = render_markdown("# Title\n\ntext\n\n## Section", 40);

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
        let lines = render_markdown("### Section", 40);

        assert_eq!(display_width(&lines[0].plain_text()), 20);
        assert!(lines[0].plain_text().ends_with('─'));
    }

    #[test]
    fn h4_has_no_rule() {
        let lines = render_markdown("#### Section", 40);

        assert_eq!(lines[0].plain_text(), "#### Section");
    }

    #[test]
    fn skips_rule_when_heading_leaves_no_room() {
        // 区切り空白 1 + 罫線 3 セルが入らない幅では罫線を引かない
        let lines = render_markdown("## abcdef", 12);

        assert!(!lines[0].plain_text().contains('─'));
    }

    #[test]
    fn h2_gets_two_blank_lines_before_it() {
        let texts = render_markdown("body\n\n## Section", 40)
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
        let lines = render_markdown("# Title", 40);

        assert!(lines[0].plain_text().starts_with("# Title"));
    }

    #[test]
    fn preserves_code_block_overflow_for_horizontal_scroll() {
        let source = "```text\nabcdefghijklmnopqrstuvwxyz\n```";
        let lines = render_markdown(source, 10);

        assert!(
            lines
                .iter()
                .any(|line| line.plain_text() == "abcdefghijklmnopqrstuvwxyz")
        );
    }
}

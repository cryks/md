use crossterm::style::{Attribute, Color};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextStyle {
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) underlined: bool,
    pub(crate) reverse: bool,
}

impl TextStyle {
    pub(crate) fn normal() -> Self {
        Self::default()
    }

    pub(crate) fn marker() -> Self {
        Self {
            fg: Some(Color::DarkGrey),
            dim: true,
            ..Self::default()
        }
    }

    pub(crate) fn heading() -> Self {
        Self {
            fg: Some(Color::Yellow),
            bold: true,
            ..Self::default()
        }
    }

    pub(crate) fn quote() -> Self {
        Self {
            fg: Some(Color::Cyan),
            ..Self::default()
        }
    }

    pub(crate) fn code() -> Self {
        Self {
            fg: Some(Color::Green),
            ..Self::default()
        }
    }

    pub(crate) fn link() -> Self {
        Self {
            fg: Some(Color::Blue),
            underlined: true,
            ..Self::default()
        }
    }

    pub(crate) fn list_marker() -> Self {
        Self {
            fg: Some(Color::DarkGrey),
            ..Self::default()
        }
    }

    pub(crate) fn table_border() -> Self {
        Self {
            fg: Some(Color::DarkGrey),
            ..Self::default()
        }
    }

    pub(crate) fn table_header() -> Self {
        Self {
            fg: Some(Color::Cyan),
            bold: true,
            ..Self::default()
        }
    }

    pub(crate) fn error() -> Self {
        Self {
            fg: Some(Color::Red),
            bold: true,
            ..Self::default()
        }
    }

    pub(crate) fn search() -> Self {
        Self {
            reverse: true,
            bold: true,
            ..Self::default()
        }
    }

    pub(crate) fn syntect(fg: syntect::highlighting::Color) -> Self {
        Self {
            fg: Some(Color::Rgb {
                r: fg.r,
                g: fg.g,
                b: fg.b,
            }),
            ..Self::default()
        }
    }

    pub(crate) fn attributes(self) -> impl Iterator<Item = Attribute> {
        [
            self.bold.then_some(Attribute::Bold),
            self.dim.then_some(Attribute::Dim),
            self.underlined.then_some(Attribute::Underlined),
            self.reverse.then_some(Attribute::Reverse),
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: TextStyle,
}

impl StyledSpan {
    pub(crate) fn new(text: impl Into<String>, style: TextStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledLine {
    pub(crate) spans: Vec<StyledSpan>,
}

impl StyledLine {
    pub(crate) fn empty() -> Self {
        Self { spans: Vec::new() }
    }

    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            spans: vec![StyledSpan::new(text, TextStyle::normal())],
        }
    }

    pub(crate) fn styled(text: impl Into<String>, style: TextStyle) -> Self {
        Self {
            spans: vec![StyledSpan::new(text, style)],
        }
    }

    pub(crate) fn push(&mut self, text: impl Into<String>, style: TextStyle) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(StyledSpan::new(text, style));
    }

    pub(crate) fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

pub(crate) fn wrap_lines(lines: &[StyledLine], width: usize) -> Vec<StyledLine> {
    let width = width.max(1);
    let mut wrapped = Vec::new();

    for line in lines {
        if line.spans.is_empty() {
            wrapped.push(StyledLine::empty());
            continue;
        }

        let mut current = StyledLine::empty();
        let mut current_width = 0usize;

        for span in &line.spans {
            for ch in span.text.chars() {
                let ch_width = char_width(ch);
                if current_width > 0 && current_width + ch_width > width {
                    wrapped.push(current);
                    current = StyledLine::empty();
                    current_width = 0;
                }

                current.push(ch.to_string(), span.style);
                current_width += ch_width;
            }
        }

        wrapped.push(current);
    }

    wrapped
}

pub(crate) fn slice_line(line: &StyledLine, left: usize, width: usize) -> StyledLine {
    if width == 0 {
        return StyledLine::empty();
    }

    let right = left.saturating_add(width);
    let mut output = StyledLine::empty();
    let mut column = 0usize;

    for span in &line.spans {
        for ch in span.text.chars() {
            let next_column = column + char_width(ch);

            if next_column <= left {
                column = next_column;
                continue;
            }
            if column >= right {
                return output;
            }
            if column >= left && next_column <= right {
                output.push(ch.to_string(), span.style);
            }

            column = next_column;
        }
    }

    output
}

pub(crate) fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).max(1)
}

pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

pub(crate) fn line_with_search_highlight(line: &StyledLine, query: &str) -> StyledLine {
    if query.is_empty() {
        return line.clone();
    }

    let plain = line.plain_text();
    let ranges = find_match_ranges(&plain, query);
    line_with_style_overlay(line, &ranges, |_| TextStyle::search())
}

/// 全 span の背景を `bg` に置き換える。前景色と属性は保つ。
pub(crate) fn line_with_bg(line: &StyledLine, bg: Color) -> StyledLine {
    StyledLine {
        spans: line
            .spans
            .iter()
            .map(|span| {
                StyledSpan::new(
                    span.text.clone(),
                    TextStyle {
                        bg: Some(bg),
                        ..span.style
                    },
                )
            })
            .collect(),
    }
}

/// plain text への byte range にある部分だけ背景を `bg` に置き換える。
/// 前景色と属性は元の span を保つ。
pub(crate) fn line_with_bg_ranges(
    line: &StyledLine,
    ranges: &[std::ops::Range<usize>],
    bg: Color,
) -> StyledLine {
    line_with_style_overlay(line, ranges, |style| TextStyle {
        bg: Some(bg),
        ..style
    })
}

/// plain text への byte range と重なる部分のスタイルを `overlay` の返り値へ
/// 差し替える。range が span 内の char 境界に合わない場合、その span では
/// 差し替えず元のスタイルを残す。
fn line_with_style_overlay(
    line: &StyledLine,
    ranges: &[std::ops::Range<usize>],
    overlay: impl Fn(TextStyle) -> TextStyle,
) -> StyledLine {
    if ranges.is_empty() {
        return line.clone();
    }

    let mut output = StyledLine::empty();
    let mut offset = 0usize;

    for span in &line.spans {
        let span_start = offset;
        let span_end = span_start + span.text.len();
        let mut cursor = 0usize;

        for range in ranges
            .iter()
            .filter(|range| range.start < span_end && range.end > span_start)
        {
            let start = range.start.saturating_sub(span_start).min(span.text.len());
            let end = range.end.saturating_sub(span_start).min(span.text.len());
            if !span.text.is_char_boundary(start) || !span.text.is_char_boundary(end) {
                continue;
            }

            if cursor < start {
                output.push(&span.text[cursor..start], span.style);
            }
            if start < end {
                output.push(&span.text[start..end], overlay(span.style));
            }
            cursor = end;
        }

        if cursor < span.text.len() {
            output.push(&span.text[cursor..], span.style);
        }

        offset = span_end;
    }

    output
}

fn find_match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let haystack = text.to_lowercase();
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = 0usize;

    while let Some(found) = haystack[start..].find(&needle) {
        let absolute = start + found;
        let end = absolute + needle.len();
        ranges.push(absolute..end);
        start = end;
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_line_by_display_columns() {
        let line = StyledLine::plain("abcdef");

        assert_eq!(slice_line(&line, 2, 3).plain_text(), "cde");
    }

    #[test]
    fn bg_overlay_keeps_fg_and_splits_spans() {
        let line = StyledLine::styled("hello world", TextStyle::code());

        let output = line_with_bg_ranges(&line, std::slice::from_ref(&(6..11)), Color::Red);

        assert_eq!(output.plain_text(), "hello world");
        assert_eq!(output.spans.len(), 2);
        assert_eq!(output.spans[0].style.bg, None);
        assert_eq!(output.spans[1].text, "world");
        assert_eq!(output.spans[1].style.bg, Some(Color::Red));
        assert_eq!(output.spans[1].style.fg, TextStyle::code().fg);
    }

    #[test]
    fn line_bg_replaces_every_span_background() {
        let mut line = StyledLine::styled("a", TextStyle::code());
        line.push("b", TextStyle::heading());

        let output = line_with_bg(&line, Color::Blue);

        assert!(
            output
                .spans
                .iter()
                .all(|span| span.style.bg == Some(Color::Blue))
        );
        assert_eq!(output.spans[1].style.fg, TextStyle::heading().fg);
        assert!(output.spans[1].style.bold);
    }
}

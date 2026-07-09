use crate::{
    inline::render_inline_with_base,
    style::{StyledLine, TextStyle, display_width},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Table {
    rows: Vec<Vec<String>>,
    aligns: Vec<Align>,
}

pub(crate) fn table_at(lines: &[&str], start: usize) -> Option<usize> {
    if start + 1 >= lines.len() {
        return None;
    }
    let header = split_row(lines[start])?;
    let separator = split_row(lines[start + 1])?;
    if header.is_empty()
        || separator.is_empty()
        || !separator.iter().all(|cell| align_cell(cell).is_some())
    {
        return None;
    }

    let mut end = start + 2;
    while end < lines.len() && split_row(lines[end]).is_some() {
        end += 1;
    }
    Some(end)
}

pub(crate) fn render_table(lines: &[&str], max_width: usize) -> Vec<StyledLine> {
    let table = parse_table(lines);
    let target_widths = column_widths(&table, max_width.max(12));
    let wrapped_rows = wrap_rows(&table, &target_widths);
    let widths = effective_column_widths(&target_widths, &wrapped_rows);
    let mut output = Vec::new();

    output.push(border("┌", "┬", "┐", &widths));
    if let Some(header) = wrapped_rows.first() {
        output.extend(render_row(header, &widths, &table.aligns));
    }
    output.push(border("├", "┼", "┤", &widths));
    for (row_index, row) in wrapped_rows.iter().skip(1).enumerate() {
        output.extend(render_row(row, &widths, &table.aligns));
        if row_index + 2 < wrapped_rows.len() {
            output.push(border("├", "┼", "┤", &widths));
        }
    }
    output.push(border("└", "┴", "┘", &widths));

    output
}

#[derive(Clone, Debug)]
struct WrappedRow {
    cells: Vec<Vec<StyledLine>>,
    header: bool,
}

fn parse_table(lines: &[&str]) -> Table {
    let header = split_row(lines[0]).unwrap_or_default();
    let separator = split_row(lines[1]).unwrap_or_default();
    let aligns = separator
        .iter()
        .map(|cell| align_cell(cell).unwrap_or_default())
        .collect::<Vec<_>>();

    let column_count = header.len().max(aligns.len());
    let mut rows = Vec::new();
    rows.push(normalize_row(header, column_count));

    for line in lines.iter().skip(2) {
        if let Some(row) = split_row(line) {
            rows.push(normalize_row(row, column_count));
        }
    }

    Table {
        rows,
        aligns: normalize_aligns(aligns, column_count),
    }
}

fn split_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }

    let trimmed = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed);

    let cells = trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect::<Vec<_>>();

    (!cells.is_empty()).then_some(cells)
}

fn align_cell(cell: &str) -> Option<Align> {
    let trimmed = cell.trim();
    let left = trimmed.starts_with(':');
    let right = trimmed.ends_with(':');
    let core = trimmed.trim_matches(':').trim();

    if core.len() >= 3 && core.chars().all(|ch| ch == '-') {
        Some(match (left, right) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
    } else {
        None
    }
}

fn normalize_row(mut row: Vec<String>, column_count: usize) -> Vec<String> {
    row.resize(column_count, String::new());
    row.truncate(column_count);
    row
}

fn normalize_aligns(mut aligns: Vec<Align>, column_count: usize) -> Vec<Align> {
    aligns.resize(column_count, Align::Left);
    aligns.truncate(column_count);
    aligns
}

fn column_widths(table: &Table, max_width: usize) -> Vec<usize> {
    let column_count = table.rows.first().map_or(0, Vec::len);
    if column_count == 0 {
        return Vec::new();
    }

    let mut widths = vec![3usize; column_count];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
    }

    let border_and_padding = column_count + 1 + column_count * 2;
    let available = max_width
        .saturating_sub(border_and_padding)
        .max(column_count * 3);
    let natural: usize = widths.iter().sum();
    if natural <= available {
        return widths;
    }

    let mut remaining = available;
    let mut shrunk = vec![3usize; column_count];
    for (index, width) in widths.iter().enumerate() {
        let columns_left = column_count - index;
        let fair_share = remaining / columns_left;
        let chosen = (*width).min(fair_share.max(3));
        shrunk[index] = chosen;
        remaining = remaining.saturating_sub(chosen);
    }

    shrunk
}

fn border(left: &str, middle: &str, right: &str, widths: &[usize]) -> StyledLine {
    let mut line = StyledLine::empty();
    line.push(left, TextStyle::table_border());
    for (index, width) in widths.iter().enumerate() {
        line.push("─".repeat(width + 2), TextStyle::table_border());
        if index + 1 == widths.len() {
            line.push(right, TextStyle::table_border());
        } else {
            line.push(middle, TextStyle::table_border());
        }
    }
    line
}

fn wrap_rows(table: &Table, widths: &[usize]) -> Vec<WrappedRow> {
    table
        .rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let header = row_index == 0;
            let cells = row
                .iter()
                .zip(widths)
                .map(|(cell, width)| {
                    wrap_cell(
                        cell,
                        *width,
                        if header {
                            TextStyle::table_header()
                        } else {
                            TextStyle::normal()
                        },
                    )
                })
                .collect();
            WrappedRow { cells, header }
        })
        .collect()
}

fn effective_column_widths(target_widths: &[usize], rows: &[WrappedRow]) -> Vec<usize> {
    let mut widths = target_widths.to_vec();
    for row in rows {
        for (cell_index, cell_lines) in row.cells.iter().enumerate() {
            for line in cell_lines {
                widths[cell_index] = widths[cell_index].max(display_width(&line.plain_text()));
            }
        }
    }
    widths
}

fn render_row(row: &WrappedRow, widths: &[usize], aligns: &[Align]) -> Vec<StyledLine> {
    let height = row.cells.iter().map(Vec::len).max().unwrap_or(1);
    let mut output = Vec::new();

    for line_index in 0..height {
        let mut line = StyledLine::empty();
        line.push("│", TextStyle::table_border());
        for (cell_index, width) in widths.iter().enumerate() {
            let content = row.cells[cell_index]
                .get(line_index)
                .cloned()
                .unwrap_or_else(StyledLine::empty);
            let aligned = align_line(
                content,
                *width,
                aligns[cell_index],
                if row.header {
                    TextStyle::table_header()
                } else {
                    TextStyle::normal()
                },
            );
            line.push(" ", TextStyle::table_border());
            line.spans.extend(aligned.spans);
            line.push(" │", TextStyle::table_border());
        }
        output.push(line);
    }

    output
}

fn wrap_cell(cell: &str, width: usize, base_style: TextStyle) -> Vec<StyledLine> {
    let inline = render_inline_with_base(cell, base_style);
    wrap_cell_line(&inline, width.max(1))
}

#[derive(Clone, Debug)]
struct TextUnit {
    text: String,
    style: TextStyle,
    width: usize,
}

fn wrap_cell_line(line: &StyledLine, width: usize) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    let mut current = Vec::<TextUnit>::new();
    let mut current_width = 0usize;

    for unit in line_units(line) {
        if current_width > 0 && current_width + unit.width > width {
            if !current.is_empty() {
                lines.push(units_to_line(current));
            }

            current = Vec::new();
            current_width = 0;
        }

        if current.is_empty() && unit.text.chars().all(char::is_whitespace) {
            continue;
        }

        current_width += unit.width;
        current.push(unit);
    }

    if current.is_empty() {
        lines.push(StyledLine::empty());
    } else {
        lines.push(units_to_line(current));
    }

    lines
}

fn line_units(line: &StyledLine) -> Vec<TextUnit> {
    let mut units = Vec::new();

    for span in &line.spans {
        let mut chars = span.text.chars().peekable();
        while let Some(ch) = chars.next() {
            let mut text = String::new();
            text.push(ch);

            if english_word_char(ch) {
                while chars.peek().is_some_and(|next| english_word_char(*next)) {
                    text.push(chars.next().expect("peeked char exists"));
                }
            }

            units.push(TextUnit {
                width: display_width(&text),
                text,
                style: span.style,
            });
        }
    }

    units
}

fn units_to_line(units: Vec<TextUnit>) -> StyledLine {
    let mut line = StyledLine::empty();
    for unit in units {
        line.push(unit.text, unit.style);
    }
    line
}

fn english_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '-')
}

fn align_line(
    mut line: StyledLine,
    width: usize,
    align: Align,
    fill_style: TextStyle,
) -> StyledLine {
    let text_width = display_width(&line.plain_text());
    if text_width >= width {
        return line;
    }

    let padding = width - text_width;
    match align {
        Align::Left => {
            line.push(" ".repeat(padding), fill_style);
            line
        }
        Align::Right => {
            let mut output = StyledLine::empty();
            output.push(" ".repeat(padding), fill_style);
            output.spans.extend(line.spans);
            output
        }
        Align::Center => {
            let left = padding / 2;
            let right = padding - left;
            let mut output = StyledLine::empty();
            output.push(" ".repeat(left), fill_style);
            output.spans.extend(line.spans);
            output.push(" ".repeat(right), fill_style);
            output
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown_table() {
        let lines = ["| name | value |", "| --- | ---: |", "| alpha | 10 |"];
        assert_eq!(table_at(&lines, 0), Some(3));
    }

    #[test]
    fn renders_unicode_table() {
        let lines = [
            "| name | value |",
            "| --- | ---: |",
            "| alpha | 10 |",
            "| beta | 20 |",
        ];
        let rendered = render_table(&lines, 80)
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("┌"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("10"));
        assert_eq!(rendered.matches('├').count(), 2);
    }

    #[test]
    fn renders_inline_markdown_in_cells() {
        let lines = ["| name |", "| --- |", "| `foobar` |"];
        let rendered = render_table(&lines, 80);

        assert!(
            rendered
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.text == "foobar" && span.style == TextStyle::code())
        );
    }

    #[test]
    fn keeps_table_width_consistent_when_wrapped_cell_width_changes() {
        let lines = ["| text |", "| --- |", "| あいう、えお |"];
        let rendered = render_table(&lines, 10);
        let widths = rendered
            .iter()
            .map(|line| display_width(&line.plain_text()))
            .collect::<Vec<_>>();

        assert!(widths.iter().all(|width| *width == widths[0]));
    }

    #[test]
    fn keeps_english_words_unsplit_in_cells() {
        let line = render_inline_with_base("supercalifragilistic", TextStyle::normal());
        let wrapped = wrap_cell_line(&line, 6);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].plain_text(), "supercalifragilistic");
    }

    #[test]
    fn does_not_apply_japanese_kinsoku_in_cells() {
        let line = render_inline_with_base("あいう、えお", TextStyle::normal());
        let wrapped = wrap_cell_line(&line, 6)
            .into_iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>();

        assert_eq!(wrapped[0], "あいう");
        assert_eq!(wrapped[1], "、えお");
    }
}

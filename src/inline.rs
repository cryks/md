use crate::style::{StyledLine, TextStyle};

pub(crate) fn render_inline(text: &str) -> StyledLine {
    render_inline_with_base(text, TextStyle::normal())
}

pub(crate) fn render_inline_with_base(text: &str, base_style: TextStyle) -> StyledLine {
    let mut line = StyledLine::empty();
    let mut index = 0usize;

    while index < text.len() {
        let rest = &text[index..];

        if let Some(end) = rest.strip_prefix('`').and_then(|tail| tail.find('`')) {
            let content_end = index + 1 + end;
            line.push("`", TextStyle::marker());
            line.push(&text[index + 1..content_end], TextStyle::code());
            line.push("`", TextStyle::marker());
            index = content_end + 1;
            continue;
        }

        if rest.starts_with('[')
            && let Some(close_bracket) = rest.find("](")
            && let Some(close_paren) = rest[close_bracket + 2..].find(')')
        {
            let text_start = index + 1;
            let text_end = index + close_bracket;
            let url_start = text_end + 2;
            let url_end = url_start + close_paren;
            line.push("[", TextStyle::marker());
            line.push(&text[text_start..text_end], TextStyle::link());
            line.push("](", TextStyle::marker());
            line.push(&text[url_start..url_end], TextStyle::link());
            line.push(")", TextStyle::marker());
            index = url_end + 1;
            continue;
        }

        if let Some(end) = rest.strip_prefix("**").and_then(|tail| tail.find("**")) {
            let content_end = index + 2 + end;
            line.push("**", TextStyle::marker());
            line.push(&text[index + 2..content_end], TextStyle::heading());
            line.push("**", TextStyle::marker());
            index = content_end + 2;
            continue;
        }

        let mut chars = rest.char_indices();
        let (_, ch) = chars.next().expect("rest is non-empty");
        let next = chars
            .next()
            .map(|(offset, _)| index + offset)
            .unwrap_or(text.len());
        line.push(ch.to_string(), base_style);
        index = next;
    }

    line
}

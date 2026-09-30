//! Rich previews: Markdown rendered with text styles, and CSV/TSV as a table.

use gtk::prelude::*;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Creates the text tags [`markdown`] uses.
pub fn create_tags(buffer: &gtk::TextBuffer) {
    let tag = |name: &str, props: &[(&str, &dyn ToValue)]| {
        buffer.create_tag(Some(name), props);
    };
    tag("h1", &[("scale", &1.6), ("weight", &800), ("pixels-above-lines", &10), ("pixels-below-lines", &4)]);
    tag("h2", &[("scale", &1.35), ("weight", &700), ("pixels-above-lines", &8), ("pixels-below-lines", &3)]);
    tag("h3", &[("scale", &1.15), ("weight", &700), ("pixels-above-lines", &6), ("pixels-below-lines", &2)]);
    tag("bold", &[("weight", &700)]);
    tag("italic", &[("style", &gtk::pango::Style::Italic)]);
    tag("strike", &[("strikethrough", &true)]);
    tag("code", &[("family", &"monospace"), ("background-rgba", &gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 0.15))]);
    tag("quote", &[("left-margin", &36), ("style", &gtk::pango::Style::Italic), ("foreground-rgba", &gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0))]);
    tag("link", &[("underline", &gtk::pango::Underline::Single), ("foreground-rgba", &gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0))]);
    tag("item", &[("left-margin", &28)]);
}

/// Renders Markdown into `buffer`, replacing its content.
pub fn markdown(buffer: &gtk::TextBuffer, source: &str) {
    buffer.set_text("");
    let mut styles: Vec<&'static str> = Vec::new();
    // Next number of each open list; `None` for bullet lists.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let insert = |text: &str, styles: &[&str]| {
        let mut end = buffer.end_iter();
        buffer.insert_with_tags_by_name(&mut end, text, styles);
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => styles.push(match level {
                    HeadingLevel::H1 => "h1",
                    HeadingLevel::H2 => "h2",
                    _ => "h3",
                }),
                Tag::Emphasis => styles.push("italic"),
                Tag::Strong => styles.push("bold"),
                Tag::Strikethrough => styles.push("strike"),
                Tag::CodeBlock(_) => styles.push("code"),
                Tag::BlockQuote(_) => styles.push("quote"),
                Tag::Link { .. } => styles.push("link"),
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            *n += 1;
                            format!("{}. ", *n - 1)
                        }
                        _ => "•  ".to_owned(),
                    };
                    insert(&format!("{}{marker}", "    ".repeat(lists.len().saturating_sub(1))), &["item"]);
                    styles.push("item");
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::BlockQuote(_) => {
                    if !matches!(end, TagEnd::Paragraph) {
                        styles.pop();
                    }
                    insert(if lists.is_empty() { "\n\n" } else { "\n" }, &[]);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    styles.pop();
                }
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        insert("\n", &[]);
                    }
                }
                TagEnd::Item => {
                    styles.pop();
                    if !buffer.end_iter().starts_line() {
                        insert("\n", &[]);
                    }
                }
                TagEnd::TableCell => insert("\t", &[]),
                TagEnd::TableRow | TagEnd::TableHead => insert("\n", &[]),
                TagEnd::Table => insert("\n", &[]),
                _ => {}
            },
            Event::Text(text) => insert(&text, &styles),
            Event::Code(code) => insert(&code, &["code"]),
            Event::SoftBreak => insert(" ", &styles),
            Event::HardBreak => insert("\n", &styles),
            Event::Rule => insert("──────────\n\n", &[]),
            Event::TaskListMarker(done) => insert(if done { "☑ " } else { "☐ " }, &styles),
            _ => {}
        }
    }
}

/// Parses comma- or tab-separated text into rows of cells, handling quoted cells.
/// Stops after `max_rows` rows.
pub fn parse_delimited(text: &str, separator: char, max_rows: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' if quoted => quoted = false,
            '"' if cell.is_empty() => quoted = true,
            c if c == separator && !quoted => row.push(std::mem::take(&mut cell)),
            '\n' if !quoted => {
                row.push(std::mem::take(&mut cell).trim_end_matches('\r').to_owned());
                rows.push(std::mem::take(&mut row));
                if rows.len() == max_rows {
                    return rows;
                }
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimited() {
        let rows = parse_delimited("name,note\r\n\"Doe, Jane\",\"says \"\"hi\"\"\"\nx,\"multi\nline\"\n", ',', 10);
        assert_eq!(rows, [vec!["name", "note"], vec!["Doe, Jane", "says \"hi\""], vec!["x", "multi\nline"]]);
        assert_eq!(parse_delimited("a\tb\nc\td", '\t', 1), [vec!["a", "b"]]);
    }
}

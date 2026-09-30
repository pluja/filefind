//! Search syntax.
//!
//! | Input            | Meaning                                           |
//! |------------------|---------------------------------------------------|
//! | `word`           | word in the name, folder or content (typo-tolerant) |
//! | `"two words"`    | exact phrase                                      |
//! | `-word`          | exclude files containing the word                 |
//! | `type:pdf`       | category (pdf, doc, sheet, slides, text, image…) or extension |
//! | `in:taxes`       | folder name                                       |
//! | `name:invoice`   | word in the file name only                        |
//!
//! Spanish keywords work too: `tipo:`, `en:`, `nombre:`.

use crate::category::{Category, TypeFilter};

#[derive(Debug, Default, PartialEq)]
pub struct ParsedQuery {
    pub words: Vec<String>,
    pub phrases: Vec<String>,
    pub name_words: Vec<String>,
    pub excluded: Vec<String>,
    pub types: Vec<TypeFilter>,
    pub folders: Vec<String>,
    /// The last loose word is still being typed, so it should also match as a prefix.
    pub typing_last: bool,
}

impl ParsedQuery {
    /// Whether the query names anything to look for (as opposed to only filters or exclusions).
    pub fn has_terms(&self) -> bool {
        !(self.words.is_empty() && self.phrases.is_empty() && self.name_words.is_empty() && self.folders.is_empty())
    }

    pub fn is_empty(&self) -> bool {
        !self.has_terms() && self.types.is_empty() && self.excluded.is_empty()
    }
}

struct Token {
    text: String,
    quoted: bool,
}

/// Splits on whitespace, keeping "quoted text" (optionally prefixed, as in `-"a b"`) together.
fn tokens(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() || quoted {
                    out.push(Token { text: std::mem::take(&mut current), quoted });
                }
                quoted = false;
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() || quoted {
        out.push(Token { text: current, quoted });
    }
    out
}

pub fn parse(input: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let tokens = tokens(input);
    let last = tokens.len().saturating_sub(1);
    for (i, token) in tokens.into_iter().enumerate() {
        let text = token.text.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(rest) = text.strip_prefix('-').filter(|r| !r.is_empty()) {
            q.excluded.push(rest.to_owned());
            continue;
        }
        if let Some((key, value)) = text.split_once(':') {
            let value = value.trim();
            let handled = match key.to_lowercase().as_str() {
                "type" | "tipo" | "ext" => {
                    q.types.extend(Category::parse(value));
                    true
                }
                "in" | "en" => {
                    if !value.is_empty() {
                        q.folders.push(value.to_owned());
                    }
                    true
                }
                "name" | "nombre" => {
                    if !value.is_empty() {
                        q.name_words.push(value.to_owned());
                    }
                    true
                }
                _ => false,
            };
            if handled {
                continue;
            }
        }
        if token.quoted {
            q.phrases.push(text.to_owned());
        } else {
            q.words.push(text.to_owned());
            q.typing_last = i == last;
        }
    }
    if input.ends_with(char::is_whitespace) {
        q.typing_last = false;
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax() {
        let q = parse(r#"invoice "net total" -draft type:pdf in:taxes name:acme 2024"#);
        assert_eq!(q.words, ["invoice", "2024"]);
        assert_eq!(q.phrases, ["net total"]);
        assert_eq!(q.excluded, ["draft"]);
        assert_eq!(q.types, [TypeFilter::Category(Category::Pdf)]);
        assert_eq!(q.folders, ["taxes"]);
        assert_eq!(q.name_words, ["acme"]);
        assert!(q.typing_last);
    }

    #[test]
    fn typing_state() {
        assert!(parse("budg").typing_last);
        assert!(!parse("budget ").typing_last);
        assert!(!parse("budget \"q1").typing_last);
        assert_eq!(parse("budget \"q1").phrases, ["q1"]);
    }

    #[test]
    fn edge_cases() {
        // Unknown keys and times are ordinary words; empty filters are ignored while typing.
        assert_eq!(parse("10:30 http://x").words, ["10:30", "http://x"]);
        assert!(parse("type:").is_empty());
        assert_eq!(parse("-\"old draft\"").excluded, ["old draft"]);
        assert_eq!(parse("tipo:fotos en:viajes").folders, ["viajes"]);
        assert!(parse("  ").is_empty());
    }
}

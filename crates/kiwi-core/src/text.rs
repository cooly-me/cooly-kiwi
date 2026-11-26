//! Text normalisation shared by the compiler, the planner and the plugins.
//!
//! Every lookup key (ids, glossary terms, aliases, titles) is normalised the same way
//! so that `ADR-005`, `adr-005` and `Adr-005` resolve to one entry, and Vietnamese
//! diacritics fold to ASCII (`Thanh toán` and `thanh toan` match).

use unicode_normalization::UnicodeNormalization;

/// Folds a string to lowercase ASCII-ish text: diacritics are removed, `đ` becomes `d`,
/// and runs of whitespace collapse to one space. Punctuation is kept.
pub fn fold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;
    for ch in input.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        let ch = match ch {
            'đ' | 'Đ' => 'd',
            other => other,
        };
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
        last_space = false;
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The normalised form used as a key in the exact-match dictionary.
pub fn key(input: &str) -> String {
    fold(input.trim())
}

fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// GitHub-style heading slug over folded text: lowercase ASCII letters, digits and
/// hyphens; spaces become hyphens; other punctuation is dropped.
pub fn slugify(input: &str) -> String {
    let folded = fold(input);
    let mut out = String::with_capacity(folded.len());
    for ch in folded.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if (ch == ' ' || ch == '-' || ch == '_') && !out.ends_with('-') {
            out.push('-');
        } else if ch.is_alphanumeric() {
            out.push(ch);
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "section".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Splits folded text into word tokens. A token is a run of letters and digits that may
/// contain the inner connectors `-`, `/`, `#`, `.` and `_`, so ids such as `adr-005` or
/// `product/checkout/prd#pricing` stay whole.
pub fn words(folded: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = folded.as_bytes();
    let mut start: Option<usize> = None;
    for (i, ch) in folded.char_indices() {
        let connector = matches!(ch, '-' | '/' | '#' | '.' | '_');
        if ch.is_alphanumeric() || (connector && start.is_some()) {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            push_word(&mut tokens, folded, s, i);
        }
    }
    if let Some(s) = start {
        push_word(&mut tokens, folded, s, bytes.len());
    }
    tokens
}

fn push_word<'a>(tokens: &mut Vec<&'a str>, text: &'a str, start: usize, end: usize) {
    let word = text[start..end].trim_end_matches(['-', '/', '#', '.', '_']);
    if !word.is_empty() {
        tokens.push(word);
    }
}

/// Returns the first sentence of a paragraph of plain text, bounded to `max_chars`
/// characters (cut at a word boundary with an ellipsis when longer).
pub fn first_sentence(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let mut end = text.len();
    let mut prev_is_terminal = false;
    for (i, ch) in text.char_indices() {
        if prev_is_terminal && ch.is_whitespace() {
            end = i;
            break;
        }
        prev_is_terminal = matches!(ch, '.' | '!' | '?' | '。');
    }
    truncate_chars(text[..end].trim(), max_chars)
}

/// Truncates to at most `max_chars` characters at a word boundary, adding `…` when cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    if let Some(space) = cut.rfind(' ') {
        cut.truncate(space);
    }
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_removes_diacritics_and_case() {
        assert_eq!(fold("Thanh  Toán Trả Góp"), "thanh toan tra gop");
        assert_eq!(fold("Đường"), "duong");
        assert_eq!(key("  ADR-005 "), "adr-005");
    }

    #[test]
    fn slug_matches_github_style() {
        assert_eq!(slugify("Payment methods"), "payment-methods");
        assert_eq!(slugify("Context & goals!"), "context-goals");
        assert_eq!(slugify("Giá bán"), "gia-ban");
        assert_eq!(slugify("!!!"), "section");
    }

    #[test]
    fn words_keep_ids_whole() {
        let folded = fold("See ADR-005, and product/checkout/prd#pricing. Done.");
        assert_eq!(
            words(&folded),
            vec!["see", "adr-005", "and", "product/checkout/prd#pricing", "done"]
        );
    }

    #[test]
    fn first_sentence_stops_at_terminal() {
        assert_eq!(first_sentence("One two. Three four.", 100), "One two.");
        assert_eq!(first_sentence("No terminal here", 100), "No terminal here");
        assert_eq!(first_sentence("alpha beta gamma delta", 12), "alpha beta…");
    }
}

//! The text analyzer shared by indexing and querying.
//!
//! A token is a run of alphanumeric characters (any script). Tokens of 40 bytes or more
//! are dropped, the rest are lowercased, folded to ASCII where a folding exists (so
//! Vietnamese `thanh toán` and `thanh toan` index alike) and stemmed with the Snowball
//! English stemmer. The same analysis runs on atoms and on queries.

use std::borrow::Cow;

use frostem::{Algorithm, Stemmer};

use crate::fold_table::FOLD;

/// Bumped whenever the analysis changes, so a build re-analyses every atom instead of
/// reusing token lists from an older analyzer.
pub const ANALYZER_VERSION: u32 = 1;

/// Tokens of this many bytes or more are dropped (hashes, base64, long URLs).
const MAX_TOKEN_BYTES: usize = 40;

/// A reusable analyzer (it owns the stemmer and a scratch buffer).
pub struct Analyzer {
    stemmer: Stemmer,
    lower: String,
    folded: String,
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Analyzer")
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    /// A new analyzer.
    pub fn new() -> Self {
        Self {
            stemmer: Stemmer::new(Algorithm::English),
            lower: String::new(),
            folded: String::new(),
        }
    }

    /// Calls `emit` with every term of `text`, in order (repeated terms repeat).
    pub fn terms(&mut self, text: &str, mut emit: impl FnMut(&str)) {
        let mut start: Option<usize> = None;
        for (offset, ch) in text.char_indices() {
            if ch.is_alphanumeric() {
                if start.is_none() {
                    start = Some(offset);
                }
            } else if let Some(begin) = start.take() {
                self.token(&text[begin..offset], &mut emit);
            }
        }
        if let Some(begin) = start {
            self.token(&text[begin..], &mut emit);
        }
    }

    /// Every term of `text`, in order.
    #[cfg(test)]
    pub fn collect(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.terms(text, |term| out.push(term.to_owned()));
        out
    }

    fn token(&mut self, raw: &str, emit: &mut impl FnMut(&str)) {
        if raw.len() >= MAX_TOKEN_BYTES {
            return;
        }
        self.lower.clear();
        if raw.is_ascii() {
            self.lower.push_str(raw);
            self.lower.make_ascii_lowercase();
        } else {
            for ch in raw.chars() {
                self.lower.extend(ch.to_lowercase());
            }
        }
        let folded: &str = if self.lower.is_ascii() {
            &self.lower
        } else {
            self.folded.clear();
            for ch in self.lower.chars() {
                match FOLD.binary_search_by_key(&ch, |(from, _)| *from) {
                    Ok(index) => self.folded.push_str(FOLD[index].1),
                    Err(_) => self.folded.push(ch),
                }
            }
            // Some lowercase letters fold to capitals (small capitals such as `ᴀ`, and
            // `ǥ`), so the folded form is lowercased again.
            self.folded.make_ascii_lowercase();
            &self.folded
        };
        match self.stemmer.stem(folded) {
            Cow::Borrowed(term) => emit(term),
            Cow::Owned(term) => emit(&term),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_diacritics_and_stems() {
        let mut analyzer = Analyzer::new();
        assert_eq!(
            analyzer.collect("Thanh toán trả góp, ĐƯỜNG payments!"),
            vec!["thanh", "toan", "tra", "gop", "duong", "payment"]
        );
        assert_eq!(
            analyzer.collect("Idempotency-Keys ADR-005"),
            vec!["idempot", "key", "adr", "005"]
        );
        assert_eq!(analyzer.collect("Straße Æsir"), vec!["strass", "aesir"]);
        assert_eq!(analyzer.collect("ǥrid ᴀlpha"), analyzer.collect("grid alpha"));
    }

    #[test]
    fn drops_long_tokens_and_keeps_other_scripts() {
        let mut analyzer = Analyzer::new();
        let long = "a".repeat(40);
        assert_eq!(
            analyzer.collect(&format!("short {long} end")),
            vec!["short", "end"]
        );
        assert_eq!(analyzer.collect("決済 платеж"), vec!["決済", "платеж"]);
    }

    #[test]
    fn the_table_is_sorted() {
        assert!(FOLD.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }
}

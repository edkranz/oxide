//! Find and replace over a document's text.

use regex::{Regex, RegexBuilder};

/// Stop collecting matches past this many; the count is shown as "N+".
pub const MATCH_LIMIT: usize = 100_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FindOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
}

/// A compiled query. Literal queries are escaped, so one code path serves both modes.
pub struct Search {
    re: Regex,
    regex_mode: bool,
}

impl Search {
    /// `Ok(None)` for an empty query; `Err` holds a regex error to show the user.
    pub fn new(query: &str, options: FindOptions) -> Result<Option<Self>, String> {
        if query.is_empty() {
            return Ok(None);
        }
        let mut pattern = if options.regex { query.to_string() } else { regex::escape(query) };
        if options.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .build()
            .map_err(|e| e.to_string().lines().last().unwrap_or("invalid pattern").trim().to_string())?;
        Ok(Some(Self { re, regex_mode: options.regex }))
    }

    /// Non-empty matches as char ranges, in order.
    pub fn find_all(&self, text: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let (mut byte, mut chars) = (0, 0);
        for m in self.re.find_iter(text) {
            if m.is_empty() {
                continue;
            }
            chars += text[byte..m.start()].chars().count();
            let start = chars;
            chars += m.as_str().chars().count();
            byte = m.end();
            out.push((start, chars));
            if out.len() >= MATCH_LIMIT {
                break;
            }
        }
        out
    }

    /// The replacement for the match spanning bytes `start..end` of `text`, if there is one.
    /// In regex mode `$1`/`${name}` expand to capture groups.
    pub fn replacement_at(&self, text: &str, start: usize, end: usize, with: &str) -> Option<String> {
        let caps = self.re.captures_at(text, start)?;
        let m = caps.get(0)?;
        if (m.start(), m.end()) != (start, end) {
            return None;
        }
        Some(self.expand(&caps, with))
    }

    /// Every match with its replacement, as char ranges.
    pub fn replace_all(&self, text: &str, with: &str) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        let (mut byte, mut chars) = (0, 0);
        for caps in self.re.captures_iter(text) {
            let m = caps.get(0).unwrap();
            if m.is_empty() {
                continue;
            }
            chars += text[byte..m.start()].chars().count();
            let start = chars;
            chars += m.as_str().chars().count();
            byte = m.end();
            out.push((start, chars, self.expand(&caps, with)));
        }
        out
    }

    fn expand(&self, caps: &regex::Captures, with: &str) -> String {
        if !self.regex_mode {
            return with.to_string();
        }
        let mut out = String::new();
        caps.expand(with, &mut out);
        out
    }
}

/// The match to select when searching forward from `pos`, wrapping at the end.
pub fn next_match(matches: &[(usize, usize)], pos: usize) -> Option<(usize, usize)> {
    let i = matches.partition_point(|m| m.0 < pos);
    matches.get(i).or(matches.first()).copied()
}

/// The match to select when searching backward from `pos`, wrapping at the start.
pub fn prev_match(matches: &[(usize, usize)], pos: usize) -> Option<(usize, usize)> {
    let i = matches.partition_point(|m| m.1 <= pos);
    i.checked_sub(1).and_then(|i| matches.get(i)).or(matches.last()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(query: &str, options: FindOptions) -> Search {
        Search::new(query, options).unwrap().unwrap()
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let s = search("a.b", FindOptions::default());
        assert_eq!(s.find_all("A.B axb a.b"), vec![(0, 3), (8, 11)]);
    }

    #[test]
    fn ranges_are_in_chars_not_bytes() {
        let s = search("x", FindOptions::default());
        assert_eq!(s.find_all("héllo x ñ x"), vec![(6, 7), (10, 11)]);
    }

    #[test]
    fn whole_word_and_regex_modes() {
        let options = FindOptions { whole_word: true, ..Default::default() };
        assert_eq!(search("cat", options).find_all("cat concat cat"), vec![(0, 3), (11, 14)]);
        let options = FindOptions { regex: true, ..Default::default() };
        assert_eq!(search(r"\d+", options).find_all("a1 b22"), vec![(1, 2), (4, 6)]);
        assert!(Search::new("(", options).is_err());
        assert!(Search::new("", options).unwrap().is_none());
    }

    #[test]
    fn replacement_expands_captures_only_in_regex_mode() {
        let regex = FindOptions { regex: true, ..Default::default() };
        let s = search(r"(\w+)@(\w+)", regex);
        assert_eq!(s.replace_all("a@b c@d", "$2@$1"), vec![(0, 3, "b@a".into()), (4, 7, "d@c".into())]);
        assert_eq!(s.replacement_at("a@b c@d", 4, 7, "$2"), Some("d".into()));
        assert_eq!(s.replacement_at("a@b c@d", 5, 7, "$2"), None);
        let literal = search("$x", FindOptions::default());
        assert_eq!(literal.replace_all("$x", "$1"), vec![(0, 2, "$1".into())]);
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let m = [(2, 4), (10, 12)];
        assert_eq!(next_match(&m, 0), Some((2, 4)));
        assert_eq!(next_match(&m, 4), Some((10, 12)));
        assert_eq!(next_match(&m, 11), Some((2, 4)));
        assert_eq!(prev_match(&m, 10), Some((2, 4)));
        assert_eq!(prev_match(&m, 2), Some((10, 12)));
        assert_eq!(next_match(&[], 0), None);
    }
}

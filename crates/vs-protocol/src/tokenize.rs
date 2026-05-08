//! Quote-aware tokenizer shared by the tree, delta, and request parsers.
//!
//! The wire format treats whitespace as a token boundary, except inside
//! a `"..."` span. The tokenizer therefore emits one token per
//! whitespace-separated chunk, but preserves quoted spans verbatim
//! (including the surrounding quotes) so the caller can decide whether
//! the quotes were significant. Inner quotes are NOT escaped on the
//! wire: encoders substitute `'` for `"` inside labels and attribute
//! values; this tokenizer never has to handle escapes.

#[derive(Debug, Clone)]
pub(crate) struct Tokenizer<'a> {
    rest: &'a str,
}

impl<'a> Tokenizer<'a> {
    pub(crate) fn new(s: &'a str) -> Self {
        Self { rest: s }
    }

    /// The unconsumed remainder of the input, with leading whitespace
    /// retained. Used by parsers that hand the tail back to a caller.
    #[allow(dead_code)]
    pub(crate) fn rest(&self) -> &'a str {
        self.rest
    }

    /// True if the remaining input is empty or whitespace only.
    #[allow(dead_code)]
    pub(crate) fn is_done(&self) -> bool {
        self.rest.bytes().all(|b| b == b' ' || b == b'\t')
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let trimmed = trim_start_spaces(self.rest);
        if trimmed.is_empty() {
            self.rest = "";
            return None;
        }
        let bytes = trimmed.as_bytes();
        let mut end = 0;
        let mut in_quotes = false;
        while end < bytes.len() {
            let b = bytes[end];
            if in_quotes {
                if b == b'"' {
                    in_quotes = false;
                }
            } else if b == b'"' {
                in_quotes = true;
            } else if b == b' ' || b == b'\t' {
                break;
            }
            end += 1;
        }
        let raw = &trimmed[..end];
        self.rest = &trimmed[end..];
        Some(raw)
    }
}

/// Strip a single layer of `"` from `s` if it is fully bare-quoted.
pub(crate) fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Bare-quote `s` if it contains whitespace, is empty, or starts with a
/// special wire sigil. Inner `"` are replaced with `'` because the wire
/// format does not escape.
pub(crate) fn quote_label(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == '"' { '\'' } else { c }).collect();
    if needs_quoting(&cleaned) {
        format!("\"{cleaned}\"")
    } else {
        cleaned
    }
}

/// Bare-quote `s` if it contains whitespace. Used for attribute values.
pub(crate) fn quote_value(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == '"' { '\'' } else { c }).collect();
    if cleaned.contains([' ', '\t']) {
        format!("\"{cleaned}\"")
    } else {
        cleaned
    }
}

fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    s.contains([' ', '\t'])
}

fn trim_start_spaces(s: &str) -> &str {
    let n = s.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
    &s[n..]
}

/// Count leading two-space indents on `line` and return `(depth, rest)`.
/// A line with three leading spaces parses as depth 1 with one space
/// of slack at the start of `rest` — the per-line parser will trim it.
pub(crate) fn split_indent(line: &str) -> (usize, &str) {
    let leading = line.bytes().take_while(|&b| b == b' ').count();
    (leading / 2, &line[leading..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &str) -> Vec<&str> {
        Tokenizer::new(input).collect()
    }

    #[test]
    fn basic_split() {
        assert_eq!(collect("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn quoted_span_kept() {
        assert_eq!(
            collect(r#"1 doc "Example Domain""#),
            vec!["1", "doc", r#""Example Domain""#],
        );
    }

    #[test]
    fn attribute_with_quoted_value() {
        assert_eq!(
            collect(r#"href=https://example.com title="Some title""#),
            vec!["href=https://example.com", r#"title="Some title""#],
        );
    }

    #[test]
    fn leading_and_trailing_spaces_ignored() {
        assert_eq!(collect("   a  b   "), vec!["a", "b"]);
    }

    #[test]
    fn empty_input() {
        assert_eq!(collect(""), Vec::<&str>::new());
        assert_eq!(collect("    "), Vec::<&str>::new());
    }

    #[test]
    fn strip_quotes_basic() {
        assert_eq!(strip_quotes(r#""hi""#), "hi");
        assert_eq!(strip_quotes("hi"), "hi");
        assert_eq!(strip_quotes(r#""""#), "");
    }

    #[test]
    fn quote_label_basic() {
        assert_eq!(quote_label("hi"), "hi");
        assert_eq!(quote_label("hi there"), "\"hi there\"");
        assert_eq!(quote_label(""), "\"\"");
    }

    #[test]
    fn quote_label_replaces_inner_quote() {
        assert_eq!(quote_label(r#"He said "hi""#), r#""He said 'hi'""#);
    }

    #[test]
    fn quote_value_quotes_only_on_whitespace() {
        assert_eq!(quote_value("plain"), "plain");
        assert_eq!(quote_value("with space"), "\"with space\"");
        assert_eq!(quote_value(""), "");
    }

    #[test]
    fn split_indent_basic() {
        assert_eq!(split_indent("foo"), (0, "foo"));
        assert_eq!(split_indent("  foo"), (1, "foo"));
        assert_eq!(split_indent("    foo"), (2, "foo"));
    }
}

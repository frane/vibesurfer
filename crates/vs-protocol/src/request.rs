//! Request line: `<primitive> [arg]... [--flag[=val]]...`
//!
//! See `docs/PROTOCOL.md` § "Request line". The primitive is a lowercase
//! identifier; positional args may be bare or `"..."`-quoted; flags use
//! the long form (`--name` or `--name=value`). The wire format does not
//! carry short flags — the CLI may accept them and translate before
//! sending.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::{ParseError, Result};
use crate::tokenize::strip_quotes;

/// One request from the CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Primitive name, e.g. `vs_open`.
    pub primitive: String,
    /// Positional arguments, in order.
    pub args: Vec<String>,
    /// Flags. `None` means a bare flag (e.g. `--full-page`); `Some(v)`
    /// means `--name=v`.
    pub flags: BTreeMap<String, Option<String>>,
}

impl Request {
    #[must_use]
    pub fn new(primitive: impl Into<String>) -> Self {
        Self {
            primitive: primitive.into(),
            args: Vec::new(),
            flags: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    #[must_use]
    pub fn flag(mut self, name: impl Into<String>) -> Self {
        self.flags.insert(name.into(), None);
        self
    }

    #[must_use]
    pub fn flag_value(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.flags.insert(name.into(), Some(value.into()));
        self
    }

    /// Encode the request as a single wire line, including the trailing
    /// newline.
    ///
    /// Args and flag values are backslash-escaped (`\` `\n` `\r` `\t`)
    /// *before* quoting so a value that itself contains newlines —
    /// e.g. a multiline `vs inspect eval` expression — survives the
    /// line-framed transport instead of being split into bogus extra
    /// request lines. `parse` reverses the escaping.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = self.primitive.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(&quote_arg(&escape_wire(arg)));
        }
        for (k, v) in &self.flags {
            out.push(' ');
            out.push_str("--");
            out.push_str(k);
            if let Some(value) = v {
                out.push('=');
                out.push_str(&quote_arg(&escape_wire(value)));
            }
        }
        out.push('\n');
        out
    }

    /// Parse one request line. Trailing newline optional.
    pub fn parse(line: &str) -> Result<Self> {
        let trimmed = line.trim_end_matches('\n').trim();
        if trimmed.is_empty() {
            return Err(ParseError::InvalidRequest {
                detail: "empty request line",
            });
        }
        let mut tokens = raw_tokens(trimmed).into_iter();
        let primitive_tok = tokens.next().ok_or(ParseError::InvalidRequest {
            detail: "missing primitive",
        })?;
        // Validate the primitive shape: lowercase identifier with
        // optional underscores.
        if !is_valid_primitive(primitive_tok) {
            return Err(ParseError::InvalidRequest {
                detail: "invalid primitive name",
            });
        }
        let mut args = Vec::new();
        let mut flags = BTreeMap::new();
        for tok in tokens {
            if let Some(rest) = tok.strip_prefix("--") {
                if rest.is_empty() {
                    return Err(ParseError::InvalidRequest {
                        detail: "flag with no name",
                    });
                }
                if let Some((name, value)) = rest.split_once('=') {
                    if name.is_empty() {
                        return Err(ParseError::InvalidRequest {
                            detail: "flag with empty name",
                        });
                    }
                    flags.insert(name.to_string(), Some(unescape_wire(strip_quotes(value))));
                } else {
                    flags.insert(rest.to_string(), None);
                }
            } else {
                args.push(unescape_wire(strip_quotes(tok)));
            }
        }
        Ok(Self {
            primitive: primitive_tok.to_string(),
            args,
            flags,
        })
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.encode().trim_end_matches('\n'))
    }
}

/// Backslash-escape the characters that the line-framed, quote-aware
/// wire would otherwise break on or mangle: the escape char itself, a
/// literal double quote (so it can't prematurely close a quoted span),
/// and the CR/LF/TAB control chars (LF/CR would split the request into
/// extra lines). Everything else passes through untouched, so the
/// common single-line, quote-free case is a no-op.
///
/// Unlike the shared `quote_value`, this is lossless: `"` survives as
/// `\"` instead of being substituted to `'`, so `vs inspect eval` can
/// carry JS string literals like `querySelector("a")` verbatim.
fn escape_wire(s: &str) -> String {
    if !s.contains(['\\', '"', '\n', '\r', '\t']) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Inverse of [`escape_wire`]. An unknown escape (`\x`) is preserved
/// verbatim — both backslash and the following char — so JS source like
/// `/\d/` round-trips even though `\d` isn't one of our escapes.
fn unescape_wire(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // `\\` → `\`; a trailing lone `\` (None) also yields `\`.
            Some('\\') | None => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Wrap an already-[`escape_wire`]d value in `"..."` if it contains
/// token-breaking whitespace or is empty; otherwise leave it bare.
///
/// Unlike the shared `quote_value`, this performs **no** `"` → `'`
/// substitution — `escape_wire` has already turned every literal quote
/// into `\"`, and substituting here would corrupt that back into `\'`.
fn quote_arg(escaped: &str) -> String {
    if escaped.is_empty() || escaped.contains([' ', '\t']) {
        format!("\"{escaped}\"")
    } else {
        escaped.to_string()
    }
}

/// Split a request line into raw tokens (quotes and escapes intact),
/// breaking on unquoted whitespace. This is a request-local, escape-
/// aware variant of the shared [`Tokenizer`]: a backslash escapes the
/// following char, so an escaped quote (`\"`) inside a quoted span does
/// not close it and an embedded space does not split the token. The
/// shared tokenizer is intentionally left escape-unaware — the tree and
/// delta grammars rely on its `'`-substitution contract — which is why
/// requests get their own splitter here.
fn raw_tokens(s: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = s;
    loop {
        let trimmed = rest.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() {
            return tokens;
        }
        let mut in_quotes = false;
        let mut end = trimmed.len();
        let mut chars = trimmed.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                // Consume the escaped char too (UTF-8 safe via the
                // iterator), so it can never be treated as a delimiter
                // or quote toggle.
                '\\' => {
                    chars.next();
                }
                '"' => in_quotes = !in_quotes,
                ' ' | '\t' if !in_quotes => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        tokens.push(&trimmed[..end]);
        rest = &trimmed[end..];
    }
}

fn is_valid_primitive(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_only() {
        let r = Request::new("vs_view");
        let s = r.encode();
        assert_eq!(s, "vs_view\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn with_arg() {
        let r = Request::new("vs_open").arg("https://example.com");
        let s = r.encode();
        assert_eq!(s, "vs_open https://example.com\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn with_quoted_arg() {
        let r = Request::new("vs_act")
            .arg("2")
            .arg("fill")
            .arg("frane@example.com");
        let s = r.encode();
        assert_eq!(s, "vs_act 2 fill frane@example.com\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn arg_with_spaces_quoted() {
        let r = Request::new("vs_act")
            .arg("2")
            .arg("fill")
            .arg("with spaces");
        let s = r.encode();
        assert_eq!(s, "vs_act 2 fill \"with spaces\"\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn bare_flag() {
        let r = Request::new("vs_capture").flag("full-page");
        let s = r.encode();
        assert_eq!(s, "vs_capture --full-page\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn flag_with_value() {
        let r = Request::new("vs_capture").flag_value("viewport", "mobile");
        let s = r.encode();
        assert_eq!(s, "vs_capture --viewport=mobile\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn flag_value_with_spaces_quoted() {
        let r = Request::new("vs_session_open").flag_value("policy", "strict mode");
        let s = r.encode();
        assert_eq!(s, "vs_session_open --policy=\"strict mode\"\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn full_request() {
        let r = Request::new("vs_capture")
            .arg("7")
            .flag("full-page")
            .flag_value("viewport", "mobile");
        let s = r.encode();
        // Flags ordered alphabetically.
        assert_eq!(s, "vs_capture 7 --full-page --viewport=mobile\n");
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn multiline_arg_survives_framing() {
        // A multiline eval expression must not break the single-line
        // wire framing: the encoded form has exactly one '\n' (the
        // terminator), and parse round-trips the original newlines.
        let expr = "const x = 1;\nconst y = 2;\nx + y";
        let r = Request::new("vs_inspect").arg("eval").arg("3").arg(expr);
        let s = r.encode();
        assert_eq!(
            s.matches('\n').count(),
            1,
            "encoded request must be a single wire line; got {s:?}"
        );
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn backslash_and_tab_round_trip() {
        let expr = "a\tb /\\d+/ c\\nd"; // real tab, regex backslash, literal \n
        let r = Request::new("vs_inspect").arg("eval").arg("1").arg(expr);
        let s = r.encode();
        assert_eq!(s.matches('\n').count(), 1);
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn double_quotes_in_arg_round_trip() {
        // The whole point of the request-local escaping: a JS string
        // literal with double quotes survives losslessly (the shared
        // quote_value would have mangled `"` → `'`).
        for expr in [
            r#"document.querySelector("a").href"#,
            r#"x === "with space" && y"#,
            r#"JSON.parse("{\"k\":1}")"#, // already-escaped quotes inside JS
        ] {
            let r = Request::new("vs_inspect").arg("eval").arg("1").arg(expr);
            let s = r.encode();
            assert_eq!(
                s.matches('\n').count(),
                1,
                "single wire line expected for {expr:?}; got {s:?}"
            );
            assert_eq!(
                Request::parse(&s).unwrap(),
                r,
                "round-trip failed for {expr:?} (encoded {s:?})"
            );
        }
    }

    #[test]
    fn multiline_flag_value_round_trips() {
        let r = Request::new("vs_act").flag_value("text", "line1\nline2");
        let s = r.encode();
        assert_eq!(s.matches('\n').count(), 1);
        assert_eq!(Request::parse(&s).unwrap(), r);
    }

    #[test]
    fn rejects_invalid_primitive() {
        for bad in ["", "VS_VIEW", "--vs", "vs!", "vs-cli"] {
            assert!(Request::parse(bad).is_err(), "{bad} should fail");
        }
    }
}

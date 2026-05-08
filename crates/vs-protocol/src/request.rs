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
use crate::tokenize::{quote_value, strip_quotes, Tokenizer};

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
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = self.primitive.clone();
        for arg in &self.args {
            out.push(' ');
            out.push_str(&quote_value(arg));
        }
        for (k, v) in &self.flags {
            out.push(' ');
            out.push_str("--");
            out.push_str(k);
            if let Some(value) = v {
                out.push('=');
                out.push_str(&quote_value(value));
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
        let mut tokens = Tokenizer::new(trimmed);
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
                    flags.insert(name.to_string(), Some(strip_quotes(value).to_string()));
                } else {
                    flags.insert(rest.to_string(), None);
                }
            } else {
                args.push(strip_quotes(tok).to_string());
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
    fn rejects_invalid_primitive() {
        for bad in ["", "VS_VIEW", "--vs", "vs!", "vs-cli"] {
            assert!(Request::parse(bad).is_err(), "{bad} should fail");
        }
    }
}

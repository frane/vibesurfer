//! Response envelope, warnings, and the state token.
//!
//! Per `docs/PROTOCOL.md` § "Response envelope":
//!
//! - `@<token>` — success.
//! - `! <CODE> [arg]...` — error.
//! - `? <code> [arg]...` — warning (one per line, before the success
//!   envelope).
//!
//! The full message envelope (warnings + success/error + body) is composed
//! by callers; this module deals with the individual lines.

use std::fmt;
use std::str::FromStr;

use crate::codes::{ErrorCode, WarningCode};
use crate::error::{ParseError, Result};
use crate::tokenize::{quote_value, strip_quotes, Tokenizer};

/// A 64-bit state token, rendered as 16 lowercase hex chars on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateToken(pub [u8; 8]);

impl StateToken {
    /// The all-zero token. Useful as a sentinel in tests; the daemon
    /// never emits this in production.
    pub const ZERO: Self = Self([0; 8]);

    /// Construct from an explicit byte array.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for StateToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for StateToken {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 16
            || !s
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ParseError::InvalidToken { raw: s.to_string() });
        }
        let mut bytes = [0u8; 8];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            // SAFETY-via-validation: chunk is guaranteed lowercase hex above.
            bytes[i] = u8::from_str_radix(std::str::from_utf8(chunk).expect("ascii"), 16)
                .map_err(|_| ParseError::InvalidToken { raw: s.to_string() })?;
        }
        Ok(Self(bytes))
    }
}

/// One warning line, preceding the success envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: WarningCode,
    pub args: Vec<String>,
}

impl Warning {
    #[must_use]
    pub fn new(code: WarningCode) -> Self {
        Self {
            code,
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_args(code: WarningCode, args: Vec<String>) -> Self {
        Self { code, args }
    }

    /// Encode as a single wire line, including the trailing newline.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = format!("? {}", self.code);
        for arg in &self.args {
            out.push(' ');
            out.push_str(&quote_value(arg));
        }
        out.push('\n');
        out
    }

    /// Parse one warning line. The leading `?` and any surrounding
    /// whitespace must already be present in `line`; trailing newline
    /// is optional.
    pub fn parse(line: &str) -> Result<Self> {
        let trimmed = line.trim_end_matches('\n');
        let body = trimmed
            .strip_prefix('?')
            .ok_or(ParseError::InvalidEnvelope {
                detail: "warning line must start with `?`",
            })?;
        let mut tokens = Tokenizer::new(body);
        let code_tok = tokens.next().ok_or(ParseError::InvalidEnvelope {
            detail: "warning missing code",
        })?;
        let code: WarningCode = code_tok.parse()?;
        let args: Vec<String> = tokens.map(|t| strip_quotes(t).to_string()).collect();
        Ok(Self { code, args })
    }
}

/// The single envelope line on a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Envelope {
    Success(StateToken),
    Error { code: ErrorCode, args: Vec<String> },
}

impl Envelope {
    /// Encode this envelope as one wire line, including the trailing
    /// newline.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Success(t) => format!("@{t}\n"),
            Self::Error { code, args } => {
                let mut out = format!("! {code}");
                for arg in args {
                    out.push(' ');
                    out.push_str(&quote_value(arg));
                }
                out.push('\n');
                out
            }
        }
    }

    /// Parse a single envelope line.
    pub fn parse(line: &str) -> Result<Self> {
        let trimmed = line.trim_end_matches('\n');
        let mut chars = trimmed.chars();
        match chars.next() {
            Some('@') => {
                let rest = chars.as_str();
                let token: StateToken = rest.parse()?;
                Ok(Self::Success(token))
            }
            Some('!') => {
                let body = chars.as_str();
                let mut tokens = Tokenizer::new(body);
                let code_tok = tokens.next().ok_or(ParseError::InvalidEnvelope {
                    detail: "error missing code",
                })?;
                let code: ErrorCode = code_tok.parse()?;
                let args: Vec<String> = tokens.map(|t| strip_quotes(t).to_string()).collect();
                Ok(Self::Error { code, args })
            }
            _ => Err(ParseError::InvalidEnvelope {
                detail: "envelope must start with `@` or `!`",
            }),
        }
    }
}

/// A response head: zero or more warnings followed by an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub warnings: Vec<Warning>,
    pub envelope: Envelope,
}

impl ResponseHead {
    #[must_use]
    pub fn ok(token: StateToken) -> Self {
        Self {
            warnings: Vec::new(),
            envelope: Envelope::Success(token),
        }
    }

    #[must_use]
    pub fn err(code: ErrorCode, args: Vec<String>) -> Self {
        Self {
            warnings: Vec::new(),
            envelope: Envelope::Error { code, args },
        }
    }

    #[must_use]
    pub fn with_warning(mut self, w: Warning) -> Self {
        self.warnings.push(w);
        self
    }

    /// Encode the head: warnings (one per line) then the envelope line.
    /// Does not emit the trailing blank line that terminates a full
    /// response — callers compose that.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for w in &self.warnings {
            out.push_str(&w.encode());
        }
        out.push_str(&self.envelope.encode());
        out
    }

    /// Parse a head from a multi-line string. Stops at the envelope line
    /// (anything after that line is the body, which the caller handles).
    pub fn parse(input: &str) -> Result<Self> {
        let mut warnings = Vec::new();
        for line in input.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('?') {
                warnings.push(Warning::parse(line)?);
            } else if line.starts_with('@') || line.starts_with('!') {
                let envelope = Envelope::parse(line)?;
                return Ok(Self { warnings, envelope });
            } else {
                return Err(ParseError::InvalidEnvelope {
                    detail: "expected `?`, `@`, or `!` at start of line",
                });
            }
        }
        Err(ParseError::InvalidEnvelope {
            detail: "no envelope line found",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip() {
        let t = StateToken([0xa3, 0xf9, 0xb2, 0xc1, 0xd4, 0xe6, 0xf7, 0x0a]);
        assert_eq!(t.to_string(), "a3f9b2c1d4e6f70a");
        assert_eq!("a3f9b2c1d4e6f70a".parse::<StateToken>().unwrap(), t);
    }

    #[test]
    fn token_zero() {
        assert_eq!(StateToken::ZERO.to_string(), "0000000000000000");
    }

    #[test]
    fn token_rejects_uppercase() {
        let err = "A3F9B2C1D4E6F70A".parse::<StateToken>().unwrap_err();
        matches!(err, ParseError::InvalidToken { .. });
    }

    #[test]
    fn token_rejects_short() {
        let err = "abc".parse::<StateToken>().unwrap_err();
        matches!(err, ParseError::InvalidToken { .. });
    }

    #[test]
    fn warning_round_trip_no_args() {
        let w = Warning::new(WarningCode::CaptchaVisible);
        let s = w.encode();
        assert_eq!(s, "? captcha_visible\n");
        assert_eq!(Warning::parse(&s).unwrap(), w);
    }

    #[test]
    fn warning_round_trip_with_args() {
        let w = Warning::with_args(WarningCode::Nav, vec!["https://example.com".into()]);
        let s = w.encode();
        assert_eq!(s, "? nav https://example.com\n");
        assert_eq!(Warning::parse(&s).unwrap(), w);
    }

    #[test]
    fn warning_arg_with_spaces_quoted() {
        let w = Warning::with_args(WarningCode::Nav, vec!["with spaces".into()]);
        let s = w.encode();
        assert_eq!(s, "? nav \"with spaces\"\n");
        assert_eq!(Warning::parse(&s).unwrap(), w);
    }

    #[test]
    fn envelope_success_round_trip() {
        let env = Envelope::Success(StateToken([0x12; 8]));
        let s = env.encode();
        assert_eq!(s, "@1212121212121212\n");
        assert_eq!(Envelope::parse(&s).unwrap(), env);
    }

    #[test]
    fn envelope_error_round_trip() {
        let env = Envelope::Error {
            code: ErrorCode::StaleToken,
            args: vec!["abcdef0123456789".into(), "nav".into()],
        };
        let s = env.encode();
        assert_eq!(s, "! STALE_TOKEN abcdef0123456789 nav\n");
        assert_eq!(Envelope::parse(&s).unwrap(), env);
    }

    #[test]
    fn response_head_with_warnings_round_trip() {
        let head = ResponseHead::ok(StateToken([0xab; 8]))
            .with_warning(Warning::with_args(
                WarningCode::Nav,
                vec!["https://example.com".into()],
            ))
            .with_warning(Warning::new(WarningCode::CaptchaVisible));
        let s = head.encode();
        let parsed = ResponseHead::parse(&s).unwrap();
        assert_eq!(parsed, head);
    }

    #[test]
    fn response_head_error() {
        let head = ResponseHead::err(ErrorCode::Timeout, vec!["5000ms".into(), "vs_wait".into()]);
        let s = head.encode();
        assert_eq!(s, "! TIMEOUT 5000ms vs_wait\n");
        assert_eq!(ResponseHead::parse(&s).unwrap(), head);
    }
}

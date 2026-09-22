//! Mask sensitive arguments before they reach the audit log.
//!
//! The default redaction list mirrors what shows up in agent
//! workflows: passwords, tokens, API keys, secrets. The match is
//! case-insensitive on the *flag name* (e.g. `--token=…`) and on
//! the immediate-prior arg (e.g. `vs_act 2 fill PASSWORD_VALUE`
//! redacts the value because the previous arg is `fill` of a
//! password-like target — but for v1 we only redact based on flag
//! names; positional secrets are the agent's responsibility unless
//! they pass `--unsafe-log` (M5).
//!
//! For now: any flag whose name matches the regex `(?i)password|
//! token|secret|key|auth` has its value replaced with `***`.

const SENSITIVE: &[&str] = &["password", "token", "secret", "key", "auth"];

/// Render the request args for the audit log, redacting sensitive
/// flag values. Returns a single string — the wire-form of args
/// minus the primitive name — suitable for `args_redacted`.
#[must_use]
pub fn redact_args(args: &[String], flags: &[(String, Option<String>)]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len() + flags.len());
    for a in args {
        parts.push(a.clone());
    }
    for (name, value) in flags {
        match value {
            Some(v) if is_sensitive(name) => parts.push(format!("--{name}=***")),
            Some(v) => parts.push(format!("--{name}={v}")),
            None => parts.push(format!("--{name}")),
        }
    }
    parts.join(" ")
}

fn is_sensitive(name: &str) -> bool {
    let lower = name.to_lowercase();
    SENSITIVE.iter().any(|n| lower.contains(n))
}

/// Redact a single free-form string (used for `vs_inspect eval`
/// expressions in `args_redacted`). Replaces inline `bearer ...` /
/// `token = ...` style secrets with `***`. Matching is intentionally
/// loose so casually-pasted credentials don't survive the audit log.
#[must_use]
pub fn redact_string(s: &str) -> String {
    const KEYWORDS: [&str; 6] = [
        "bearer ",
        "authorization:",
        "x-api-key:",
        "secret",
        "password",
        "token",
    ];
    // Every index here is a char boundary: the keywords are ASCII, the
    // delimiter search returns a boundary, and the fallback advances by
    // one whole char. Walking raw bytes instead panicked the dispatch
    // task on any non-ASCII expression — an em-dash in a comment, an
    // accented string literal — and surfaced as `! ENGINE_CRASH`, and
    // the bytes that did not panic came out as mojibake.
    //
    // ASCII-only lowercasing preserves byte offsets, so an index found
    // in `lower` is valid in `s`.
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(kw_len) = KEYWORDS
            .iter()
            .find(|kw| lower[i..].starts_with(**kw))
            .map(|kw| kw.len())
        {
            out.push_str(&s[i..i + kw_len]);
            let after = i + kw_len;
            let end = s[after..]
                .find(['"', '\'', ';', '\n', '}', ')'])
                .map_or(s.len(), |off| after + off);
            if end > after {
                out.push_str("***");
            }
            i = end;
            continue;
        }
        let c = s[i..].chars().next().expect("index is a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-ASCII input must survive intact. Indexing this string by
    /// byte panicked mid-codepoint, and a panic in an engine job
    /// comes back to the agent as `! ENGINE_CRASH` on a call that was
    /// perfectly valid — an eval whose only sin was a comment with an
    /// em-dash in it.
    #[test]
    fn non_ascii_survives_and_does_not_panic() {
        let s = "// pick the — dash — and the café ☕\nreturn 1";
        assert_eq!(redact_string(s), s);
        // Same, with a secret after the multi-byte run: still redacted.
        let got = redact_string("café token=abc123;");
        assert_eq!(got, "café token***;");
    }

    #[test]
    fn keyword_without_a_value_is_left_alone() {
        assert_eq!(redact_string("token"), "token");
        assert_eq!(redact_string("token;"), "token;");
    }

    #[test]
    fn no_flags_is_just_args() {
        assert_eq!(redact_args(&["a".into(), "b".into()], &[]), "a b");
    }

    #[test]
    fn bare_flag_kept() {
        assert_eq!(
            redact_args(&[], &[("full-page".into(), None)]),
            "--full-page"
        );
    }

    #[test]
    fn token_flag_redacted() {
        assert_eq!(
            redact_args(&[], &[("token".into(), Some("abcdef0123456789".into()))],),
            "--token=***",
        );
    }

    #[test]
    fn password_flag_redacted_case_insensitively() {
        assert_eq!(
            redact_args(&[], &[("Password".into(), Some("hunter2".into()))]),
            "--Password=***",
        );
    }

    #[test]
    fn key_inside_name_triggers_redaction() {
        assert_eq!(
            redact_args(&[], &[("api-key".into(), Some("xxx".into()))]),
            "--api-key=***",
        );
    }

    #[test]
    fn unrelated_flag_kept() {
        assert_eq!(
            redact_args(&[], &[("viewport".into(), Some("mobile".into()))]),
            "--viewport=mobile",
        );
    }
}

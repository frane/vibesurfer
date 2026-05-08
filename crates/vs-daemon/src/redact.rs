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
    let mut out = String::with_capacity(s.len());
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the start of one of the keywords.
        let rest = &lower[i..];
        let mut hit = None;
        for kw in [
            "bearer ",
            "authorization:",
            "x-api-key:",
            "secret",
            "password",
            "token",
        ] {
            if rest.starts_with(kw) {
                hit = Some(kw.len());
                break;
            }
        }
        if let Some(kw_len) = hit {
            out.push_str(&s[i..i + kw_len]);
            // Skip until the next quote/semicolon/whitespace boundary.
            let mut j = i + kw_len;
            while j < bytes.len() && !matches!(bytes[j], b'"' | b'\'' | b';' | b'\n' | b'}' | b')')
            {
                j += 1;
            }
            if j > i + kw_len {
                out.push_str("***");
            }
            i = j;
            continue;
        }
        out.push(s.as_bytes()[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

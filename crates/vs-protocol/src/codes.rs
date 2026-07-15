//! Short codes used on the wire.
//!
//! Tables match `docs/codes.md` at the workspace root. Every code
//! enum implements [`Display`](std::fmt::Display) (the canonical wire
//! form) and [`FromStr`] (returning [`UnknownCode`] on a stranger).

use std::fmt;
use std::str::FromStr;

/// A code string that did not match any known variant of the requested
/// kind. Returned by [`FromStr`] implementations on the code enums.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("unknown {kind} code: {value}")]
pub struct UnknownCode {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! code_enum {
    (
        $(#[$attr:meta])*
        $vis:vis enum $name:ident as $kind:literal {
            $(
                $(#[$variant_attr:meta])*
                $variant:ident => $wire:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis enum $name {
            $(
                $(#[$variant_attr])*
                $variant,
            )+
        }

        impl $name {
            /// The canonical wire-format string for this variant.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $wire, )+
                }
            }

            /// Every variant of this enum, in source order.
            #[must_use]
            pub const fn all() -> &'static [Self] {
                &[ $( Self::$variant, )+ ]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownCode;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(match s {
                    $( $wire => Self::$variant, )+
                    other => return Err(UnknownCode {
                        kind: $kind,
                        value: other.to_string(),
                    }),
                })
            }
        }
    };
}

code_enum! {
    /// A11y role code as it appears on the wire.
    pub enum Role as "role" {
        Doc => "doc",
        Btn => "btn",
        Lnk => "lnk",
        Tf => "tf",
        Ta => "ta",
        Sel => "sel",
        Chk => "chk",
        Rad => "rad",
        Img => "img",
        Hd => "hd",
        P => "p",
        Li => "li",
        Lst => "lst",
        Tbl => "tbl",
        Row => "row",
        Cell => "cell",
        Hdr => "hdr",
        Nav => "nav",
        Frm => "frm",
        Dlg => "dlg",
        Itm => "itm",
        Sec => "sec",
        Art => "art",
        Mn => "mn",
        El => "el",
    }
}

code_enum! {
    /// Operation an agent can perform on a tree node.
    pub enum Op as "op" {
        Click => "click",
        Fill => "fill",
        Scroll => "scroll",
        Key => "key",
        Submit => "submit",
        Hover => "hover",
        Focus => "focus",
    }
}

code_enum! {
    /// A protocol-level error code (the `! CODE` form on the wire).
    pub enum ErrorCode as "error" {
        StaleToken => "STALE_TOKEN",
        EngineUnsupported => "ENGINE_UNSUPPORTED",
        PolicyDeny => "POLICY_DENY",
        Timeout => "TIMEOUT",
        NotFound => "NOT_FOUND",
        WrongSession => "WRONG_SESSION",
        ConfirmRequired => "CONFIRM_REQUIRED",
        DaemonStartFailed => "DAEMON_START_FAILED",
        EngineCrash => "ENGINE_CRASH",
        BadRequest => "BAD_REQUEST",
        UnknownKind => "UNKNOWN_KIND",
        EvalError => "EVAL_ERROR",
        EvalSyntax => "EVAL_SYNTAX",
    }
}

code_enum! {
    /// A protocol-level warning code (the `? code` form on the wire).
    pub enum WarningCode as "warning" {
        Nav => "nav",
        CaptchaVisible => "captcha_visible",
        AuthLoaded => "auth_loaded",
        ViewportChanged => "viewport_changed",
        IdempotentHit => "idempotent_hit",
        ConsoleError => "console_error",
        HiddenTarget => "hidden_target",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<C>(samples: &[C])
    where
        C: Copy + PartialEq + fmt::Debug + fmt::Display + FromStr<Err = UnknownCode>,
    {
        for c in samples {
            let s = c.to_string();
            let parsed: C = s.parse().expect("known code");
            assert_eq!(parsed, *c);
        }
    }

    #[test]
    fn roles_round_trip() {
        round_trip(Role::all());
    }

    #[test]
    fn ops_round_trip() {
        round_trip(Op::all());
    }

    #[test]
    fn error_codes_round_trip() {
        round_trip(ErrorCode::all());
    }

    #[test]
    fn warning_codes_round_trip() {
        round_trip(WarningCode::all());
    }

    #[test]
    fn unknown_role_rejected() {
        let err = "xx".parse::<Role>().unwrap_err();
        assert_eq!(err.kind, "role");
        assert_eq!(err.value, "xx");
    }

    #[test]
    fn unknown_error_rejected() {
        let err = "WHATEVER".parse::<ErrorCode>().unwrap_err();
        assert_eq!(err.kind, "error");
    }

    #[test]
    fn role_display_matches_wire() {
        assert_eq!(Role::Btn.to_string(), "btn");
        assert_eq!(Role::Lnk.as_str(), "lnk");
    }

    #[test]
    fn role_count() {
        assert_eq!(Role::all().len(), 25);
        assert_eq!(Op::all().len(), 7);
    }
}

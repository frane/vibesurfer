//! WebKit engine binding for vibesurfer.
//!
//! This crate hosts the [`Engine`] trait — the daemon's view of "a
//! browser" — and the [`EngineRuntime`] that pins the implementation
//! to a dedicated OS thread (the only thread that may call WebKit on
//! either supported platform).
//!
//! # Backends
//!
//! - [`backend::stub`] — in-process placeholder. Always available;
//!   used by daemon tests and as the default until the real backends
//!   land. M3a.
//! - **`backend::wpe`** *(M3b)* — WPE WebKit on Linux via FFI (or
//!   `webkit2gtk-sys` as the documented fallback; see ADR 0001),
//!   driven from a thread that owns a `GMainLoop`.
//! - **`backend::webkit`** *(M3c)* — system WebKit framework on macOS
//!   via `objc2` + `objc2-web-kit`, driven from a thread that owns a
//!   `CFRunLoop` against an offscreen `WKWebView`.
//!
//! All backends implement [`Engine`]; [`EngineRuntime::spawn`] is
//! generic over the constructor so the daemon can pick a backend at
//! runtime without compile-time coupling.

#![cfg_attr(
    not(any(target_os = "macos", target_os = "windows")),
    forbid(unsafe_code)
)]
// On macOS we have a small surface of FFI to AppKit/WebKit. The unsafe
// code is constrained to `backend::webkit`; everywhere else stays
#![cfg_attr(
    target_os = "macos",
    allow(
        clippy::redundant_closure_for_method_calls,
        clippy::needless_pass_by_value,
        clippy::type_complexity,
        clippy::manual_map,
        clippy::uninlined_format_args,
        clippy::map_unwrap_or
    )
)]
// unsafe-free.
#![cfg_attr(any(target_os = "macos", target_os = "windows"), allow(unsafe_code))]

pub mod backend;
pub mod engine;
pub mod inspector;
pub mod runtime;

pub use engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};
pub use inspector::{
    ConsoleEntry, ConsoleLevel, Header, NetworkEntry, NetworkStatus, RequestDetail, RingBuffer,
};
pub use runtime::EngineRuntime;

/// Returns the crate version (matches the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}

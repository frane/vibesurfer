//! WebKit engine binding for vibesurfer.
//!
//! This crate hosts the [`Engine`] trait — the daemon's view of "a
//! browser" — and the [`EngineRuntime`] that pins the implementation
//! to a dedicated OS thread (the only thread that may call WebKit on
//! either supported platform).
//!
//! # Backends
//!
//! - [`backend::webkit`] *(macOS)* — system WebKit framework via
//!   `objc2` + `objc2-web-kit`, driven from the Cocoa main thread.
//! - [`backend::wpe`] *(Linux)* — WebKitGTK 6 via `webkit6` + `glib`,
//!   driven from a thread that owns a GLib main context.
//! - [`backend::webview2`] *(Windows)* — WebView2 via `webview2-com` +
//!   `windows-rs`, driven from a thread that owns a Win32 message pump.
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

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use backend::auth::normalize as normalize_auth_blob;
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

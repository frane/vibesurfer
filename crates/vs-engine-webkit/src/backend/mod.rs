//! Concrete [`Engine`](crate::engine::Engine) implementations.
//!
//! - [`webkit`] *(macOS)* — system WebKit framework via `objc2` +
//!   `objc2-web-kit`. Real `WKWebView`. Requires the Cocoa main thread.
//! - [`wpe`] *(Linux)* — WebKitGTK 6 via `webkit6` + `glib`. Real
//!   `WebView`. Requires the GLib main context.
//! - [`webview2`] *(Windows)* — Microsoft WebView2 via `webview2-com` +
//!   `windows-rs`. Real `ICoreWebView2`. Requires the Win32 message
//!   pump.

pub(crate) mod auth;
mod common;
pub(crate) mod inspector_bridge;

#[cfg(target_os = "macos")]
pub mod webkit;

#[cfg(target_os = "linux")]
pub mod wpe;

#[cfg(target_os = "linux")]
mod wpe_input;

#[cfg(target_os = "windows")]
pub mod webview2;

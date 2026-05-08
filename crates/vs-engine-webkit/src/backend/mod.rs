//! Concrete [`Engine`](crate::engine::Engine) implementations.
//!
//! - [`stub`] — in-process placeholder with synthetic responses. Used
//!   by daemon / store / protocol tests that need an `Engine` but do
//!   not need a real browser. NOT shipped to users on the production
//!   path.
//! - [`webkit`] *(macOS)* — system WebKit framework via `objc2` +
//!   `objc2-web-kit`. Real `WKWebView`. Requires the Cocoa main thread.
//! - [`wpe`] *(Linux)* — WebKitGTK 6 via `webkit6` + `glib`. Real
//!   `WebView`. Requires the GLib main context.
//! - [`webview2`] *(Windows)* — Microsoft WebView2 via `webview2-com` + `windows-rs`. Skeleton — open and snapshot are TODO; the trait shape and DOM-walker JS are ready.

mod common;
pub(crate) mod inspector_bridge;

pub mod stub;

#[cfg(target_os = "macos")]
pub mod webkit;

#[cfg(target_os = "linux")]
pub mod wpe;

#[cfg(target_os = "windows")]
pub mod webview2;

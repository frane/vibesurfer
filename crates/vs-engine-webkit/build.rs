// Platform-specific WebKit linking lands in milestone M3:
//   - Linux: pkg-config probe for WPE WebKit (with webkit2gtk-4.1 fallback).
//   - macOS: cargo:rustc-link-framework=WebKit and Foundation/AppKit.
// M0 ships a no-op build script so the workspace builds without WebKit.
fn main() {}

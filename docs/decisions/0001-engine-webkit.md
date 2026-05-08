# 0001 — Engine: WebKit (cross-platform)

**Status:** Accepted (M0). Amended 2026-05-06 to make macOS a peer
target rather than a deferred one.
**Date:** 2026-05-06.

## Context

vibesurfer needs a web engine that:

1. Renders modern web pages with sufficient compatibility to log into
   real SaaS apps and click through real flows.
2. Embeds cleanly into a long-running daemon — i.e., a stable embedding
   API we can drive from Rust without hosting an entire browser build.
3. Supports headless operation as a first-class mode.
4. Has a tractable threading model.
5. Runs natively on the developer's machine. The two platforms that
   matter from day one are **Linux** and **macOS**. Windows is out of
   scope for v1.

Candidate engines considered:

- **Chromium / CDP.** Most compatible. Embedding is CEF/Electron-class
  — we'd own a Chromium build. CDP is the protocol we are explicitly
  not inheriting.
- **Lightpanda.** Lightweight V8-based engine, fast for scraping.
  Rendering and layout are limited; the visual primitives
  (`vs_capture`, `vs_layout`, `vs_viewport`) cannot be implemented
  faithfully.
- **Servo.** Rust-native, attractive on paper. Headless embedding
  story is immature; project velocity is uneven; production users are
  rare.
- **WebKit.** A single engine family with mature platform-specific
  ports. JavaScriptCore, full layout, full rendering, headless.

## Decision

**WebKit, embedded via the platform-appropriate port.**

| Platform | Port                    | Linkage                                 |
| -------- | ----------------------- | --------------------------------------- |
| Linux    | WPE WebKit              | `pkg-config wpe-webkit-1.x`, FFI        |
| macOS    | System WebKit framework | `WebKit.framework` via `objc2` bindings |

The two ports share an engine but differ in embedding API. Both paths
land at the same `Engine` trait in `vs-daemon`. macOS is supported
natively from M3 — there is no Docker or VM detour.

If WPE-specific Rust bindings on Linux prove unworkable in M3, the
documented fallback is `webkit2gtk-sys`; either way the daemon is
insulated from the binding crate by the trait boundary. On macOS the
expected stack is `objc2` + `objc2-web-kit` + `objc2-foundation`,
running `WKWebView` against an offscreen target with explicit run-loop
ownership.

## Consequences

- The engine crate is `vs-engine-webkit`, not `vs-engine-wpe`. Its
  Cargo.toml has `[target.'cfg(target_os = ...)']` sections per
  platform; only the host platform's bindings compile.
- Both ports follow the same threading rule: the engine runs on a
  dedicated OS thread that owns the platform's run loop (GMainLoop on
  Linux, CFRunLoop/NSRunLoop on macOS). Tokio never touches engine
  state directly. Channels bridge the boundary. (See ADR 0005's
  consequences.)
- macOS work has its own platform-shaped problems: WKWebView normally
  expects a Cocoa main thread and a parented NSWindow. The headless
  setup uses an offscreen `WKWebView` parented to a hidden NSWindow on
  the engine thread, with a CFRunLoop driving the WebKit IPC. This is
  documented in detail in M3.
- CI runs the workspace on both `ubuntu-latest` and `macos-latest`
  from M0 onward.
- We don't write a JS engine. JavaScriptCore comes with WebKit on
  both platforms.

## Rejected

- "Build on Linux first, port to macOS later." Reverted at the user's
  request before M1. Treating macOS as a deferred port leads to APIs
  shaped around glib idioms; a peer-from-day-one constraint forces a
  cleaner trait.
- "Use Chromium with CDP under the hood." Defeats the entire point of
  the project — the protocol is the wedge, not the engine.
- "Build a fallback Chromium engine for v1 in case WebKit has gaps."
  Engine pluralism is a v2 problem. v1 ships one engine family on two
  platforms; the trait exists so v2 is *possible*, not so v1 has to
  maintain two engines.
- "WPE WebKit on macOS via Homebrew." It works in some cases but
  routes through GTK-on-macOS, which compounds platform brittleness
  with toolchain brittleness. The system WebKit framework is the
  right target on macOS.

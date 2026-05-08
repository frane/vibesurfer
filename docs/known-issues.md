# Known issues

Honest list of behaviors that aren't yet what they should be. Each entry has a short reproduction and a planned-or-tracked fix.

## Wait conditions

- **`NetIdle` is not implemented** on any backend. Calling it returns `! ENGINE_UNSUPPORTED webkit wait:net-idle-or-token-change` (or the equivalent on Linux/Windows). Implementing it requires DevTools-Protocol-style request tracking; not on the M5.5 critical path.
- **`TokenChange` is not implemented**. Same response. Conceptually a daemon-level concern (waits on the page's state token to advance) rather than an engine concern; routing TBD.

## Act targets

- **`ActTarget::Mark`** (act on a named anchor instead of a live ref) returns `NotImplemented` on every real backend. Marks already round-trip through `vs_mark` / `vs_annotate`; the missing piece is mapping a mark name to a current `data-vs-ref` attribute at act time. Land alongside the next round of mark-driven flows.

## Layout on the stub

- The `StubEngine` returns synthetic layout boxes (`box=0,0,100,20 vis=true`). It exists for protocol-coverage tests, not realism. Real layout requires a visual tier (always available on macOS / Linux); the stub is gated to `cfg(test)`.

## Linux WPE viewport

- `WpeBackend::set_viewport` resizes the WebView via GTK's `set_size_request` only. There's no equivalent of WKWebView's `setFrame` semantics on WebKitGTK; the resulting viewport may render at the requested CSS size but the page may not reflow as crisply on retina displays. Acceptable for layout extraction; less acceptable for pixel-perfect screenshots at non-default DPRs.

## Windows WebView2

- The Windows backend has `open` and `snapshot` implemented against `webview2-com`'s sample patterns, but **has not been verified at runtime** on a Windows host (we develop on macOS). CI on `windows-latest` will catch compile-level regressions; runtime correctness remains contingent on the COM message-loop integration with `MainThreadDispatcher`. Mark as "skeleton" in `vs status` until a Windows host actually drives a navigation end-to-end.
- `act`, `wait`, `capture`, `layout`, `set_viewport` all return `NotImplemented` on Windows. Planned but not yet ported.

## DOM walker

- The walker uses `document.body.innerText` for leaf-role labels, capped at 200 chars. On heavily styled pages with `display: contents` or shadow-DOM-rooted content, the label may be empty or truncated unintuitively. We've tightened container roles to use direct text only (M6 / Phase I) so containers no longer bleed full subtree text upward, but leaf-label correctness is still on a "report and we'll trace it" basis.

## Auth blob portability

- `vs_auth save` snapshots cookies + `localStorage` + `sessionStorage` only; it does **not** capture HttpOnly cookies (JS can't see them) or IndexedDB. For most agent flows that's enough. Native cookie-store extraction (WKHttpCookieStore on macOS, etc.) is a future enhancement.

## Daemon shutdown ordering

- On macOS, `vs serve` ctrl-c is handled on the tokio worker thread; the main thread's `NSRunLoop` loop only exits when the engine channel closes (i.e., when the daemon and runtime are dropped). In practice this is one extra runloop slice (~50ms). Acceptable; flagged here so it isn't surprising in a profiling trace.

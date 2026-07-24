# Known issues

Honest list of behaviors that aren't yet what they should be. Each entry has a short reproduction and a planned-or-tracked fix.

## Act targets

- **`ActTarget::Mark`** (act on a named anchor instead of a live ref) returns `NotImplemented` on every real backend. Marks already round-trip through `vs_mark` / `vs_annotate`; the missing piece is mapping a mark name to a current `data-vs-ref` attribute at act time. Land alongside the next round of mark-driven flows.

## Trusted input coverage

- Ref-based `vs act click` dispatches trusted native input on macOS only. On Linux (WebKitGTK) and Windows (WebView2) it still routes through injected JS (`isTrusted = false`); the coordinate cursor primitives (`vs click-at`, `vs hover-at`, `vs move-to`, `vs drag`) are the trusted path on those engines (since v0.1.11).
- Pure-Wayland Linux without Xwayland returns `ENGINE_UNSUPPORTED` for the cursor primitives; the libei path landed in v0.1.12 but requires the compositor's RemoteDesktop portal consent.

## Layout on the stub

- The `StubEngine` returns synthetic layout boxes (`box=0,0,100,20 vis=true`). It exists for protocol-coverage tests, not realism. Real layout requires a visual tier (always available on macOS / Linux); the stub is gated to `cfg(test)`.

## Linux WPE viewport

- `WpeBackend::set_viewport` resizes the hidden host window and the WebView's size request. There's no equivalent of WKWebView's `setFrame` semantics on WebKitGTK; the resulting viewport may render at the requested CSS size but the page may not reflow as crisply on retina displays. Acceptable for layout extraction; less acceptable for pixel-perfect screenshots at non-default DPRs.

## DOM walker

- The walker uses `document.body.innerText` for leaf-role labels, capped at 200 chars. On heavily styled pages with `display: contents` or shadow-DOM-rooted content, the label may be empty or truncated unintuitively. We've tightened container roles to use direct text only (M6 / Phase I) so containers no longer bleed full subtree text upward, but leaf-label correctness is still on a "report and we'll trace it" basis.

## Auth blob portability

- `vs_auth save` snapshots cookies (via the host-side cookie store on all three backends, so HttpOnly cookies are included) plus `localStorage` + `sessionStorage`. It does **not** capture IndexedDB. For most agent flows that's enough.

## Daemon shutdown ordering

- On macOS, `vs serve` ctrl-c is handled on the tokio worker thread; the main thread's `NSRunLoop` loop only exits when the engine channel closes (i.e., when the daemon and runtime are dropped). In practice this is one extra runloop slice (~4ms). Acceptable; flagged here so it isn't surprising in a profiling trace.

## WebAuthn / passkeys (virtual authenticator)

- vibesurfer cannot drive a passkey/WebAuthn login automatically on the macOS (WKWebView) backend. A scripted virtual authenticator (the CDP `WebAuthn.addVirtualAuthenticator` primitive) has no public WKWebView API — it exists only through a browser automation protocol (CDP in Chromium/WebView2, or `safaridriver`'s W3C WebAuthn extension), and attaching such a protocol is exactly the automation surface vibesurfer deliberately does not expose. The supported path is the fallback: `vs auth import` — the human completes the passkey login in their own browser, exports the session (cookies + storage) as a v2 auth-blob JSON, and imports it so `vs auth load` injects it into the headless page. A real virtual authenticator may land later on the Windows WebView2 backend (which speaks CDP); it is not planned for WKWebView.

## Caller-key sessions vs command substitution

- Session auto-binding keys on the parent process (`<ppid>-<start_time>`). Shell command substitution `P=$(vs open …)` runs `vs` under a *subshell* pid, so it binds a different caller key than a bare `vs session-open` in the same script — pages land in separate auto-created sessions and follow-up calls hit `WRONG_SESSION`. Workaround: pin `VS_SESSION` (`export VS_SESSION=$(vs session-open | grep -o 's_[a-z0-9]*')`) or pass `--session`. A fix (walking up past short-lived subshells, or a session-affinity file per script) is under consideration.

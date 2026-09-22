# Known issues

Honest list of behaviors that aren't yet what they should be. Each entry has a short reproduction and a planned-or-tracked fix.

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

- `vs_auth save` snapshots cookies (via the host-side cookie store on all three backends, so HttpOnly cookies are included), `localStorage`, `sessionStorage`, and IndexedDB (v0.2.7+; cell `cell_auth_carries_indexeddb`, verified on macOS). IndexedDB travels as JSON, so a record holding a `Blob`, a `File`, an `ArrayBuffer` or a typed array cannot be carried: those are counted and reported as `? storage_partial indexeddb_records=<N>` rather than restored as an empty object. Cache Storage and the Origin Private File System are not captured.

## Daemon shutdown ordering

- On macOS, `vs serve` ctrl-c is handled on the tokio worker thread; the main thread's `NSRunLoop` loop only exits when the engine channel closes (i.e., when the daemon and runtime are dropped). In practice this is one extra runloop slice (~4ms). Acceptable; flagged here so it isn't surprising in a profiling trace.

## WebAuthn / passkeys (virtual authenticator)

- `vs auth webauthn <page>` installs a virtual authenticator on macOS (WKWebView) and any WebKit backend that can inject a document-start script. It is a pure-JS software authenticator (`webauthn_virtual.js`) that overrides `navigator.credentials.create`/`.get` with an ES256 (P-256) authenticator built on WebCrypto — no CDP, no WebDriver, `navigator.webdriver` stays undefined, so it is the same "injected script, no automation surface" model as the snapshot walker. Registration and login round-trip against a real relying party. Limits: ES256 only (the common passkey algorithm); "none" attestation, so a relying party that demands direct/packed attestation with a trusted AAGUID will reject it; credentials live in-page (per document, shared across a create/get in the same session). For sites where those limits bite, `vs auth import` remains the fallback (log in with a passkey elsewhere, import the session). Linux WPE / Windows WebView2 return `ENGINE_UNSUPPORTED` for `enable_webauthn` until their document-start injection is wired.

## Caller-key sessions vs command substitution (Windows only)

- Fixed on Unix: session auto-binding keys on the POSIX session id (`sid-<sid>`), which is identical across command substitution, nested shells and pipelines, so `P=$(vs open …); vs view $P` works. Cell `cell_session_survives_command_substitution`.
- Windows has no POSIX session id and still falls back to `<parent_pid>-<parent_start_time>`. PowerShell assignment runs in-process so the common shape is unaffected, but `for /f` in `cmd.exe` spawns a child shell and would bind its own session. The planned fix is a console-scoped key (all processes sharing a console share its `GetConsoleWindow` handle), with the pid key kept for processes that have no console. Until then, on Windows pin `VS_CALLER` or `VS_SESSION`, or pass `--session`.

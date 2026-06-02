# Changelog

All notable changes to vibesurfer are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]



## [v0.1.12] - 2026-06-02

### Added
- `vs capture --base64` (alias `--b64`): reads the on-disk PNG and emits `base64=<bytes>\npath=…` on the response body. Off by default for CLI users so they still get a path; ON by default for the MCP path so Claude Desktop / Codex agents can show the pixels inline.
- MCP pending-queue for `vs_prompt_input`. The `vs mcp` subprocess has no tty, so the v0.1.11 local prompt path didn't work for agents on Claude Desktop / Codex. Now the agent's `vs_prompt_input` call enqueues a pending entry on the daemon and parks on a Condvar; the local user runs `vs pending fulfill` interactively, types the value at the local tty (rpassword for `--secret`), and the agent's tool call returns with the new state token. Wire: `vs_prompt_input_queue`, `vs_pending_list`, `vs_pending_peek`, `vs_pending_fulfill`, `vs_pending_cancel`. CLI: `vs pending list/fulfill/cancel` (alias `pe`).

### Changed
- `vs_prompt_input` (MCP-only) route now goes through the pending queue instead of trying to read tty in the subprocess. Local `vs prompt-input` is unchanged.


## [v0.1.11] - 2026-06-02

### Added
- Trusted native input dispatch on Linux + Windows for the cursor primitives (`vs move-to`, `vs click-at`, `vs hover-at`, `vs drag`). All three engines now emit `MouseEvent`s with `isTrusted = true` in JS — anti-bot pipelines (Google, Cloudflare, hCaptcha) no longer flag clicks as automated. Coverage and approach per platform:
  - **Linux (WebKitGTK 6)** — XTest `FakeInput` over the pure-Rust `x11rb` client. Connection is opened once per process and cached in a `OnceLock`. Every WebView is now hosted in a hidden, decoration-off `gtk::Window` so the XTest event has a real `GdkSurface` on the X server to land on. Works under xvfb (CI) and any X11 / Xwayland session. Pure Wayland without Xwayland is detected and reserved for the v0.1.12 libei path (scaffold + detection live now; see `wpe_input::LibeiDispatcher::try_new`).
  - **Windows (WebView2)** — Migrated controller creation from `CreateCoreWebView2Controller` to `CreateCoreWebView2CompositionController` so the page exposes `SendMouseInput`, the Microsoft-documented input-injection API. DirectComposition wiring (device / target / visual created with `DCompositionCreateDevice2`) backs each page; every page also gets its own message-only HWND so DirectComposition doesn't reject the second target with `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`.
  - **macOS (WKWebView)** — unchanged; the existing NSEvent path from v0.1.8 already emitted trusted input.
- `vs drag` now also synthesizes the HTML5 drag-and-drop event chain (`dragstart` → `dragenter` → `dragover` → `drop` → `dragend`) with a real `DataTransfer` after the OS-level mouse path completes. The browser's HTML5 dnd pipeline only fires from real hardware input, so without this synthetic OS-level drag events (NSEvent / XTest / SendMouseInput) would miss `react-dnd`'s HTML5 backend, native `draggable="true"` widgets, and React-Flow nodes wired to HTML5 dnd. Targets that don't care (canvas drag, React-Flow pan, sliders) absorb the extra events as no-ops. If the source handler `preventDefault`s `dragstart`, the rest of the chain is skipped so we don't fabricate a drop the page opted out of.

### Changed
- `wpe.rs` open()/close() lifecycle: every WebView is parented to a hidden `gtk::Window` and dismissed via `Window::close()` on `Engine::close`. `set_viewport` resizes both the hidden window and the WebView's size request so responsive CSS still tests right at narrow widths.
- `webview2.rs` per-page state grows `comp_controller`, `_dcomp_device`, `_dcomp_target`, `_dcomp_visual`, `last_mouse`; the singleton `parent_hwnd` is replaced by a per-page HWND created via the same `vs-webview2-host` class.

### Dependency notes
- Linux gains `x11rb 0.13` (pure-Rust X11 client) with the `xtest` feature. No `unsafe`, no `dlopen`, no `libc` on Linux — the `#![forbid(unsafe_code)]` policy is preserved for the Linux backend.
- Windows gains the `Win32_Graphics_DirectComposition` feature on the `windows` crate.

### Compatibility
- No wire-protocol change. Cursor primitives that previously returned `ENGINE_UNSUPPORTED` on Linux / Windows now return the same `state_token` / success envelope they always have on macOS. Agents that branched on `! ENGINE_UNSUPPORTED` will see those branches stop firing.
- Pure-Wayland Linux without Xwayland still returns `ENGINE_UNSUPPORTED` for the cursor primitives. Agents on those hosts fall back to ref-based `vs act` exactly as before.

## [v0.1.10] - 2026-06-01

### Security
- Sensitive `<input>` values are now masked in the accessibility tree returned by `vs view`. Up through v0.1.9 the snapshot walker's `labelFor` read `el.value` for every input regardless of type — including `<input type="password">` — so any agent that called `vs view` after `vs prompt-input --secret` saw the cleartext password the user had just typed via tty. The `--secret` flag only suppressed terminal echo; the value still landed in the next snapshot. Fixed: inputs whose `type` is `password` or `hidden`, or whose `autocomplete` is `current-password` / `new-password` / `one-time-code` / `cc-number` / `cc-csc`, now report `***` in the tree if a value is set, or the placeholder if empty.

### Fixed
- Walker and ref-lookup helpers now pierce open shadow roots. OneTrust, Cookiebot, Sourcepoint, and most web-component-based UIs put their cookie consent buttons (and other actionable elements) inside a shadow root that `document.querySelectorAll` doesn't cross. v0.1.10's `visit()` recurses into `el.shadowRoot.children` alongside `el.children`, and the new `window.__vsFindRef(r)` global helper does a shadow-piercing lookup the act / wait / layout / inspect-dom JS now use. The five existing `document.querySelector('[data-vs-ref=...]')` call sites fall back to the document query if the helper isn't installed yet, so older pages still work.
- MCP `build_cli` arms for `vs_move_to`, `vs_click_at`, `vs_hover_at`, `vs_drag`, `vs_prompt_input`, `vs_prompt_confirm`. Without these the dispatch hit a catch-all "unknown tool" branch and Claude Desktop surfaced "Failed to call tool" with no underlying detail. `vs_prompt_input` still won't read from a tty when invoked via MCP (the `vs mcp` subprocess has none) — the pending-queue mechanism for that lands in v0.1.11. Cursor primitives work over MCP normally.


## [v0.1.9] - 2026-06-01

### Added
- `vs prompt-input <PAGE> <REF> --message="..." [--secret] --token=<TOK>` (short `pi`). The CLI reads a value from the local tty (`rpassword` for `--secret` so terminal echo is off), then ships it to the daemon, which fills the field via the existing trusted-prototype-setter path. The agent that issued the call never sees the bytes the user typed. Intended for any value the agent must not see: passwords, TANs, credit-card numbers, recovery phrases.
- `vs prompt-confirm <PAGE> --message="..."` (short `pc`). Blocks until the human at the local tty presses Enter; aborts on Ctrl-C / EOF. Use as a human-in-loop gate before a sensitive mutating click.
- MCP tools `vs_prompt_input` and `vs_prompt_confirm` with the same shape.
- SKILL.md gets a "Human-in-loop" section under the primitives table, explicitly directing agents to call `vs prompt-input` (not `vs act fill`) whenever a value should not enter the agent context.

### Compatibility
- No wire-protocol change. Both prompt primitives are pure CLI sugar: `prompt-input` synthesizes a `vs_act fill` request after reading the value; `prompt-confirm` returns locally without any wire call.


## [v0.1.8] - 2026-06-01

### Added
- Four coordinate-addressed cursor primitives: `vs move-to` (short `mt`), `vs click-at` (`ca`), `vs hover-at` (`ha`), `vs drag` (`dr`). Each takes `(x, y)` (drag takes `(x1, y1, x2, y2)`) plus `--mode={human,careful,robotic}` (short `-M`). Mutating ops (`click-at`, `drag`) also require `--token=<state token>`.
- `vs-humanize` is now wired through the engine. `vs act click` and the new cursor primitives on macOS dispatch a Bezier-pathed `MouseMoved` lead-in from the page's last-known cursor position to the target rect / coordinates before the trusted `mouseDown`/`mouseUp` pair. Every event keeps `isTrusted = true` in JS; the visible motion is indistinguishable from a real cursor reaching the target.
- Snapshot walker now consults the ARIA `role` attribute (Radix UI, Headless UI, Reach UI, every `<div role="button|option|menuitem|...">` pattern) before falling back to HTML tag names. A tabindex heuristic catches focusable div/span triggers that don't carry an explicit `role`. Modern React UIs surface as actionable refs without coordinate workarounds.
- MCP tool registrations for the four new primitives.
- README "Not detected as automated" section explaining the trust chain.

### Changed
- The 19-primitive line in SKILL.md is now 23 primitives. Lifecycle / Read / Mutate / Search / Capture sections unchanged; new "Cursor coordinates" section documents the four new entries.

### Known gaps
- Linux (WebKitGTK) and Windows (WebView2) backends still dispatch the existing `vs act click` through injected JS (`isTrusted = false`); the new cursor primitives return `ENGINE_UNSUPPORTED` on those engines. v0.1.9 wires native input on both: GDK `gdk_display_put_event` for WebKitGTK, CDP `Input.dispatchMouseEvent` for WebView2.


## [v0.1.7] - 2026-06-01

### Fixed
- Concurrent agents no longer collide on a single shared session. Up through v0.1.6 the CLI resolved the session id from a single `~/.vibesurfer/active-session` file; two agents both running `vs session-open` overwrote each other and every subsequent `vs view`/`vs act` from either side could land in the wrong session. The CLI now keys sessions by `<parent_pid>-<parent_start_time>` — two shells (or two agent processes) get independent sessions automatically and the active-session footgun is gone.

### Added
- `VS_SESSION` env var. When set, takes precedence over the caller-key auto-mapping but is overridden by an explicit `--session=<id>`/`-S`. Useful for shells that want to pin a session across invocations or share one across cooperating processes.
- Auto session-open. If a command needs a session and none has been resolved for the current caller, the CLI implicitly runs `vs_session_open` first and binds the new id to the caller key. Agents stop having to call `vs session-open` explicitly before the first `vs open`.
- `~/.vibesurfer/callers/<key>` files replace `~/.vibesurfer/active-session`. Each caller-key file stores one session id; the old single-tenant pointer file is no longer read or written.


## [v0.1.6] - 2026-06-01

### Fixed
- Cookies set via `Set-Cookie` on in-session fetches now persist in `WKHTTPCookieStore` on macOS. v0.1.5 left `WKWebViewConfiguration.websiteDataStore` to its default value; in headless Cocoa processes that default landed on a non-persistent or per-page store, and `Set-Cookie` from XHR/fetch responses got dropped silently between the network stack and the cookie jar. v0.1.6 explicitly assigns `WKWebsiteDataStore.defaultDataStore()` on every config so all pages share the same persistent jar.

### Added
- `vs serve --stop` reads `~/.vibesurfer/daemon.pid` and sends `SIGTERM` (Unix) / `TerminateProcess` (Windows), then waits up to 5s for the socket to disappear. The daemon writes the PID file on startup and removes it on graceful shutdown. Replaces the `pkill -f "vs serve"` ritual after `brew upgrade`.
- The daemon now also handles `SIGTERM` (in addition to `SIGINT`) as a graceful-shutdown signal.


## [v0.1.5] - 2026-05-19

### Fixed
- `vs inspect storage cookies` now sees HttpOnly cookies. The command used to enumerate `document.cookie` from injected JS, which by spec cannot read entries with the `HttpOnly` attribute, so any session cookie set by a real auth flow showed up as empty. The cookies scope now reads from the host-side cookie store (`WKHTTPCookieStore` on macOS, `WebKitCookieManager` on Linux, `ICoreWebView2CookieManager` on Windows) and reports `secure`, `httponly`, `samesite=...`, and `expires=...` flags inline.

### Added
- `cell_inspect_storage_cookies_includes_http_only` integration cell verifies the HttpOnly listing on each backend.


## [v0.1.4] - 2026-05-19

### Fixed
- crates.io now renders the project README on every published crate page. Previously the package tarballs shipped no README (the canonical one lives at the repo root, outside any single crate dir), so `crates.io/crates/vibesurfer` showed "no README.md file". Each crate now includes a mirrored copy of the root README and references it via `readme = "README.md"`. `scripts/sync-plugin.sh` keeps the mirrors aligned.


## [v0.1.3] - 2026-05-18

### Fixed
- `vs act fill` now reaches React-style controlled inputs. The primitive used to set `el.value` directly, which trips React's per-instance value-setter override and leaves the framework's internal state empty; the form would then submit with empty payloads. The fill JS now calls the original `HTMLInputElement.prototype` (or `HTMLTextAreaElement.prototype`) setter via `Object.getOwnPropertyDescriptor(..., 'value').set.call(el, …)` — the canonical Playwright / Puppeteer fix.

### Added
- `cell_act_fill_react_controlled_input` integration cell against a React-tracker fixture (`fixtures/react-form.html`); ensures the form actually POSTs the value rather than submitting an empty body.

### Changed
- Workspace crates published to crates.io. The CLI ships as the `vibesurfer` crate (binary stays `vs`); supporting crates `vs-protocol`, `vs-store`, `vs-engine-webkit`, `vs-daemon`, `vs-humanize` are publishable lib crates with version-pinned path deps. `cargo install vibesurfer` now works.

## [v0.1.2] - 2026-05-18

### Fixed

- `vs auth save/load` previously operated on `document.cookie` from injected JS. By spec that API cannot see or write cookies with the `HttpOnly` attribute, so every modern web app's session token silently dropped on save and the agent burned cycles wondering why protected pages kept redirecting to login. Save and load now route through the host-side cookie store on every backend: `WKHTTPCookieStore` on macOS, `WebKitCookieManager` on Linux, `ICoreWebView2CookieManager` on Windows. `localStorage` and `sessionStorage` still go through the JS shim since those buckets are JS-accessible by design.
- Auth-blob schema bumped to v2 with a structured `cookies` array (name, value, domain, path, expires_unix, secure, http_only, same_site). v1 blobs from v0.1.1 still decode for back-compat.

### Added

- New integration cell `auth::cell_auth_http_only_save_and_load` exercises an `HttpOnly` session cookie through save → load on a fresh page.
- New fixture routes `/login-httponly` (issues `HttpOnly` cookie) and `/dashboard-httponly` (gated on it).

### Changed

- License switched from MIT to Apache-2.0.


## [v0.1.1] - 2026-05-13

Memory-leak fix on the inspector capture path. v0.1.0 ring-buffered the console and network `Entry` lists at 1000 each, but the parallel `RequestDetail` and in-flight `NetworkPending` maps were unbounded `HashMap`s. A long-running page (any SPA with a chatty fetch loop, or a tab left open over a day) accumulated them indefinitely; one real-world session reached ~33 GB of resident memory in the daemon before being killed.

### Fixed

- `RequestDetailStore` and `NetworkPending` are now bounded FIFO maps at the same `DEFAULT_BUFFER_CAPACITY` (1000) as the ring buffers. Insertion past capacity evicts the oldest entry in O(n) on the order deque (n ≤ capacity). Five new unit tests in `inspector_bridge.rs` cover the eviction shape and the start/end pairing.
- `RingBuffer::push` now returns the evicted entry as `Option<T>` so callers maintaining side-tables can stay aligned without external bookkeeping.

### Compatibility

No wire-format change. No CLI change. `vs inspect request <seq>` may now return "not found" for a seq beyond the 1000-entry retention window (previously: would return any seq that had ever ended on this page). The retention window is configurable in code via `DEFAULT_BUFFER_CAPACITY`; a runtime knob is deferred.


## [v0.1.0] - 2026-05-11

First public release. The wire protocol is stable, the three engines (macOS WKWebView, Linux WebKitGTK 6, Windows WebView2) are all CI-verified by the same 48-cell integration suite on every push.

### Highlights

- **20 primitives** end-to-end on every backend (`session-open` / `session-close` / `open` / `close` / `view` / `read` / `act` / `find` / `wait` / `extract` / `mark` / `annotate` / `status` / `log` / `skill` / `capture` / `viewport` / `layout` / `auth` / `inspect`).
- **State-token concurrency** with `! STALE_TOKEN` on read-write conflicts; **tree-delta wire format** after the first snapshot; **audit-by-construction** — every primitive writes one row to `actions` before returning.
- **Trusted clicks on macOS** via native `NSEvent` dispatch (regression test pins `event.isTrusted = true`). Linux + Windows still route clicks through injected JS; native dispatch on those engines is the next milestone.
- **AES-256-GCM auth blobs** persisted via `vs auth save/load/list/clear`; keyring with `~/.vibesurfer/key` fallback.
- **CLI short-form aliases** for every primitive and frequent flag (`vs o`, `vs v`, `vs a`, `-S`, `-F`, `-s`, `-n`, `-P`, `-j`, …) — long forms remain, 35 parity tests pin them byte-for-byte.
- **MCP server** (`vs mcp`) exposes each primitive as an MCP tool. **`vs skill install`** writes the SKILL.md and MCP config into Claude Desktop, Cursor, Codex, Gemini, and OpenClaw.
- **Plugin manifests** for Claude Code marketplace, Codex CLI, Gemini CLI, and any MCP-aware agent in `plugin/` and `.claude-plugin/`.
- **Multi-platform release pipeline** at `.github/workflows/release.yml` builds `vs` for `{x86_64,aarch64}-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` on tag push.

### Known gaps

- Linux + Windows produce JS-synthetic clicks (`event.isTrusted = false`), so anti-bot pipelines will treat them as automated. Use the macOS engine for fingerprint-sensitive sites.
- `vs act mark:<name>` is the one remaining `NotImplemented` in the engine layer; today only `ref:<N>` targets work for mutations.
- The `vs-humanize` crate ships as pure math (Bezier paths, Fitts arrival times, lognormal keystroke timing, inertia scrolls) and is not yet wired into the engine layer.

### Detail by milestone

The chronological work log below covers M1 through M7 PR1, in the order it landed.

### Added
- M6 follow-up: Linux WPE primitives complete + Windows WebView2 skeleton.
  - **Linux WpeBackend full primitive set.** `act`
    (Click/Fill/Scroll/Key/Submit/Hover/Focus via JS dispatch), `wait`
    (Stable/Text/RefAppears/RefGone — JS poll), `capture`
    (`WebView::snapshot` → cairo `ImageSurface::write_to_png` →
    disk), `layout` (`getBoundingClientRect` per ref), `set_viewport`
    (`WebView::set_size_request`). Same JS injection pattern as the
    macOS backend; the DOM-walker payload is shared via the
    `snapshot_dom_walker.js` `include_str!`. Other primitives
    (`save_auth`, `load_auth`) stay `NotImplemented`.
  - **Linux `vs serve` wiring.** `vs-cli/src/serve.rs` now has a
    third platform path (`cfg(target_os = "linux")`): GTK4 init on
    the OS main thread, WpeBackend constructed with the `--home`
    captures dir, tokio runtime spawned on a worker thread,
    `MainThreadDispatcher` drains engine jobs between iterations of
    the GLib main context. Mirrors the macOS shape (NSApp +
    NSRunLoop) with the equivalent GTK/GLib primitives.
  - **Windows Webview2Backend skeleton** (`backend/webview2.rs`,
    new, `cfg(target_os = "windows")`). Real `webview2-com` +
    `windows-rs` deps wired in. Engine trait shape, `W2Page` state
    holder, `parse_role` + `parse_snapshot` + DOM walker
    `include_str!` are present. `open` and `snapshot` documented
    with the COM dance required (`CreateCoreWebView2Environment` →
    `CreateCoreWebView2Controller` → `Navigate` → `NavigationCompleted`
    → `ExecuteScript`) but currently return `NotImplemented` —
    the message-loop integration with `MainThreadDispatcher` is the
    last unimplemented piece. Linux + macOS builds verified clean
    with the Windows module gated out.
  - **Cargo dep coordination.** `webkit6 = "0.6"` pulls
    `gtk4 = "0.11"` transitively; vs-cli now matches that version
    (and `glib = "0.22"`) on Linux to avoid the `links = "gtk-4"`
    duplicate-package conflict. `webview2-com = "0.39"` and
    `windows = "0.62"` (with the Win32 features used by WebView2:
    Foundation, COM, Threading, WindowsAndMessaging, Graphics_Gdi)
    on Windows.
  - **Test count: 151.** Same as before; all gates green
    (fmt / clippy `-D warnings` / workspace test) on macOS. Linux
    and Windows paths are structurally complete, gated behind
    target cfg, and waiting for CI verification with the matching
    system packages installed.
- M6: real browser backends — no more stubs.
  - **macOS WkBackend** (`vs-engine-webkit/src/backend/webkit.rs`,
    new). Real `WKWebView` driven via `objc2` + `objc2-web-kit`.
    All seven primitives: `open` (real navigation, custom
    `WKNavigationDelegate` waiting on `didFinishNavigation:` /
    `didFailNavigation:`), `snapshot` (injected JS DOM walker stamps
    `data-vs-ref` attributes for stable refs across calls, parsed
    JSON → `Tree`), `act` (Click/Fill/Scroll/Key/Submit/Hover/Focus
    via JS dispatch on the ref'd element), `wait` (Stable, Text,
    RefAppears, RefGone — JS predicate poll), `capture` (real
    `takeSnapshotWithConfiguration:` → NSImage → NSBitmapImageRep
    → PNG, written to a configurable directory), `layout`
    (`getBoundingClientRect` per ref via JS), `set_viewport`
    (`setFrame:` on the WKWebView).
  - **macOS NSApp wiring in `vs serve`** (`vs-cli/src/serve.rs`,
    rewritten under `cfg(target_os = "macos")`). `NSApplication` now
    runs on the OS main thread; the tokio runtime is spawned on a
    worker thread. Engine calls flow from tokio → mpsc channel →
    main thread, where they're drained between `NSRunLoop`
    slices via the new `MainThreadDispatcher`. Existing 19-primitive
    e2e test (`vs-cli/tests/e2e_cli.rs`) now runs against the real
    WebKit-backed daemon and passes.
  - **`EngineRuntime::dispatcher`** (new constructor in
    `vs-engine-webkit/src/runtime.rs`). Builds a runtime handle
    paired with a `MainThreadDispatcher`; lets the daemon issue
    engine calls from tokio workers while the engine itself stays
    pinned to the OS main thread. Complements the existing
    `EngineRuntime::spawn` (worker-thread engine) for backends that
    don'`t require thread pinning.
  - **`Engine` trait dropped its `Send` bound.** WKWebView
    `Retained<>` types and `MainThreadMarker` are intentionally
    `!Send`; the engine never crosses threads in the
    `MainThreadDispatcher` mode, so the bound was unnecessarily
    restrictive.
  - **Linux WpeBackend** (`vs-engine-webkit/src/backend/wpe.rs`,
    new, `cfg(target_os = "linux")`). Real `WebKitGTK 6` via the
    `webkit6` + `glib` crates. Skeleton implements `open`
    (`load_uri` + `connect_load_changed` waiting for
    `LoadEvent::Finished`), `snapshot` (same JS DOM walker via
    `evaluate_javascript`), `close`. Other primitives return
    `EngineError::NotImplemented`. **NOT verified on macOS** — the
    file is excluded from compilation on non-Linux targets via
    `cfg`. Linux CI must validate the build with the matching
    `-dev` packages installed (`libwebkitgtk-6.0-dev`,
    `libgtk-4-dev`, `libsoup-3.0-dev`).
  - **DOM walker JS extracted to a shared file**
    (`crates/vs-engine-webkit/src/backend/snapshot_dom_walker.js`,
    new). Both backends `include_str!` the same payload; they can'`t
    drift out of sync.
  - **`EngineError::NotImplemented`** — new variant
    (`vs-engine-webkit/src/engine.rs`). Distinct from `Unsupported`:
    the platform *can* support the primitive, the port just
    isn'`t there yet. Used by both `WkBackend` and `WpeBackend` for
    primitives that aren'`t live in this milestone.
  - **`short_id` widened from 12 → 24 hex chars**
    (`vs-daemon/src/daemon/mod.rs`). The 12-char form lopped off
    everything after the v7 ms timestamp prefix and reproducibly
    collided when two ids minted in the same millisecond
    (`primitives_10_19::annotate_targets` was failing on every
    run). 24 chars include enough of `rand_b` (62 random bits) that
    same-ms collisions are vanishingly unlikely.
  - **`vs-engine-webkit/examples/wk_smoke.rs`** — runs the real
    WkBackend against a URL (default: `https://example.com`),
    exercises every primitive, prints the canonical `Tree` and
    writes two PNG screenshots. Verified live: example.com loaded,
    Hacker News loaded with full table+row+cell+lnk extraction,
    layout box for "Learn more" link returned `(256, 198, 82×18)`,
    desktop and mobile-viewport screenshots produced.
  - **Test count: 151.** All gates green: fmt, clippy
    `-D warnings`, workspace test. macOS path runs against real
    WebKit; Linux path is structurally complete and waiting for CI
    verification.
- M5 step 5: wire-level conflict tests + binary-driven e2e script.
  - **Two new wire tests** (`vs-daemon/tests/wire.rs`):
    - `wire_stale_token_rejected` — opens a session and a page, sends
      `vs_act` with a fabricated stale token, asserts the response is
      `! STALE_TOKEN <current> <reason>`. Validates the
      optimistic-concurrency contract over the real Unix socket.
    - `wire_idempotent_replay_returns_warning` — sends the same
      `vs_act` twice with the same before-token, asserts the second
      response carries the `? idempotent_hit` warning before the
      success envelope. Validates the audit/idempotency replay path
      end-to-end (CLI → wire → daemon → store).
  - **Binary-driven e2e script** (`vs-cli/tests/e2e_cli.rs`). Uses
    `env!("CARGO_BIN_EXE_vs")` to spawn the actual `vs` binary as
    `vs serve` against a temp $VIBESURFER_HOME, then drives the
    same binary as a CLI through every primitive (1–19, including
    open/close/session-close), then opens the persisted SQLite database
    read-only and asserts on row state: session/page closed, mark
    persisted, annotation attached, audit row for every primitive,
    `vs_act` row carries its `group=login-flow` label, captures
    directory holds at least one PNG. This is the milestone exit gate
    for the agent path: argv → wire → socket → daemon → store.
  - **Test count: 151 (was 148).** Wire tests: 1 → 3. CLI tests now
    include 1 binary-spawning e2e + 2 in-process smoke. All gates
    green: fmt, clippy `-D warnings`, workspace test.
- M5 step 4: file-split refactor + primitives-10-19 integration tests.
  - **Five oversized files split into directory modules.** No file in
    the workspace now exceeds 403 lines (was 1368).
    - `vs-daemon/src/daemon.rs` (1368) →
      `daemon/{mod, audit, responses, lifecycle, page_ops, store_ops,
      engine_ops}.rs`. Largest piece: `engine_ops.rs` at 271 lines.
    - `vs-daemon/src/server.rs` (902) →
      `server/{mod, helpers, lifecycle, page_ops, store_ops,
      engine_ops}.rs`. Largest: 263 lines.
    - `vs-store/src/store.rs` (855) →
      `store/{mod, sessions, pages, refs, marks, annotations,
      actions, auth_blobs, skill_cache, tests}.rs`. Largest: 246
      lines.
    - `vs-protocol/src/delta.rs` (1043) →
      `delta/{mod, apply, diff, tests}.rs`. Largest: 397 lines.
    - `vs-cli/src/commands.rs` (503) →
      `commands/{mod, dispatch, render}.rs`. Largest: 403 lines.
  - Each `Daemon` / `Store` method group lives in its own submodule
    contributing one `impl Daemon { ... }` / `impl Store { ... }`
    block; private helpers exposed as `pub(crate)` / `pub(super)`
    only where actually needed.
  - **Bug fix in `Daemon::viewport`.** Was consuming `force_full`
    inside its own re-snapshot, leaving the *next* `vs_view` as
    `NoChange` instead of `Full`. Now leaves `force_full=true` after
    apply, matching `docs/PROTOCOL.md`'s "next vs_view is a fresh
    full tree" contract. Caught by the new
    `viewport_rebaselines_view` integration test.
  - **13 new integration tests** in
    `vs-daemon/tests/primitives_10_19.rs` covering
    `vs_extract` (table schema + unknown-schema error),
    `vs_mark` (audit trail), `vs_annotate` (page + ref targets),
    `vs_log` (group filter), `vs_skill` (list/show, missing skill,
    real skill body), `vs_capture` (PNG header verification),
    `vs_viewport` (re-baseline regression test), `vs_layout` (one
    box per ref), and `vs_auth` (full save/list/load/clear cycle +
    no-master-key error).
  - **Test count: 148 (was 135).** All gates green: fmt, clippy
    `-D warnings`, test, build.
- M5 step 3: primitives 15–19 + StubEngine extension.
  - **`StubEngine` extension.** `with_capture_dir(path)` builder;
    `capture` writes a 1×1 transparent PNG (option (b) per the M5
    plan) to the configured directory, returns the on-disk path.
    `layout` produces synthetic stacked boxes (one per ref, 320×24
    each). `EngineCapabilities::STUB` updated to advertise
    `renders / honors_viewport / measures_layout / persists_auth`.
  - **`Daemon` configuration builders.** `Daemon::with_captures_dir`,
    `with_skills_dir`, `with_master_key`. Optional fields default to
    sensible values; production wiring lives in `vs-cli::serve` which
    resolves the master key via `MasterKey::resolve` (keyring →
    `~/.vibesurfer/key` fallback).
  - **Primitive 15 — `vs_skill list|show`.** Lists subdirectories of
    `~/.vibesurfer/skills/`; `show <name>` returns the body of
    `<name>/SKILL.md`. Execution dispatch is M6.
  - **Primitive 16 — `vs_capture`.** Wired through to
    `Engine::capture`. Stub returns a real PNG path on disk; M3b/M3c
    will produce real pixels.
  - **Primitive 17 — `vs_viewport`.** Sets viewport via the engine,
    invalidates the page baseline, re-snapshots, returns the new
    token plus a `? viewport_changed <W>x<H>` warning.
  - **Primitive 18 — `vs_layout`.** Computed boxes per ref. Stub
    renders synthetic positions; daemon emits one row per ref on the
    wire (`<ref> x=… y=… w=… h=… visible=… z=…`).
  - **Primitive 19 — `vs_auth save|load|list|clear`.** End-to-end
    auth blob persistence: engine `save_auth` → AES-256-GCM encrypt
    via `vs_store::Store::save_auth` (using the daemon's
    `MasterKey`); reverse on `load`. `load` invalidates the page
    baseline and emits `? auth_loaded <name>`.
  - Wire handlers for all five new primitives in `vs-daemon::server`,
    plus `parse_viewport_spec` covering both presets and `WxH` form.
  - Clap subcommands for all five in `vs-cli::commands`: `vs skill`,
    `vs capture`, `vs viewport`, `vs layout`, `vs auth`.
  - **All 19 primitives now reachable end-to-end** through the CLI
    against the in-process stub engine.
- M5 step 2: AuditGuard refactor + primitives 10–14.
  - **`AuditCtx` + `audit_call` helper.** Each primitive body runs
    inside a closure that receives `&mut AuditCtx`; the wrapper
    records exactly one row in `actions` regardless of `Ok`/`Err`,
    closing the audit-on-error gap flagged at M4 review. The
    13-arg internal `audit()` helper is gone; `audit_from_ctx`
    takes `&AuditCtx` instead.
  - **`ActCall` parameter struct.** `Daemon::act` no longer takes 8
    positional args; callers build an `ActCall { ... }` struct.
    `#[allow(clippy::too_many_arguments)]` retired honestly.
  - **All 10 existing primitives back-ported** to the new pattern:
    `vs_session_open`, `vs_session_close`, `vs_open`, `vs_close`,
    `vs_view`, `vs_read`, `vs_act`, `vs_find`, `vs_wait`, `vs_status`.
  - **Primitive 10 — `vs_extract`.** Schemas: `table` (walk
    `tbl→row→cell`, emit one record per row) and `list` (walk
    `lst→itm/li`, emit `[role, label]`). `form|jsonld|webmcp` return
    `BadRequest "not implemented in stub backend"`; unknown schemas
    error cleanly.
  - **Primitive 11 — `vs_mark`.** Persists `(session, page, ref,
    name)` via `Store::create_mark`. Synthesizes a stub `dom_path`
    of `<role>#<ref>`; real engines will replace this in M3b/M3c.
    Token-fresh check on entry.
  - **Primitive 12 — `vs_annotate`.** Targets `ref:N | mark:NAME |
    page`; thin wrapper over `Store::add_annotation`.
  - **Primitive 14 — `vs_log`.** Slice the audit log via
    `Store::list_actions`; CLI flags `--page`, `--group`, `--since`,
    `--limit` map onto `ActionFilter`.
  - Wire handlers in `vs-daemon::server` + clap subcommands in
    `vs-cli::commands` for all four new primitives.
  - **LSP / clippy now working.** Updated `~/.agented/config.json`
    with `ide.languages.rust.servers[].init_options` carrying
    `linkedProjects` (pointing at the workspace `Cargo.toml`) and
    `check.command = "clippy"`. Inline `diag` lines now surface
    clippy lints on writes (caught a missing match arm during this
    milestone before cargo would have).

  Remaining for M5: primitives 15–19 (skill, capture, viewport,
  layout, auth), end-to-end CLI script over the 19, wire-test
  additions for stale-token and idempotency.
- `vs-cli` crate (M5 step 1): CLI scaffolding.
  - `clap`-derived `Cli` with subcommands for primitives 1–9 plus
    `status`. Global flags: `--session`, `--socket`, `--home`,
    `--no-spawn`, `--json`.
  - `client::Client` — synchronous Unix-socket round-trip (one
    connection per CLI invocation), with `connect_with_retry` for
    immediate-post-spawn races.
  - `active_session` — `~/.vibesurfer/active-session` pointer
    read/write/clear; `vs session-open` writes it on success,
    `vs session-close` clears it.
  - `spawn::spawn_daemon` — detached `vibesurferd` launch (resolves
    the binary via `$VIBESURFERD_BIN`, sibling-of-`vs`, then `$PATH`),
    plus `wait_for_socket` polling.
  - `vs --help` works; `vs status` calls `Daemon::status` (newly
    added; primitive 13).
  - 4 tests: 2 CLI unit (`active_session`) + 2 CLI integration
    (`tests/cli_smoke.rs`) exercising the end-to-end dispatch path
    against an in-process daemon.
  - Deps: `clap` (derive + wrap_help), `anyhow`. Test-only:
    `tempfile`, `tokio` (multi_thread, time), and the daemon stack.

  Remaining for M5 (next iteration): AuditGuard refactor across
  primitives 1–9, primitives 10–19 (extract, mark, annotate, log,
  skill, capture, viewport, layout, auth), end-to-end CLI script
  exercising all 19, wire-test additions for stale-token and
  idempotency.
- M5 architectural cleanup (post-step-1 review):
  - **Single binary.** The `vibesurferd` binary is gone; the `vs`
    binary doubles as the daemon via `vs serve`. `[[bin]]` was
    removed from `vs-daemon/Cargo.toml` and `vs-daemon/src/main.rs`
    deleted. `vs-cli` gained `tokio`, `vs-daemon`, `vs-engine-webkit`,
    `vs-store`, `tracing`, `tracing-subscriber` as regular deps; the
    `serve` module hosts the daemon entrypoint. Auto-spawn re-execs
    `current_exe() serve` (with `$VS_DAEMON_BIN` as an explicit test
    override).
  - **Strict JSON.** `--json` output now goes through `serde_json`
    (RFC-8259 escaping, `to_string_pretty`); the hand-rolled encoder
    is gone. `serde_json` is added to `vs-cli` only — `vs-protocol`
    remains serde-free per ADR 0003.
- `vs-daemon` crate (M4): the integration layer.
  - `Daemon` struct owns a `Store` + `EngineRuntime` + per-session
    in-memory state. One method per primitive 1–9
    (`session_open`, `session_close`, `open`, `close`, `view`, `read`,
    `act`, `find`, `wait`).
  - `tokens::compute` derives the 8-byte `StateToken` from
    `blake3(canonical_tree || url || page_id)`, leveraging
    `vs_protocol::Tree::encode`'s deterministic ordering.
  - `page_state::PageState` tracks per-page caches (`last_tree`,
    `last_token`, `force_full`); `apply_snapshot` returns a
    `ViewForm` (`Full` / `Delta` / `NoChange`) for `vs_view`.
  - `redact::redact_args` masks flags whose names match
    `password|token|secret|key|auth` before the audit log.
  - Idempotency cache check on every `vs_act`; cache hits return
    `? idempotent_hit` without re-executing the engine work.
  - Token freshness check on every `vs_act`; mismatch yields
    `! STALE_TOKEN <current> mutate`.
  - Audit row written for every primitive call (success or failure)
    via `Store::record_action` before returning.
  - `server::serve` runs an async `tokio::net::UnixListener`,
    dispatching wire requests via `spawn_blocking` so the engine
    thread never blocks Tokio workers.
  - `vibesurferd` binary: tracing, store at `~/.vibesurfer/state.db`,
    `EngineRuntime::spawn(StubEngine::new)`, socket at
    `~/.vibesurfer/daemon.sock`. Real WebKit backends arrive in
    M3b / M3c.
  - 24 tests: 19 unit + 5 in-process integration
    (`tests/end_to_end.rs`) + 1 wire-level integration
    (`tests/wire.rs`) exercising actual Unix socket I/O.
  - Deps: `tokio`, `blake3`, `thiserror`, `anyhow`, `tracing`,
    `tracing-subscriber`, `uuid` (v7); test-only `tempfile` and the
    tokio test-util feature.
- `vs-engine-webkit` crate (M3a): the engine layer's architectural
  contract.
  - `Engine` trait + supporting types (`PageHandle`, `ActTarget`,
    `Action`, `WaitCondition`, `CaptureScope`, `Viewport` with M0
    presets, `LayoutBox`, `EngineCapabilities`, `EngineError`).
  - `EngineRuntime` pins the engine to a dedicated OS thread via
    `std::sync::mpsc` channels. Synchronous facade for every trait
    method; drop-safe shutdown joins the thread.
  - `backend::stub::StubEngine` — in-process placeholder that satisfies
    the trait. Lets M4 (daemon) proceed against a real `Engine` impl
    without WebKit on the developer's machine.
  - 18 tests: 16 unit + 2 integration (`tests/runtime_round_trip.rs`)
    exercising a full primitive sequence through the runtime + stub.
  - Real WebKit backends deferred to M3b (WPE on Linux) and M3c
    (system WebKit on macOS); ROADMAP updated.

- `vs-protocol` crate (M1): full encoder/decoder for the wire format.
  Modules: `codes` (Role, Op, ErrorCode, WarningCode), `tree` (Ref,
  Node, Tree, indented full-tree format), `delta` (DeltaOp + encode,
  parse, apply, diff), `envelope` (StateToken, Warning, Envelope,
  ResponseHead), `request` (Request line). 63 tests including 3
  proptest cases: tree round-trip, `apply(diff(A, B), A) == B` via
  random mutations, and DeltaOp encode round-trip. Workspace deps:
  `thiserror = "2"`, `proptest = "1"` (dev).
- `vs-store` crate (M2): SQLite-backed durable state.
  - Numbered migrations (`migrations/0001_initial.sql`) creating all
    eight tables (sessions, pages, refs, marks, annotations, actions,
    auth_blobs, skill_cache) plus `_migrations` bookkeeping. WAL mode,
    `synchronous=NORMAL`, foreign keys on.
  - Full CRUD via `Store` for every table, with typed domain mirrors
    in `vs_store::types`.
  - AES-256-GCM auth blobs via `ring`. `MasterKey` resolves OS keyring
    (service `vibesurfer`, account `default`) → `~/.vibesurfer/key`
    fallback. Inner bytes never reach `Debug`.
  - Idempotency cache: `Store::lookup_idempotent` matches on
    `(page_id, before_token, args_hash)` with a configurable TTL
    (`IDEMPOTENCY_TTL_SECS = 30`). Failed actions are never returned.
  - 24 tests: 21 unit (auth, sessions, pages, refs, marks,
    annotations, actions, idempotency, skill cache) + 3 file-backed
    integration tests under `tests/end_to_end.rs`.
  - Deps: `rusqlite = "0.39"` (bundled), `ring = "0.17"`,
    `keyring = "3"` (per-platform native features),
    `thiserror = "2"`, `tempfile = "3"` (dev).

## [0.0.1] - 2026-05-06

### Added

- Cargo workspace with five crate stubs: `vs-cli`, `vs-daemon`,
  `vs-protocol`, `vs-store`, `vs-engine-webkit`.
- GitHub Actions CI: fmt + clippy on Linux; test + build on a
  `[ubuntu-latest, macos-latest]` matrix. macOS is a peer first-class
  target from M0.
- MIT `LICENSE`.
- Documentation skeleton: `PROTOCOL.md`, `ARCHITECTURE.md`, `ROADMAP.md`,
  `PRIMITIVES.md`, `SKILL.md`, `codes.md`, and five ADRs in
  `docs/decisions/`.
- Empty placeholder directories for `fixtures/`, `tests/`, `skills/`, and
  `crates/vs-store/migrations/`.

This release intentionally contains no working browser code. The aim is to
establish the workspace shape and the documentation that drives every
subsequent milestone.

# Reality check

> Cells reflect the **actual** verification state, not the target.
>
> This table is hand-maintained and nothing enforces it, which has
> bitten once already: the Windows column read `yes` on every row for
> about a month while that job was failing every cell (see
> "Verification — Windows column"). Treat the
> [engine-tests workflow](../.github/workflows/engine-tests.yml) as
> the source of truth and this file as a summary of it. If you edit a
> cell, cite the run you are citing it from. Exact pass counts are
> deliberately not recorded here — they drifted stale too, and the run
> log already has them.

## Cell states

- `yes` — code exists; integration test exists; test passes on its
  native verification environment (Mac on `macos-latest`, Linux on
  bare `ubuntu-latest` with WebKitGTK 6, Windows on `windows-latest`
  with WebView2).
- `partial` — code exists but no test, or test exists but is
  known-failing.
- `unverified` — code and test exist, but no green run on that
  backend backs the claim.
- `no` — neither code nor test.

Target: every cell on every backend `yes` — currently met. All three
platforms are load-bearing jobs in the engine-tests workflow and a
regression on any of them turns the badge red.

## Status

| Capability | Linux WebKitGTK | macOS WKWebView | Windows WebView2 |
|---|---|---|---|
| vs_session_open | yes | yes | yes |
| vs_session_close | yes | yes | yes |
| vs_open | yes | yes | yes |
| vs_close | yes | yes | yes |
| vs_view | yes | yes | yes |
| vs_read | yes | yes | yes |
| vs_act click | yes | yes | yes |
| vs_act fill | yes | yes | yes |
| vs_act scroll | yes | yes | yes |
| vs_act key | yes | yes | yes |
| vs_act submit | yes | yes | yes |
| vs_act hover | yes | yes | yes |
| vs_act focus | yes | yes | yes |
| vs_find | yes | yes | yes |
| vs_wait stable | yes | yes | yes |
| vs_wait net-idle | yes | yes | yes |
| vs_wait ref | yes | yes | yes |
| vs_wait gone | yes | yes | yes |
| vs_wait text | yes | yes | yes |
| vs_wait token-change | yes | yes | yes |
| vs_extract table | yes | yes | yes |
| vs_extract form | yes | yes | yes |
| vs_extract list | yes | yes | yes |
| vs_extract jsonld | yes | yes | yes |
| vs_extract webmcp | yes | yes | yes |
| vs_mark | yes | yes | yes |
| vs_annotate | yes | yes | yes |
| vs_status | yes | yes | yes |
| vs_log | yes | yes | yes |
| vs_skill | yes | yes | yes |
| vs_capture | yes | yes | yes |
| vs_download url | yes | yes | yes |
| vs_download captured | yes | yes | yes |
| vs_download list | yes | yes | yes |
| iframe src in tree (`ifr`) | yes | yes | yes |
| vs_viewport | yes | yes | yes |
| vs_layout | yes | yes | yes |
| vs_auth save | yes | yes | yes |
| vs_auth load | yes | yes | yes |
| vs_inspect console | yes | yes | yes |
| vs_inspect network | yes | yes | yes |
| vs_inspect request | yes | yes | yes |
| vs_inspect eval | yes | yes | yes |
| vs_inspect storage cookies | yes | yes | yes |
| vs_inspect storage local | yes | yes | yes |
| vs_inspect storage session | yes | yes | yes |
| vs_inspect storage indexeddb | yes | yes | yes |
| vs_inspect scripts | yes | yes | yes |
| vs_inspect script | yes | yes | yes |
| vs_inspect dom | yes | yes | yes |
| vs_inspect performance | yes | yes | yes |

## Verification — Mac column

All cells in `crates/vs-cli/tests/m6/*.rs` (including the
capability-gate `cell_engine_unsupported_when_install_disabled`) pass
against the host's real `WKWebView` via the `vs serve` subprocess.
Mac runs the largest set — a few cells are `cfg`'d off on the other
two backends — and is the slowest, because trusted input goes through
real `NSEvent` dispatch and the responder chain.

Sequential execution required (Cocoa main-thread constraint):

```
cargo test --test m6 -- --test-threads=1
```

## Verification — Linux column

The same suite, minus the Mac-only cells, passes on GitHub Actions
`ubuntu-latest` with WebKitGTK 6 + xvfb against the real `WebView`.
WebKitGTK's sandbox needs
unprivileged user namespaces, which Ubuntu's default AppArmor
profile restricts; CI relaxes the restriction with one sysctl.
The capability-gate cell flips correctly when
`VS_DISABLE_INSPECTOR=1` is set: install path short-circuits,
`Engine::capabilities()` reports `inspector_*: false`, daemon's
`vs_inspect` gate fires, `! ENGINE_UNSUPPORTED` flows out on the
wire.

```
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
xvfb-run --auto-servernum cargo test --test m6 -- --test-threads=1
```

For Linux runs on a non-Linux host, the `vs-test-linux` Docker
container in the repo wraps the same setup (needs `--privileged`
for the namespace bit).

## Known CI-only gaps

- **WebAuthn cell on the GitHub macOS runner.** The virtual authenticator
  passes locally on a real Mac (VERIFIED in ~5s) but the hosted
  `macos-latest` runner's WebKit never completes the `crypto.subtle`
  sign/verify, so `cell_auth_webauthn_virtual_authenticator` is skipped
  when `CI` is set (pending-manual-verification). Run it locally to
  verify the feature.

## Verification — Windows column

The same suite passes on GitHub Actions `windows-latest` against the
host's WebView2 runtime via the `vs serve` subprocess. Sequential
execution required for the same reason as Mac (single-threaded COM
STA / Win32 message pump constraint).

```
cargo test --test m6 -- --test-threads=1
```

This column was previously all `yes` on the strength of a claim the
job history did not support: `windows-latest` had been failing every
cell for at least a month. The cause was a third bug, found only once
the job was allowed to run long enough to print its own failure —

- **`STATUS_STACK_OVERFLOW` (0xC00000FD) in `vs serve` at startup** —
  Windows reserves 1 MiB for a process's main thread against 8 on
  Unix, and `serve` does its whole startup there (store open, WebView2
  backend, engine runtime, daemon), which a debug build does not fit
  into. The daemon died before binding its named pipe, so every cell
  failed at the harness's 10s spawn deadline and 71 × 10s pushed the
  job past its own 12-minute cap — killing it before the failure
  summary could print, so the crash stayed invisible. `vs-cli/build.rs`
  now links the binary with an 8 MiB reserve, the harness inlines
  `daemon.log` on spawn failure, and the cap is 25 minutes.

Two earlier bugs surfaced during CI bring-up and are also fixed in tree:

- **`STATUS_ACCESS_VIOLATION` on `Webview2Backend::open`** — the
  host `WNDCLASSW` was registered with `lpfnWndProc: None`, so the
  first window message dispatched into a null pointer. A
  vectored-exception-handler trace
  (`AddVectoredExceptionHandler` in `vs-cli/src/serve.rs`) caught
  the fault with `rip=0` and `rcx` shaped like an HWND, which
  uniquely identified the missing WndProc. Fixed by wiring a
  module-level shim that forwards to `DefWindowProcW`.
- **Inspector trio failing on console/network/request** — the
  `webkit.messageHandlers` → `chrome.webview` bridge shim was
  passing `JSON.stringify(...)` to `chrome.webview.postMessage`.
  `args.WebMessageAsJson()` on the host then returned a
  JSON-encoded string literal that the host parser couldn't
  field-access. Fixed by passing the object directly so
  `WebMessageAsJson` returns the object form.

The SEH handler stays in `vs serve` — if a future regression
crashes the daemon on Windows, the trace lands in
`<home>/daemon.log` and the test harness inlines it on failure.

What's in place:

- `crates/vs-engine-webkit/src/backend/webview2.rs` — full
  implementation against `webview2-com 0.39` for every primitive
  (open, close, snapshot, act, wait, layout, capture, set_viewport,
  save_auth, load_auth, eval_js, storage, scripts, script_source,
  dom, performance, console_entries, network_entries,
  request_detail, capabilities). Inspector capture wired via
  `AddScriptToExecuteOnDocumentCreated` (document-start) +
  `WebMessageReceived` (host-side dispatch).
- `crates/vs-cli/src/serve.rs` — Windows arm: `CoInitializeEx` STA
  on main, `Webview2Backend` constructed there, tokio runtime on a
  worker thread, Win32 message pump (`PeekMessageW` /
  `DispatchMessageW`) on main, draining engine jobs between
  iterations. Mirrors the macOS `NSRunLoop` and Linux
  `glib::MainContext` shapes.

The capability-flag pipeline mirrors Mac/Linux 1:1: `W2Page` carries
`inspector_installed: bool`, `install_inspector` honors
`VS_DISABLE_INSPECTOR=1` for the gate test, `capabilities()`
aggregates per-page bools, the daemon `vs_inspect` gate is platform-
agnostic.

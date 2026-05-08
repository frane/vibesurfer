# Reality check

> Updated continuously as cells land. Cells reflect the **actual**
> verification state at the time of reading, not the target.

## Cell states

- `yes` — code exists; integration test exists; test passes on its
  native verification environment for that backend (Mac native or
  Linux Docker).
- `pending-manual-verification` — code exists; integration test
  exists; CI workflow exists. Awaiting a manual run on a real
  Windows machine.
- `partial` — code exists but no test, or test exists but is
  known-failing. Not a valid end-state for any cell at the close of
  M6.
- `no` — neither code nor test. Not a valid end-state for any cell at
  the close of M6.

End-state target met: every Mac and Linux cell `yes` (94 cells,
including the capability-gate cell), every Windows cell
`pending-manual-verification` (47 cells, including the capability-gate
cell). Total: 141 cells (138 protocol cells + 3 capability-gate
cells, one per backend).

## Status as of M6 third transaction (Mac + Linux + Windows)

| Capability | Linux WebKitGTK | macOS WKWebView | Windows WebView2 |
|---|---|---|---|
| vs_session_open | yes | yes | pending-manual-verification |
| vs_session_close | yes | yes | pending-manual-verification |
| vs_open | yes | yes | pending-manual-verification |
| vs_close | yes | yes | pending-manual-verification |
| vs_view | yes | yes | pending-manual-verification |
| vs_read | yes | yes | pending-manual-verification |
| vs_act click | yes | yes | pending-manual-verification |
| vs_act fill | yes | yes | pending-manual-verification |
| vs_act scroll | yes | yes | pending-manual-verification |
| vs_act key | yes | yes | pending-manual-verification |
| vs_act submit | yes | yes | pending-manual-verification |
| vs_act hover | yes | yes | pending-manual-verification |
| vs_act focus | yes | yes | pending-manual-verification |
| vs_find | yes | yes | pending-manual-verification |
| vs_wait stable | yes | yes | pending-manual-verification |
| vs_wait net-idle | yes | yes | pending-manual-verification |
| vs_wait ref | yes | yes | pending-manual-verification |
| vs_wait gone | yes | yes | pending-manual-verification |
| vs_wait text | yes | yes | pending-manual-verification |
| vs_wait token-change | yes | yes | pending-manual-verification |
| vs_extract table | yes | yes | pending-manual-verification |
| vs_extract form | yes | yes | pending-manual-verification |
| vs_extract list | yes | yes | pending-manual-verification |
| vs_extract jsonld | yes | yes | pending-manual-verification |
| vs_extract webmcp | yes | yes | pending-manual-verification |
| vs_mark | yes | yes | pending-manual-verification |
| vs_annotate | yes | yes | pending-manual-verification |
| vs_status | yes | yes | pending-manual-verification |
| vs_log | yes | yes | pending-manual-verification |
| vs_skill | yes | yes | pending-manual-verification |
| vs_capture | yes | yes | pending-manual-verification |
| vs_viewport | yes | yes | pending-manual-verification |
| vs_layout | yes | yes | pending-manual-verification |
| vs_auth save | yes | yes | pending-manual-verification |
| vs_auth load | yes | yes | pending-manual-verification |
| vs_inspect console | yes | yes | pending-manual-verification |
| vs_inspect network | yes | yes | pending-manual-verification |
| vs_inspect request | yes | yes | pending-manual-verification |
| vs_inspect eval | yes | yes | pending-manual-verification |
| vs_inspect storage cookies | yes | yes | pending-manual-verification |
| vs_inspect storage local | yes | yes | pending-manual-verification |
| vs_inspect storage session | yes | yes | pending-manual-verification |
| vs_inspect storage indexeddb | yes | yes | pending-manual-verification |
| vs_inspect scripts | yes | yes | pending-manual-verification |
| vs_inspect script | yes | yes | pending-manual-verification |
| vs_inspect dom | yes | yes | pending-manual-verification |
| vs_inspect performance | yes | yes | pending-manual-verification |

## Verification — Mac column

48 of 48 tests in `crates/vs-cli/tests/m6/{lifecycle,act,wait,
extract,visual,auth,memory,inspect}.rs` (including the capability-
gate `cell_engine_unsupported_when_install_disabled`) pass against
the host's real `WKWebView` via the `vs serve` subprocess. Sequential
execution required (Cocoa main-thread constraint).

```
cargo test --test m6 -- --test-threads=1
# → test result: ok. 48 passed; 0 failed; finished in ~38s
```

## Verification — Linux column

Same 48 tests pass inside `vs-test-linux` Docker container running
Ubuntu 24.04 + WebKitGTK 6 + xvfb against the real `WebView`. The
capability-gate cell flips correctly when `VS_DISABLE_INSPECTOR=1` is
set: install path short-circuits, `Engine::capabilities()` reports
`inspector_*: false`, daemon's `vs_inspect` gate fires,
`! ENGINE_UNSUPPORTED` flows out on the wire.

```
docker run --rm --privileged \
  -v "$PWD":/work \
  -v vs-target-linux:/work/target-linux \
  -v vs-cargo-linux:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/work/target-linux \
  vs-test-linux
# → test result: ok. 48 passed; 0 failed; finished in ~28s
```

## Verification — Windows column

Code, tests, and CI workflow all shipped. The Windows column stays
at `pending-manual-verification` because the M6 work was authored on
a macOS host with no access to a Windows machine. The full M6 suite
runs on `windows-latest` via `.github/workflows/m6.yml` (the
`continue-on-error: true` job lets the matrix surface failures
without blocking PRs); a green run there + a manual confirmation
flips the column to `yes`.

What's in place:

- `crates/vs-engine-webkit/src/backend/webview2.rs` — full
  implementation against `webview2-com 0.39.1` for every primitive
  (open, close, snapshot, act, wait, layout, capture, set_viewport,
  save_auth, load_auth, eval_js, storage, scripts, script_source,
  dom, performance, console_entries, network_entries,
  request_detail, capabilities). Inspector capture wired via
  `AddScriptToExecuteOnDocumentCreated` (document-start) +
  `WebMessageReceived` (host-side dispatch). The shim that maps
  `webkit.messageHandlers.<name>.postMessage` onto
  `chrome.webview.postMessage` is documented in the install path.
- `crates/vs-cli/src/serve.rs` — Windows arm: `CoInitializeEx` STA
  on main, `Webview2Backend` constructed there, tokio runtime on a
  worker thread, Win32 message pump (`PeekMessageW` /
  `DispatchMessageW`) on main, draining engine jobs between
  iterations. Mirrors the macOS `NSRunLoop` and Linux
  `glib::MainContext` shapes.
- `crates/vs-cli/tests/support/mod.rs` —
  `Backend::Windows::available_on_current_platform()` returns
  `cfg!(target_os = "windows")` so the existing 48 cell tests
  activate on the Windows runner.
- `.github/workflows/m6.yml` — three-job matrix (mac native, linux
  docker, windows native). Mac + Linux are required; Windows has
  `continue-on-error: true` per the policy above.

The capability-flag pipeline mirrors Mac/Linux 1:1: `W2Page` carries
`inspector_installed: bool`, `install_inspector` honors
`VS_DISABLE_INSPECTOR=1` for the gate test, `capabilities()`
aggregates per-page bools, the daemon `vs_inspect` gate is platform-
agnostic.

The capability-gate cell
(`cell_engine_unsupported_when_install_disabled`) targets the same
`VS_DISABLE_INSPECTOR=1` env var on Windows, so it'll either flip
correctly there or surface a real bug — no special-casing.

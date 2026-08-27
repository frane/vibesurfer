# Reality check

> Updated continuously as cells land. Cells reflect the **actual**
> verification state at the time of reading, not the target.

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

Target: every cell on every backend `yes`. **Not currently met — the
Windows column is unverified across the board** (see below). Mac and
Linux are load-bearing jobs in the engine-tests workflow and a
regression there turns the badge red.

## Status

| Capability | Linux WebKitGTK | macOS WKWebView | Windows WebView2 |
|---|---|---|---|
| vs_session_open | yes | yes | unverified² |
| vs_session_close | yes | yes | unverified² |
| vs_open | yes | yes | unverified² |
| vs_close | yes | yes | unverified² |
| vs_view | yes | yes | unverified² |
| vs_read | yes | yes | unverified² |
| vs_act click | yes | yes | unverified² |
| vs_act fill | yes | yes | unverified² |
| vs_act scroll | yes | yes | unverified² |
| vs_act key | yes | yes | unverified² |
| vs_act submit | yes | yes | unverified² |
| vs_act hover | yes | yes | unverified² |
| vs_act focus | yes | yes | unverified² |
| vs_find | yes | yes | unverified² |
| vs_wait stable | yes | yes | unverified² |
| vs_wait net-idle | yes | yes | unverified² |
| vs_wait ref | yes | yes | unverified² |
| vs_wait gone | yes | yes | unverified² |
| vs_wait text | yes | yes | unverified² |
| vs_wait token-change | yes | yes | unverified² |
| vs_extract table | yes | yes | unverified² |
| vs_extract form | yes | yes | unverified² |
| vs_extract list | yes | yes | unverified² |
| vs_extract jsonld | yes | yes | unverified² |
| vs_extract webmcp | yes | yes | unverified² |
| vs_mark | yes | yes | unverified² |
| vs_annotate | yes | yes | unverified² |
| vs_status | yes | yes | unverified² |
| vs_log | yes | yes | unverified² |
| vs_skill | yes | yes | unverified² |
| vs_capture | yes | yes | unverified² |
| vs_download url | yes | yes | unverified² |
| vs_download captured | yes | yes | unverified² |
| vs_download list | yes | yes | unverified² |
| iframe src in tree (`ifr`) | yes | yes | unverified² |
| vs_viewport | yes | yes | unverified² |
| vs_layout | yes | yes | unverified² |
| vs_auth save | yes | yes | unverified² |
| vs_auth load | yes | yes | unverified² |
| vs_inspect console | yes | yes | unverified² |
| vs_inspect network | yes | yes | unverified² |
| vs_inspect request | yes | yes | unverified² |
| vs_inspect eval | yes | yes | unverified² |
| vs_inspect storage cookies | yes | yes | unverified² |
| vs_inspect storage local | yes | yes | unverified² |
| vs_inspect storage session | yes | yes | unverified² |
| vs_inspect storage indexeddb | yes | yes | unverified² |
| vs_inspect scripts | yes | yes | unverified² |
| vs_inspect script | yes | yes | unverified² |
| vs_inspect dom | yes | yes | unverified² |
| vs_inspect performance | yes | yes | unverified² |

² The whole Windows column. The `windows-latest` engine-tests job has
not been green: every cell fails at the harness's 10s daemon-spawn
deadline, which is also why the job used to hit its own time cap
before printing the failure summary. This column previously read
`yes` throughout, which the job history does not support. It stays
`unverified` until a green `windows-latest` run says otherwise.

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

Same 48 tests pass on GitHub Actions `ubuntu-latest` with WebKitGTK
6 + xvfb against the real `WebView`. WebKitGTK's sandbox needs
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
# → test result: ok. 48 passed; 0 failed; finished in ~30s
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

**Currently red. Do not read the code inventory below as verification.**

The `windows-latest` engine-tests job fails every cell. The signature
is uniform: each test dies exactly 10.0s after the previous one, which
is the `spawn_daemon` deadline in
`crates/vs-cli/tests/support/mod.rs` — the daemon never opens its
named pipe, so `TestContext::start()` panics and each cell is a
spawn failure rather than a real result. 71 cells × 10s is also what
used to push the job past its old 12-minute cap, killing it before
the failure summary (and the inlined `daemon.log`) could print. The
cap is now 25 minutes and the harness inlines the daemon log on spawn
failure, so the next run should carry the actual cause.

The section previously claimed "Same 48 tests pass on GitHub Actions
`windows-latest`" with a green sample output. The job history does
not support that, so it has been removed rather than left standing.

Sequential execution is required for the same reason as Mac
(single-threaded COM STA / Win32 message pump constraint).

Two bugs surfaced during CI bring-up and are fixed in tree:

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

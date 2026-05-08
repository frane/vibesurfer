# M6 plan — verification reality

> Status: drafted 2026-05-07 at the start of M6. Updated as cells land.

## Goal

Every cell in `docs/REALITY_CHECK.md` reaches its target state:

- **Mac (`yes`)** — verified locally on the host's WKWebView.
- **Linux (`yes`)** — verified inside a Docker container with WebKitGTK 6 + xvfb.
- **Windows (`pending-manual-verification`)** — code + test + CI workflow shipped, awaiting hands-on a Windows machine.

Capability flags advertised by each backend match the table exactly. A flag is `true` only if the runtime install path actually succeeded for that page; never hardcoded.

## Order

The order interleaves shared work with platform work so a failure on one platform surfaces against a known-good reference, not in a vacuum.

1. **Fixtures + harness** (this transaction). Build the fixture HTTP server and the 13 fixture HTML pages. Extract the daemon-spawn pattern from `e2e_cli.rs` into a shared test-support module so the same harness drives Mac (host), Linux (docker), and Windows (CI). No real tests yet beyond a smoke test.
2. **Mac column to all-yes** (this transaction). Drive every primitive + every inspect subcommand against real WKWebView via the harness. One integration test per cell. Each test loads a fixture URL, exercises the cell through the public CLI, and asserts a real-engine outcome.
3. **Report to user, sanity-check the harness shape**. Before replicating for two more backends, the user reviews the Mac patterns and signs off (or redirects).
4. **Linux column** (next transaction). Add `Dockerfile` + `docker-compose.yml` with WebKitGTK 6 + xvfb + the fixture server's runtime deps. The integration test target for `Backend::Linux` runs inside the container; `cargo test --features backend-linux` invokes it. Iterate until each Linux cell hits `yes`.
5. **Windows column** (next transaction). Implement WebView2 backend from skeleton to full coverage by reading the WebView2 docs. Write tests that target it. Add the CI workflow that runs the suite on a Windows runner. Mark every cell `pending-manual-verification` until the user runs them on a real Windows machine.
6. **CI workflow + DEVELOPMENT.md**. GitHub Actions for Mac native + Linux Docker + Windows runner. `docs/DEVELOPMENT.md` documents the local-iteration loop for each platform.
7. **Final REALITY_CHECK.md**. Populated with the actual end state.

## Why fixtures + harness first

- Validates the infrastructure shape against a real backend before replicating it for two more.
- Surfaces protocol-level issues (a primitive that's hard to verify against any real engine) before the same code is written three times.
- Mac is the platform that iterates fastest right now; lock the patterns there, port outward.

## What the harness looks like

```
crates/vs-cli/tests/
  support/
    mod.rs              — pub items: FixtureServer, DaemonGuard, vs(), token_of(), …
    fixture_server.rs   — tiny_http server, serves fixtures/ on a random port
    fixtures.rs         — paths, name → URL helper
  m6_*.rs               — one file per logical group of cells
fixtures/               — 13 HTML files (workspace root, source-controlled)
```

`Backend` enum lives in the support module and currently has one branch — `Mac`. Linux + Windows branches land in their respective transactions.

## Capability-flag discipline

Per rule #6 of the kickoff: every `inspector_console: bool` etc. is computed from feature detection, not hardcoded. The Mac install path that fails partway returns false until it succeeds. The Windows install path is `false` until the runtime feature-detection flips it on.

Concretely: `WkPage` and `WpePage` carry an `inspector_installed: bool` set when the user-script + handlers register without error. `capabilities()` reads from a per-instance state, not a `const`.

## Audit + commit cadence

Per binding rule #2: one `ae` transaction per logical chunk; commit when build + test + clippy all green. This document is updated in the same transaction as the work it describes — the plan and the work travel together.

## Binding rules (locked during M6)

These constraints emerged during the work and become part of the
project rules going forward. They prevent the same gap from
recurring for any future primitive.

1. **Wire vocab and CLI surface stay in sync.** Any wire-level
   primitive that the daemon recognizes must be reachable through
   the public `vs` CLI. A wire-only primitive can't be exercised by
   integration tests through the public surface, which violates the
   M6 verification standard. (Surfaced when M6 discovered `vs_inspect`
   was wired in the daemon but missing from clap; the gap predated
   the milestone.)

2. **Capability flags are computed, never declared.** Every flag
   returned by `Engine::capabilities()` reflects the actual install
   state for the engine instance. Backends store per-page bools that
   the install path stamps on success; `capabilities()` aggregates
   them. The daemon gates the relevant primitive: when a flag is
   `false`, the wire returns `! ENGINE_UNSUPPORTED <op>` rather than
   an empty buffer. Locked by the
   `cell_engine_unsupported_when_install_disabled` test
   (`VS_DISABLE_INSPECTOR=1` forces the install to short-circuit; the
   flag goes false; the wire returns the correct error).

3. **Test runs are sequential per platform.** Every M6 test target
   pumps a real GUI engine (Cocoa main thread on macOS, GLib main
   context on Linux WebKitGTK, Win32 message loop on WebView2).
   Parallel runs saturate the platform's main thread/loop and produce
   timing-sensitive flakes. `cargo test --test m6 -- --test-threads=1`
   is the canonical invocation; documented in `docs/DEVELOPMENT.md`.

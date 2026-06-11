# Roadmap

> **Status: historical.** Milestones M0–M6 all shipped (v0.1.x). This
> document is kept as the record of the original build order and exit
> criteria; it is not a forward-looking plan. Post-M6 work is tracked
> in the [CHANGELOG](../CHANGELOG.md) per release. Note the primitive
> counts below describe the M5-era surface (19 primitives); v0.1.8+
> added `vs_inspect`, the cursor primitives, prompt-input, and the
> pending queue (29 wire primitives as of v0.1.13).

Build order is sequential. Each milestone has explicit exit criteria.
Stop at every milestone for review before starting the next.

## M0 — Repo skeleton + CI

**Goal.** Establish the workspace shape and the documentation that drives
every subsequent milestone. No working browser code.

**Exit criteria.**
- Workspace and all crate stubs compile.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all
  pass.
- GitHub Actions CI on Linux runs fmt, clippy, test, build.
- `docs/PROTOCOL.md` and `docs/ARCHITECTURE.md` drafted.
- `CHANGELOG.md` exists with `0.0.1`.
- MIT `LICENSE`.
- Five ADRs in `docs/decisions/` — engine choice, no transactions, line
  protocol, stable refs, tree deltas.

## M1 — `vs-protocol` crate

**Goal.** A pure encoder/decoder for the wire format. No engine, no
SQLite.

**Exit criteria.**
- Envelope, request, tree, delta, error, warning all encode and parse.
- Round-trip property tests: encode → decode → equal for random valid
  trees, requests, and deltas.
- Diff algorithm: `apply(diff(A, B), A) == B` for random tree pairs.
- Role-code, error-code, warning-code enums with `From<&str>` and
  `Display`. Tables match [`docs/codes.md`](codes.md).

## M2 — `vs-store` crate

**Goal.** SQLite as the source of truth.

**Exit criteria.**
- Numbered migrations create the full schema (sessions, pages, refs,
  marks, annotations, actions, auth_blobs, skill_cache).
- CRUD coverage for every table.
- AES-256-GCM round-trip for auth blobs via `ring`, key from OS keyring
  with fallback to `~/.vibesurfer/key`.
- Idempotency cache lookup by `(page_id, before_token, args_hash)` with
  30s TTL.
- Integration tests against a temporary SQLite file.

## M3 — `vs-engine-webkit`

The original M3 was scoped to "real WebKit on Linux + macOS in one
milestone." Real FFI on both platforms turned out to be too large for a
single review-stop unit (Cocoa main-thread plumbing on macOS, WPE's
`pkg-config` dance and GMainLoop bridge on Linux). M3 is therefore split
into three review-stops sharing one workspace state.

### M3a — `Engine` trait, runtime, stub backend

**Goal.** Lock down the architectural contract and let M4 (daemon) build
against a real `Engine` impl without WebKit-on-developer-machine
prerequisites.

**Exit criteria.**
- `Engine` trait + supporting types (`PageHandle`, `ActTarget`,
  `Action`, `WaitCondition`, `CaptureScope`, `Viewport`, `LayoutBox`,
  `EngineCapabilities`, `EngineError`) defined in `vs-engine-webkit`.
- `EngineRuntime` pins a constructor closure to a dedicated OS thread
  via `std::sync::mpsc` channels. Synchronous facade for every trait
  method. Drop-safe shutdown that joins the thread.
- `backend::stub::StubEngine` — in-memory implementation that satisfies
  the trait, returns a canned a11y tree on `snapshot`, exercises every
  primitive in unit tests.
- Integration test (`tests/runtime_round_trip.rs`) drives a full
  primitive sequence (open → wait → snapshot → act → save_auth →
  close) through `EngineRuntime` + `StubEngine`.

### M3b — WPE WebKit on Linux

**Goal.** Real WebKit pixels on Linux.

**Exit criteria.**
- `backend::wpe` linked via FFI (or `webkit2gtk-sys` as the documented
  fallback; ADR 0001 records the choice).
- Engine thread owns its own `GMainLoop`; commands enqueued onto a
  `glib::MainContext` source.
- `open`, `close`, `snapshot`, `act` (click + fill), `set_viewport`,
  `capture`, `wait` (`stable`, `net-idle`, `ref`) implemented.
- Integration tests against `fixtures/static.html` pass on
  `ubuntu-latest` in CI.

### M3c — system WebKit on macOS

**Goal.** Real WebKit pixels on macOS — peer to M3b, no Docker detour.

**Exit criteria.**
- `backend::webkit` via `objc2` + `objc2-web-kit` + `objc2-foundation`
  + `objc2-app-kit`. `WKWebView` parented to a hidden offscreen
  `NSWindow`, driven from a thread that owns a `CFRunLoop`.
- Same trait method coverage as M3b.
- Same integration tests against `fixtures/static.html` pass on
  `macos-latest` in CI.
- Known per-platform divergences recorded in `docs/known-issues.md`.

## M4 — `vs-daemon`

**Goal.** End-to-end primitives 1 through 9 over the Unix socket.

**Exit criteria.**
- Socket server, request parser using `vs-protocol`.
- Session and page lifecycle backed by `vs-store`.
- Stable ref allocator, last-tree cache per `(page, agent)`.
- Tree delta on `vs_view`; full tree on first call, navigation, viewport
  change, auth load.
- State token computed on every read, validated on every write.
- Audit row written for every primitive call (success or failure) before
  returning.
- Idempotency cache consulted before engine work.
- Implemented: `vs_session_open`, `vs_session_close`, `vs_open`,
  `vs_close`, `vs_view`, `vs_read`, `vs_act`, `vs_find`, `vs_wait`.

## M5 — `vs-cli` and remaining primitives

**Goal.** All 19 primitives reachable from the CLI.

**Exit criteria.**
- `vs` binary with one subcommand per primitive.
- Daemon auto-spawn, active-session file management.
- Daemon implements primitives 10 through 19: `vs_extract`, `vs_mark`,
  `vs_annotate`, `vs_status`, `vs_log`, `vs_skill`, `vs_capture`,
  `vs_viewport`, `vs_layout`, `vs_auth`.
- End-to-end integration test exercises all 19 primitives and verifies
  SQLite state.

## M6 — Bootstrap docs and reference skills

**Goal.** A new agent can pick up the protocol from `SKILL.md` alone and
do a real task within a 600-token bootstrap budget.

**Exit criteria.**
- `docs/SKILL.md` fits within 600 tokens, covers envelope, tree format,
  delta ops, all 19 primitives, viewport presets, wait conditions, common
  codes, and one worked end-to-end example.
- `skills/responsive-review/` and `skills/download-with-confirm/` ship as
  reference skills.
- README quickstart and a terminal-cast demo of an agent doing a real
  task.

## Beyond M6 (not on the v1 critical path)

- ~~Windows engine support.~~ *Shipped: the WebView2 backend implements
  every primitive and the M6 suite runs on `windows-latest` in CI.*
- ~~MCP shim (translates the protocol for Anthropic-style tool use).~~
  *Shipped: `vs mcp`.*
- Branching skill scripts (currently linear only).
- CDP shim (for Playwright/Puppeteer compatibility, if there's demand).
- Network exposure of the daemon (with its own auth story).

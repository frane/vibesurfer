# Architecture

vibesurfer is split into a stateless CLI, a long-running daemon that owns
the engine, a SQLite store for durable state, and an FFI crate that talks
to WebKit (WPE on Linux, the system framework on macOS). The wire protocol
between them is documented separately in [`PROTOCOL.md`](PROTOCOL.md).

```
┌─────────────┐                ┌──────────────────────────────────────────┐
│   vs CLI    │ unix socket    │             vibesurferd                  │
│  (stateless)│ ─────────────▶ │                                          │
└─────────────┘                │  ┌────────────┐    ┌──────────────────┐  │
                               │  │  server    │    │  engine thread   │  │
                               │  │  server    │    │  engine thread   │  │
                               │  │  session   │◀──▶│  (run loop)      │  │
                               │  │  page      │    │  WebKit + JSCore │  │
                               │  │  tokens    │    └──────────────────┘  │
                               │  │  audit     │                          │
                               │  │  auth      │    ┌──────────────────┐  │
                               │  └────────────┘    │  SQLite (WAL)    │  │
                               │         ▲           │  ~/.vibesurfer/  │  │
                               │         └──────────▶│  state.db        │  │
                               │                     └──────────────────┘  │
                               └──────────────────────────────────────────┘
```

## Components

### `vs` — the CLI

- One binary, one subcommand per primitive. Reads `~/.vibesurfer/active-session`
  to find the current session id (override with `--session=<id>`).
- On invocation, checks for `~/.vibesurfer/daemon.sock`. If missing, spawns
  `vibesurferd` (double-fork detached), waits up to 2s, prints
  `! DAEMON_START_FAILED` on timeout.
- Default output is the line protocol from [`PROTOCOL.md`](PROTOCOL.md).
  `--json` switches to JSON for humans/debugging; agents never use this.
- Exit codes: `0` success, `1` error envelope, `2` warnings + success.

### `vibesurferd` — the daemon

- Owns the Unix socket and dispatches requests to handlers.
- Manages session and page state. Lifetime of a session is one
  `vs_session_open`/`vs_session_close` pair.
- Holds in-memory caches (last emitted tree per `(page, agent)`, ref
  allocator, idempotency cache) that are derivative — on restart they are
  rebuilt from SQLite.
- Dispatches engine work to a dedicated WebKit thread (see below).
- Writes one row to `actions` for every primitive before returning. Audit
  is mandatory; if the SQLite write fails, the primitive fails.

### Engine boundary

`crates/vs-daemon/src/engine/mod.rs` defines the `Engine` trait. The
production implementation lives in `crates/vs-engine-webkit` and links a
WebKit port appropriate for the host platform — WPE WebKit on Linux, the
system WebKit framework on macOS. The crate is feature-gated so the
protocol and store crates can build without any WebKit installed.

The trait is deliberately small and synchronous from the daemon's
perspective: open/close, snapshot the a11y tree, perform an action, wait
for a condition, capture, layout, set viewport, save/load auth, and report
capabilities. Actions that an engine cannot perform (e.g. a future
text-only engine that does not render) return `EngineError::Unsupported`,
which the daemon surfaces as `! ENGINE_UNSUPPORTED`.

### Engine thread

WebKit, on either platform, must be driven from the thread that owns its
run loop — `GMainLoop` on Linux (glib's threading rules), `CFRunLoop` /
`NSRunLoop` on macOS (Cocoa's). Tokio's async workers cannot call WebKit
directly on either platform. `vs-engine-webkit` runs WebKit on a
dedicated OS thread that owns the platform's run loop, and bridges the
boundary with mpsc channels:

```
tokio task ──(Command + oneshot reply)──▶ engine thread ── WebKit + JSCore
tokio task ◀──(Result via oneshot)─────── engine thread
```

Commands are enqueued onto the platform run loop (`glib::MainContext`
sources on Linux, `CFRunLoopSource` / `dispatch_async` on macOS); results
travel back over a `tokio::sync::oneshot`. The daemon never blocks a
tokio worker on the engine, and never calls WebKit from a worker thread.
or vice versa.

### `vs-store` — SQLite

- Single file at `~/.vibesurfer/state.db`, WAL mode, foreign keys on,
  `PRAGMA synchronous=NORMAL`.
- Schema is defined by numbered migrations in
  `crates/vs-store/migrations/`.
- One write connection per session; read-only connections for log queries.
- Tables: `sessions`, `pages`, `refs`, `marks`, `annotations`, `actions`,
  `auth_blobs`, `skill_cache`. Schema lives in [`PRIMITIVES.md`](PRIMITIVES.md)
  and the migrations.

### `vs-protocol` — wire format

- Pure encoder/decoder, no IO. Both `vs-cli` and `vs-daemon` link this.
- Tree representation, delta operations, role/error/warning code tables,
  envelope parsing, request parsing.
- Property-tested round-trip: encode → string → decode → equal.

## Process model

- Daemon is one OS process. Tokio runtime for IO. One additional thread
  for WebKit + GMainLoop. Audit and SQLite writes are synchronous from
  the handler's perspective (durability over throughput).
- CLI is a short-lived process per invocation. No persistent caches; the
  daemon authoritatively answers every question.
- One daemon per user (the socket path is per-`$HOME`). Concurrent agents
  share the daemon and disambiguate via `--session=<id>`.

## State location

```
~/.vibesurfer/
  daemon.sock           # the IPC socket
  active-session        # one-line file with the active session id
  state.db              # SQLite, WAL mode
  state.db-wal
  state.db-shm
  log/
    vibesurferd.log     # rotating tracing output
  key                   # AES-256 key fallback if OS keyring unavailable
```

`state.db` is the source of truth. Anything held in memory in the daemon
is reconstructible from it.

## Failure model

- Daemon crash: socket vanishes; next CLI invocation respawns the daemon,
  which rebuilds caches from SQLite. Open pages are lost (a browser tab
  cannot survive a process restart); sessions persist as records but page
  state is gone, marked `closed_at = <restart_time>`.
- Engine OOM: WebKit thread is killed; daemon emits `! ENGINE_CRASH` on
  any in-flight request and refuses new page work until the engine thread
  is respawned. Sessions stay open; page-level state is rebuilt on next
  `vs_open`.
- SQLite write failure: primitive fails. There is no "skip audit" path.
- Stale state token: rejected with `! STALE_TOKEN <new> <reason>`. Agent
  re-reads and retries.

## What lives where

| Concern                            | Crate              |
| ---------------------------------- | ------------------ |
| Wire format (parse/serialize)      | `vs-protocol`      |
| Tree, delta, role/error tables     | `vs-protocol`      |
| SQLite schema, migrations, queries | `vs-store`         |
| Auth blob encryption (`ring`)      | `vs-store`         |
| Session/page state, dispatch       | `vs-daemon`        |
| Stable ref allocator, token calc   | `vs-daemon`        |
| Audit log writer                   | `vs-daemon`        |
| `Engine` trait                     | `vs-daemon`        |
| WebKit FFI + run-loop bridging     | `vs-engine-webkit` |
| Daemon auto-spawn, active session  | `vs-cli`           |
| `clap` parsing, argv → request     | `vs-cli`           |

## Out of scope (v1)

- TCP/HTTP exposure of the daemon.
- Windows. Linux and macOS are peer first-class targets from M3.
- A second engine (Lightpanda, headless Chromium). The trait exists so
  this is *possible*; v1 ships one.
- Distributed session state. One daemon per user, one host per session.

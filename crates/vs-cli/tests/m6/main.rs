//! M6 real-browser integration suite — entry point.
//!
//! Cargo discovers `tests/<name>/main.rs` as a single test binary
//! named `<name>` (here: `m6`). All cell tests live in submodules
//! grouped by primitive family. The shared harness (fixture server,
//! daemon spawn, CLI helpers) is at `crates/vs-cli/tests/support/`
//! and reached via `#[path]` so `m6_smoke.rs` and this binary share
//! exactly one harness.
//!
//! Per-test pattern (every cell):
//!   1. `TestContext::start()` spawns a tiny_http fixture server and
//!      a real `vs serve` subprocess (WkBackend on macOS, WpeBackend
//!      on Linux).
//!   2. The test drives the cell through the public `vs` CLI.
//!   3. The test asserts an observable outcome that could only be
//!      true if the real engine did the work.
//!
//! Run sequentially: `cargo test --test m6 -- --test-threads=1`.

#![allow(clippy::missing_panics_doc)]

#[path = "../support/mod.rs"]
pub(crate) mod support;

pub(crate) mod helpers;

mod act;
mod auth;
mod download;
mod extract;
mod inspect;
mod lifecycle;
mod memory;
mod visual;
mod wait;
mod webentry;

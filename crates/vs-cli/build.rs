//! Build script for the `vs` binary.
//!
//! Its only job is the Windows main-thread stack size.
//!
//! Windows reserves 1 MiB for a process's main thread; the Unix
//! platforms we target reserve 8. `vs serve` does its whole startup on
//! main — open the SQLite store, construct the WebView2 backend, build
//! the engine runtime and the daemon — and in a debug build (no
//! inlining, every temporary materialized) that path does not fit in
//! 1 MiB. The daemon died with STATUS_STACK_OVERFLOW (0xC00000FD)
//! before it ever bound its named pipe, so every m6 cell on
//! `windows-latest` failed at the harness's 10s spawn deadline and the
//! Windows column of docs/REALITY_CHECK.md was never real.
//!
//! Raising the reserve is the fix rather than restructuring startup:
//! the WebView2 STA and its Win32 message pump have to stay on main,
//! so the work cannot simply move to a `Builder::stack_size` thread.
//!
//! This goes through `rustc-link-arg-bins` instead of `rustflags` in
//! `.cargo/config.toml` on purpose — a `RUSTFLAGS` env var (which
//! `ci.yml` sets) overrides config `rustflags` wholesale, which would
//! silently drop the flag in exactly the CI job that needs it.

/// 8 MiB, matching the Unix default.
const STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    if target.contains("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:{STACK_BYTES}");
    } else {
        // windows-gnu goes through ld, which spells it differently.
        println!("cargo:rustc-link-arg-bins=-Wl,--stack,{STACK_BYTES}");
    }
}

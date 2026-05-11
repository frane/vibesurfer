//! Pure-math input synthesis for vibesurfer's three input modes.
//!
//! The crate is intentionally narrow: given a start and end point,
//! some text to type, or a scroll delta, return a sequence of
//! discrete `Step`s tagged with relative time. The engine crate
//! consumes that sequence and translates each step into a
//! platform-native event (`NSEvent` on macOS, `GdkEvent` on Linux,
//! `Input.dispatchMouseEvent` over CDP on Windows).
//!
//! The split keeps the math testable without WebKit and the
//! per-platform code testable without doing humanization math. It
//! also concentrates the realism work in one place: improving the
//! Bezier sampler or the keystroke distribution lifts every backend
//! at once.
//!
//! ## Modes
//!
//! | Mode    | Sequence shape                                |
//! |---------|-----------------------------------------------|
//! | Human   | Bezier path, Fitts arrival, digraph keys      |
//! | Careful | Single-shot dispatch, no path or per-key delay|
//! | Robotic | Empty sequence — caller falls back to JS path |
//!
//! See [`InputMode`] for details.
//!
//! ## Determinism
//!
//! Every entry point takes a `seed: u64`. Same seed + same inputs
//! produce byte-identical output. This lets tests assert exact
//! sequences and lets the daemon persist a per-session seed so a
//! given agent produces consistent typing patterns across runs
//! against the same site.

mod keys;
mod mouse;
mod rng;
mod scroll;

pub use keys::{key_sequence, Key, KeyStep, KeyStepKind};
pub use mouse::{mouse_path, MouseButton, MouseStep, MouseStepKind, Point};
pub use scroll::{scroll_sequence, Vec2, WheelStep};

/// Which input style to synthesize.
///
/// `Human` produces multi-event sequences with realistic timing.
/// `Careful` produces single-shot sequences with no behavioral
/// synthesis but still uses the trusted-event dispatch path on the
/// engine side. `Robotic` produces an empty sequence — the engine
/// reads the empty vector as a signal to fall back to JS synthetic
/// dispatch (the historical vibesurfer behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputMode {
    /// Bezier paths, Fitts-law arrival times, digraph-distributed
    /// keystroke gaps, occasional typo + correction. Bounded total
    /// duration (see per-function docs).
    Human,
    /// Single-shot trusted dispatch. No paths, no per-key delays. The
    /// "instant" mode for power users who don't need detection
    /// resistance.
    Careful,
    /// Empty sequence — the engine falls back to its historical
    /// JS-synthetic event dispatch. Provided so callers don't branch
    /// on the mode themselves; they always call into `vs-humanize`
    /// and the empty `Vec` is the contract for "skip me."
    Robotic,
}

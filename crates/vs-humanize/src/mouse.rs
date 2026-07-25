//! Mouse path synthesis.
//!
//! `Human`: cubic Bezier from start to end with two perpendicular
//! control points whose magnitude is a fraction of the path length;
//! sampled at 16 ms (≈ 60 Hz). Total duration follows Fitts' law
//! (Fitts, 1954) with a fixed target width — the function signature
//! doesn't expose width, so we use a conservative default that
//! approximates a typical clickable region.
//!
//! `Careful`: a single `Move` at the endpoint followed by the click
//! triple (`Down`, `Up`, `Click`). No path; the engine's trusted
//! dispatch handles the rest.
//!
//! `Robotic`: empty `Vec`. Engine falls back to JS synthetic
//! dispatch.
//!
//! Bezier control-point construction picks the perpendicular offset
//! direction from a coin flip and the magnitude from a uniform draw
//! in [0.05·L, 0.25·L] — gives a visible curve without veering wildly
//! off-axis. Overshoot is implemented by carrying the path 5 px past
//! the endpoint along the direct-line direction then returning, which
//! shows up in the sampled sequence as a brief reverse `Move` near
//! the end.

use std::time::Duration;

use crate::rng::Rng;
use crate::InputMode;

/// 2D point in CSS pixels. The caller picks the coordinate frame
/// (page-local or viewport-local); we just emit numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Which physical button the step refers to. Vibesurfer only uses
/// `Left` today, but `Middle` and `Right` are exposed so future
/// primitives don't need a wire-format break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// What kind of mouse event this step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseStepKind {
    /// Pointer movement only — no button change.
    Move,
    /// Button press.
    Down,
    /// Button release.
    Up,
    /// Logical click (DOM `click` event). Some engines synthesize
    /// this from a `Down`/`Up` pair; emitting it explicitly lets the
    /// engine choose.
    Click,
}

/// One mouse event in a synthesized sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseStep {
    /// Time since the start of the sequence.
    pub at: Duration,
    /// Coordinates at this instant.
    pub point: Point,
    /// Event kind.
    pub kind: MouseStepKind,
    /// Which button (only meaningful for `Down` / `Up` / `Click`).
    pub button: MouseButton,
}

// --- Tuning constants ---

/// Fitts' law intercept: time even a zero-distance move requires
/// before the click sequence can start. Includes neural latency.
const FITTS_A_MS: f64 = 100.0;

/// Fitts' law slope: ms per bit of index-of-difficulty.
const FITTS_B_MS: f64 = 150.0;

/// Assumed target width for Fitts ID computation. We don't take it
/// as a parameter because the engine doesn't always know the target
/// rect when this function is called. 32 px is the typical clickable
/// target on a desktop site (a button, an icon).
const FITTS_TARGET_WIDTH_PX: f64 = 32.0;

/// Hard cap on Bezier path total duration. PR 5 in M7 will surface a
/// `?slow_dispatch` warning when this cap is hit; the math here only
/// enforces it as a saturation.
const HUMAN_MOVE_CAP_MS: f64 = 1200.0;

/// Sampling period — 16 ms is roughly 60 Hz, which matches the
/// dispatch rate of most production trackpads / mice.
const SAMPLE_PERIOD_MS: f64 = 16.0;

/// Overshoot distance in CSS pixels for human paths — see module
/// docs for rationale.
const OVERSHOOT_PX: f64 = 5.0;

/// Range (in fractions of path length) for Bezier control-point
/// perpendicular offset magnitude.
const BEZIER_PERP_MIN: f64 = 0.05;
const BEZIER_PERP_MAX: f64 = 0.25;

/// Human pre-click hover range (ms). The mouse rests on the target
/// briefly before the press — common in real users picking out a
/// small element.
const HOVER_MIN_MS: f64 = 80.0;
const HOVER_MAX_MS: f64 = 250.0;

/// Press-to-release dwell range (ms).
const PRESS_MIN_MS: f64 = 30.0;
const PRESS_MAX_MS: f64 = 90.0;

/// Synthesize a left-click mouse path from `start` to `end`.
///
/// Returns the event sequence in chronological order. Each step's
/// `at` is offset from time-zero (the call instant on the caller's
/// side).
///
/// - `InputMode::Human`: Bezier path sampled at 16 ms, Fitts-bounded
///   total duration capped at 1.2 s, overshoot + correction near the
///   endpoint, hover-then-press-then-release-then-click ending.
/// - `InputMode::Careful`: one `Move` at `end`, then `Down`/`Up`/
///   `Click` at the same point with no delay between them.
/// - `InputMode::Robotic`: empty vec.
///
/// `seed` selects the deterministic stream of random choices (control
/// point offset direction, hover duration, press dwell, overshoot
/// jitter). Same seed + same inputs produces a bit-identical output.
#[must_use]
pub fn mouse_path(start: Point, end: Point, mode: InputMode, seed: u64) -> Vec<MouseStep> {
    match mode {
        InputMode::Robotic => Vec::new(),
        InputMode::Careful => careful_path(end),
        InputMode::Human => human_path(start, end, seed),
    }
}

fn careful_path(end: Point) -> Vec<MouseStep> {
    let zero = Duration::ZERO;
    vec![
        MouseStep {
            at: zero,
            point: end,
            kind: MouseStepKind::Move,
            button: MouseButton::Left,
        },
        MouseStep {
            at: zero,
            point: end,
            kind: MouseStepKind::Down,
            button: MouseButton::Left,
        },
        MouseStep {
            at: zero,
            point: end,
            kind: MouseStepKind::Up,
            button: MouseButton::Left,
        },
        MouseStep {
            at: zero,
            point: end,
            kind: MouseStepKind::Click,
            button: MouseButton::Left,
        },
    ]
}

#[allow(clippy::too_many_lines)]
fn human_path(start: Point, end: Point, seed: u64) -> Vec<MouseStep> {
    let mut rng = Rng::seed_from_u64(seed);

    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let distance = (dx * dx + dy * dy).sqrt();

    // Fitts' law: index of difficulty + linear arrival model.
    let id = (distance / FITTS_TARGET_WIDTH_PX + 1.0).log2();
    let mut total_ms = FITTS_A_MS + FITTS_B_MS * id;
    if total_ms > HUMAN_MOVE_CAP_MS {
        total_ms = HUMAN_MOVE_CAP_MS;
    }

    // Bezier control points: pick the perpendicular direction from a
    // coin flip on the first random draw, magnitude from a uniform
    // draw. Magnitudes can differ between the two control points so
    // the curve isn't artificially symmetric.
    let perp_sign = if rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
    let perp_mag_a = distance * rng.next_uniform(BEZIER_PERP_MIN, BEZIER_PERP_MAX);
    let perp_mag_b = distance * rng.next_uniform(BEZIER_PERP_MIN, BEZIER_PERP_MAX);
    let (perp_x, perp_y) = if distance > f64::EPSILON {
        let inv = 1.0 / distance;
        (-dy * inv, dx * inv) // perpendicular to (dx, dy)
    } else {
        (0.0, 0.0)
    };
    let cp1 = Point {
        x: start.x + dx / 3.0 + perp_sign * perp_mag_a * perp_x,
        y: start.y + dy / 3.0 + perp_sign * perp_mag_a * perp_y,
    };
    let cp2 = Point {
        x: start.x + 2.0 * dx / 3.0 + perp_sign * perp_mag_b * perp_x,
        y: start.y + 2.0 * dy / 3.0 + perp_sign * perp_mag_b * perp_y,
    };

    // Sample at ≥1 step so even a sub-16ms movement has one frame.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n_steps = ((total_ms / SAMPLE_PERIOD_MS).ceil() as usize).max(1);
    let mut out: Vec<MouseStep> = Vec::with_capacity(n_steps + 4);

    for i in 0..=n_steps {
        #[allow(clippy::cast_precision_loss)]
        let t = (i as f64) / (n_steps as f64);
        // Minimum-jerk velocity profile. Sampling the curve at uniform
        // `t` gives near-constant speed, which reads as robotic; real
        // pointer motion accelerates from rest, peaks mid-path, and eases
        // to a stop. Smootherstep on the position parameter (time stays
        // uniform) produces that bell-shaped velocity. te(0)=0, te(.5)=.5,
        // te(1)=1, so endpoints and the seed-divergence midpoint hold.
        let te = t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
        let p = cubic_bezier(start, cp1, cp2, end, te);
        // Overshoot ramp: push slightly past `end` along the direct
        // direction near the tail of the path, then correct. Only
        // applied for paths of meaningful length.
        let p = if distance > 16.0 && t > 0.85 {
            let overshoot_t = (t - 0.85) / 0.15; // 0..1 within tail
            let lobe = 4.0 * overshoot_t * (1.0 - overshoot_t); // 0..1..0 triangle
            let inv = 1.0 / distance;
            Point {
                x: p.x + lobe * OVERSHOOT_PX * dx * inv,
                y: p.y + lobe * OVERSHOOT_PX * dy * inv,
            }
        } else {
            p
        };
        let at_ms = total_ms * t;
        out.push(MouseStep {
            at: ms(at_ms),
            point: p,
            kind: MouseStepKind::Move,
            button: MouseButton::Left,
        });
    }

    // Hover at the endpoint.
    let hover_ms = rng.next_uniform(HOVER_MIN_MS, HOVER_MAX_MS);
    let press_ms = rng.next_uniform(PRESS_MIN_MS, PRESS_MAX_MS);
    let down_at = total_ms + hover_ms;
    let up_at = down_at + press_ms;

    out.push(MouseStep {
        at: ms(down_at),
        point: end,
        kind: MouseStepKind::Down,
        button: MouseButton::Left,
    });
    out.push(MouseStep {
        at: ms(up_at),
        point: end,
        kind: MouseStepKind::Up,
        button: MouseButton::Left,
    });
    out.push(MouseStep {
        at: ms(up_at),
        point: end,
        kind: MouseStepKind::Click,
        button: MouseButton::Left,
    });

    out
}

fn cubic_bezier(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let b0 = u * u * u;
    let b1 = 3.0 * u * u * t;
    let b2 = 3.0 * u * t * t;
    let b3 = t * t * t;
    Point {
        x: b0 * p0.x + b1 * p1.x + b2 * p2.x + b3 * p3.x,
        y: b0 * p0.y + b1 * p1.y + b2 * p2.y + b3 * p3.y,
    }
}

fn ms(value: f64) -> Duration {
    // Clamp negative inputs to zero (Duration is unsigned) and round
    // to whole milliseconds — sub-ms timing is not meaningful given
    // the platforms' event-loop granularity.
    let v = value.max(0.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms_int = v as u64;
    Duration::from_millis(ms_int)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: Point = Point { x: 0.0, y: 0.0 };
    const FAR: Point = Point { x: 800.0, y: 600.0 };

    #[test]
    fn robotic_is_empty() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Robotic, 0);
        assert!(steps.is_empty());
    }

    #[test]
    fn careful_is_four_steps_at_endpoint() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Careful, 0);
        assert_eq!(steps.len(), 4);
        for s in &steps {
            assert_eq!(s.point, FAR);
            assert_eq!(s.at, Duration::ZERO);
            assert_eq!(s.button, MouseButton::Left);
        }
        assert_eq!(steps[0].kind, MouseStepKind::Move);
        assert_eq!(steps[1].kind, MouseStepKind::Down);
        assert_eq!(steps[2].kind, MouseStepKind::Up);
        assert_eq!(steps[3].kind, MouseStepKind::Click);
    }

    #[test]
    fn human_starts_at_start_ends_at_end() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Human, 42);
        let first = steps.first().expect("non-empty");
        let last_move = steps
            .iter()
            .rev()
            .find(|s| s.kind == MouseStepKind::Move)
            .expect("at least one move");
        // Bezier path starts at `start` exactly at t=0.
        assert!((first.point.x - ORIGIN.x).abs() < 1.0);
        assert!((first.point.y - ORIGIN.y).abs() < 1.0);
        // Last `Move` is near the endpoint (overshoot returns to it).
        assert!(
            (last_move.point.x - FAR.x).abs() < OVERSHOOT_PX + 1.0,
            "last move x off from end: {} vs {}",
            last_move.point.x,
            FAR.x
        );
        assert!(
            (last_move.point.y - FAR.y).abs() < OVERSHOOT_PX + 1.0,
            "last move y off from end: {} vs {}",
            last_move.point.y,
            FAR.y
        );
    }

    #[test]
    fn human_emits_many_moves_for_long_distance() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Human, 42);
        let moves = steps
            .iter()
            .filter(|s| s.kind == MouseStepKind::Move)
            .count();
        // Long-distance Fitts ID ~ log2(1000/32 + 1) ≈ 5 → ~850ms /
        // 16ms ≈ 53 samples. Allow significant slack; just assert the
        // count is "many" so a regression to single-step is caught.
        assert!(moves > 20, "expected many move samples; got {moves}");
    }

    #[test]
    fn human_click_sequence_present() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Human, 42);
        let kinds: Vec<MouseStepKind> = steps.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&MouseStepKind::Down));
        assert!(kinds.contains(&MouseStepKind::Up));
        assert!(kinds.contains(&MouseStepKind::Click));
        // Down must come before Up which must come before Click.
        let down = kinds
            .iter()
            .position(|k| *k == MouseStepKind::Down)
            .unwrap();
        let up = kinds.iter().position(|k| *k == MouseStepKind::Up).unwrap();
        let click = kinds
            .iter()
            .position(|k| *k == MouseStepKind::Click)
            .unwrap();
        assert!(down < up, "down must precede up");
        assert!(up <= click, "up must precede or coincide with click");
    }

    #[test]
    fn human_zero_distance_still_produces_click() {
        let steps = mouse_path(ORIGIN, ORIGIN, InputMode::Human, 7);
        // Even a zero-distance hover should yield the click triple.
        let kinds: Vec<MouseStepKind> = steps.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&MouseStepKind::Down));
        assert!(kinds.contains(&MouseStepKind::Up));
        assert!(kinds.contains(&MouseStepKind::Click));
    }

    #[test]
    fn human_is_deterministic_under_seed() {
        let a = mouse_path(ORIGIN, FAR, InputMode::Human, 1234);
        let b = mouse_path(ORIGIN, FAR, InputMode::Human, 1234);
        assert_eq!(a, b);
    }

    #[test]
    fn human_seed_change_changes_path() {
        let a = mouse_path(ORIGIN, FAR, InputMode::Human, 1);
        let b = mouse_path(ORIGIN, FAR, InputMode::Human, 2);
        // Total step count or some intermediate point should differ.
        // Compare the midpoint of each path; they'd only coincide on
        // accident.
        let mid_a = a[a.len() / 2].point;
        let mid_b = b[b.len() / 2].point;
        let diff = ((mid_a.x - mid_b.x).powi(2) + (mid_a.y - mid_b.y).powi(2)).sqrt();
        assert!(
            diff > 1.0,
            "paths suspiciously identical: {mid_a:?} vs {mid_b:?}"
        );
    }

    #[test]
    fn human_total_duration_capped() {
        // Pathological distance: Fitts ID would blow past the cap.
        let far = Point {
            x: 100_000.0,
            y: 0.0,
        };
        let steps = mouse_path(ORIGIN, far, InputMode::Human, 0);
        let last_click = steps
            .iter()
            .rev()
            .find(|s| s.kind == MouseStepKind::Click)
            .expect("click present");
        // Total = move cap + hover + press, with hover ≤ 250ms and
        // press ≤ 90ms, so ≤ 1200 + 250 + 90 = 1540ms.
        assert!(
            last_click.at.as_millis() <= 1540,
            "total duration not capped: {}ms",
            last_click.at.as_millis()
        );
    }

    #[test]
    fn human_steps_monotonic_in_time() {
        let steps = mouse_path(ORIGIN, FAR, InputMode::Human, 99);
        let times: Vec<u128> = steps.iter().map(|s| s.at.as_millis()).collect();
        for w in times.windows(2) {
            assert!(w[0] <= w[1], "times not monotonic: {w:?}");
        }
    }

    #[test]
    fn human_path_stays_within_overshoot_bound() {
        // No sampled point should exceed the line bounding box by
        // more than overshoot + perp_max·distance.
        let steps = mouse_path(ORIGIN, FAR, InputMode::Human, 42);
        let max_excursion = OVERSHOOT_PX + BEZIER_PERP_MAX * 1000.0; // distance ~1000
        for s in &steps {
            if s.kind != MouseStepKind::Move {
                continue;
            }
            // Distance from the direct line: project onto perpendicular.
            let dx = FAR.x - ORIGIN.x;
            let dy = FAR.y - ORIGIN.y;
            let len = (dx * dx + dy * dy).sqrt();
            let perp = ((s.point.x - ORIGIN.x) * (-dy) + (s.point.y - ORIGIN.y) * dx).abs() / len;
            assert!(
                perp < max_excursion + 2.0,
                "point too far from direct line: {perp}px > {}",
                max_excursion + 2.0
            );
        }
    }
}

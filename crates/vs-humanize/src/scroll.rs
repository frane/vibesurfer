//! Wheel scroll synthesis.
//!
//! `Human`: split a large scroll into many small wheel events with
//! an exponential decay (initial wheel turns are big, late wheels
//! taper as the user "arrives") and 16–32 ms gaps. The decay matches
//! real trackpad inertia.
//!
//! `Careful`: one wheel event carrying the entire delta.
//!
//! `Robotic`: empty vec — engine falls back to `Element.scrollBy` or
//! similar JS dispatch.

use std::time::Duration;

use crate::rng::Rng;
use crate::InputMode;

/// 2D vector (delta).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

/// One wheel-tick event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelStep {
    pub at: Duration,
    pub delta: Vec2,
}

// --- Tuning ---

/// Minimum / maximum gap between wheel events in Human mode (ms).
const HUMAN_GAP_MIN_MS: f64 = 16.0;
const HUMAN_GAP_MAX_MS: f64 = 32.0;

/// Inertia decay factor per tick. Each successive tick carries
/// `prev * decay` of the remaining delta. With `decay = 0.55`, a
/// large scroll burns down in ~8 ticks.
const HUMAN_DECAY: f64 = 0.55;

/// Cap on the number of inertia ticks. Prevents the synthesizer
/// from emitting hundreds of micro-events on a pathologically large
/// scroll request.
const HUMAN_MAX_TICKS: usize = 24;

/// Pixel threshold below which we stop emitting ticks and stuff any
/// residual into the final event. Avoids the "fractional pixel
/// forever" tail of pure exponential decay.
const RESIDUAL_THRESHOLD_PX: f64 = 1.0;

/// Synthesize a scroll sequence delivering total `delta` over time.
///
/// Returns events in chronological order; the sum of all `delta`
/// values equals the requested total (modulo rounding to whole
/// pixels in the last step).
///
/// - `InputMode::Human`: exponentially-decaying inertia ticks with
///   randomized 16–32 ms gaps. Total tick count bounded.
/// - `InputMode::Careful`: one event at `at = 0` carrying the full
///   delta.
/// - `InputMode::Robotic`: empty vec.
#[must_use]
pub fn scroll_sequence(delta: Vec2, mode: InputMode, seed: u64) -> Vec<WheelStep> {
    match mode {
        InputMode::Robotic => Vec::new(),
        InputMode::Careful => careful_sequence(delta),
        InputMode::Human => human_sequence(delta, seed),
    }
}

fn careful_sequence(delta: Vec2) -> Vec<WheelStep> {
    vec![WheelStep {
        at: Duration::ZERO,
        delta,
    }]
}

fn human_sequence(delta: Vec2, seed: u64) -> Vec<WheelStep> {
    if delta.x.abs() < f64::EPSILON && delta.y.abs() < f64::EPSILON {
        return Vec::new();
    }
    let mut rng = Rng::seed_from_u64(seed);
    let mut out: Vec<WheelStep> = Vec::new();
    let mut remaining = delta;
    let mut t_ms = 0.0;

    for i in 0..HUMAN_MAX_TICKS {
        let weight = if i == 0 {
            1.0 - HUMAN_DECAY
        } else {
            (1.0 - HUMAN_DECAY) * (1.0 - residual_fraction(&remaining, &delta))
        };
        let mut step = Vec2 {
            x: remaining.x * (1.0 - HUMAN_DECAY).max(weight),
            y: remaining.y * (1.0 - HUMAN_DECAY).max(weight),
        };
        // If residual is tiny, dump everything into this last step
        // and exit the loop on the next iteration check.
        let residual_mag = (remaining.x * remaining.x + remaining.y * remaining.y).sqrt();
        if residual_mag <= RESIDUAL_THRESHOLD_PX {
            step = remaining;
        }
        out.push(WheelStep {
            at: ms(t_ms),
            delta: step,
        });
        remaining = Vec2 {
            x: remaining.x - step.x,
            y: remaining.y - step.y,
        };
        let residual_mag2 = (remaining.x * remaining.x + remaining.y * remaining.y).sqrt();
        if residual_mag2 <= RESIDUAL_THRESHOLD_PX {
            // Fold any leftover sub-pixel residual into the previous
            // step to ensure the sum is exact.
            if let Some(last) = out.last_mut() {
                last.delta.x += remaining.x;
                last.delta.y += remaining.y;
            }
            break;
        }
        t_ms += rng.next_uniform(HUMAN_GAP_MIN_MS, HUMAN_GAP_MAX_MS);
    }
    out
}

fn residual_fraction(remaining: &Vec2, total: &Vec2) -> f64 {
    let r = (remaining.x * remaining.x + remaining.y * remaining.y).sqrt();
    let t = (total.x * total.x + total.y * total.y).sqrt();
    if t <= f64::EPSILON {
        0.0
    } else {
        r / t
    }
}

fn ms(value: f64) -> Duration {
    let v = value.max(0.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms_int = v as u64;
    Duration::from_millis(ms_int)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robotic_is_empty() {
        let steps = scroll_sequence(Vec2 { x: 0.0, y: 600.0 }, InputMode::Robotic, 0);
        assert!(steps.is_empty());
    }

    #[test]
    fn careful_is_single_event_with_full_delta() {
        let d = Vec2 { x: 0.0, y: 800.0 };
        let steps = scroll_sequence(d, InputMode::Careful, 0);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].delta, d);
        assert_eq!(steps[0].at, Duration::ZERO);
    }

    #[test]
    fn human_empty_delta_yields_empty_sequence() {
        let steps = scroll_sequence(Vec2 { x: 0.0, y: 0.0 }, InputMode::Human, 7);
        assert!(steps.is_empty());
    }

    #[test]
    fn human_emits_multiple_ticks() {
        let steps = scroll_sequence(Vec2 { x: 0.0, y: 800.0 }, InputMode::Human, 42);
        assert!(
            steps.len() >= 3,
            "expected several inertia ticks; got {}",
            steps.len()
        );
        assert!(
            steps.len() <= HUMAN_MAX_TICKS,
            "tick cap violated: {}",
            steps.len()
        );
    }

    #[test]
    fn human_sum_equals_requested_delta() {
        let d = Vec2 { x: 0.0, y: 800.0 };
        let steps = scroll_sequence(d, InputMode::Human, 42);
        let sum_x: f64 = steps.iter().map(|s| s.delta.x).sum();
        let sum_y: f64 = steps.iter().map(|s| s.delta.y).sum();
        assert!((sum_x - d.x).abs() < 0.5);
        assert!((sum_y - d.y).abs() < 0.5);
    }

    #[test]
    fn human_inertia_first_tick_largest() {
        // Decay model means the magnitude of the first tick exceeds
        // the magnitude of the last tick (which carries the small
        // residual or a small final fraction).
        let steps = scroll_sequence(Vec2 { x: 0.0, y: 1200.0 }, InputMode::Human, 42);
        assert!(steps.len() >= 2);
        let first_mag = steps[0].delta.y.abs();
        let last_mag = steps.last().unwrap().delta.y.abs();
        assert!(
            first_mag >= last_mag,
            "expected first tick magnitude {first_mag} >= last tick magnitude {last_mag}"
        );
    }

    #[test]
    fn human_is_deterministic_under_seed() {
        let a = scroll_sequence(Vec2 { x: 0.0, y: 1000.0 }, InputMode::Human, 99);
        let b = scroll_sequence(Vec2 { x: 0.0, y: 1000.0 }, InputMode::Human, 99);
        assert_eq!(a, b);
    }

    #[test]
    fn human_steps_monotonic_in_time() {
        let steps = scroll_sequence(Vec2 { x: 0.0, y: 1000.0 }, InputMode::Human, 99);
        for w in steps.windows(2) {
            assert!(w[0].at <= w[1].at);
        }
    }
}

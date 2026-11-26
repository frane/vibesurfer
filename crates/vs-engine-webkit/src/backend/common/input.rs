//! Input timing every backend shares: the seed of a cursor primitive's
//! humanized path and the per-key cadence of trusted typing.

use vs_humanize::InputMode;

/// Seed for a cursor primitive's humanized path, from its coordinates:
/// the same op traces the same path on every run. A plain mixer;
/// replays need determinism, not cryptographic spread. macOS keeps its
/// own seed (`webkit::vs_humanize_seed_for_xy`).
#[cfg(not(target_os = "macos"))]
pub(crate) fn cursor_seed(op: crate::engine::CursorOp) -> u64 {
    use crate::engine::CursorOp;
    let (a, b, c, d) = match op {
        CursorOp::MoveTo { x, y }
        | CursorOp::HoverAt { x, y }
        | CursorOp::ClickAt { x, y }
        | CursorOp::Click { x, y, .. } => (x, y, 0.0, 0.0),
        CursorOp::Wheel { x, y, dx, dy } => (x, y, dx, dy),
        CursorOp::Drag { x1, y1, x2, y2 } => (x1, y1, x2, y2),
    };
    a.to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ b.to_bits().wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ c.to_bits().wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ d.to_bits()
}

/// Milliseconds between keys when typing carefully.
const CAREFUL_KEY_MS: u64 = 120;
/// Mean milliseconds between keys when typing like a human.
const HUMAN_KEY_MS: u64 = 45;

/// Per-key timing for trusted typing. Robotic types back to back;
/// careful waits a fixed beat; human varies the wait by up to ±40% of
/// its beat with a xorshift seeded from the text, so typing is not
/// metronomic yet the same text replays identically.
pub(crate) struct KeyCadence {
    beat: u64,
    span: u64,
    jitter: u64,
}

impl KeyCadence {
    #[must_use]
    pub(crate) fn new(mode: InputMode, text: &str) -> Self {
        let beat = match mode {
            InputMode::Robotic => 0,
            InputMode::Careful => CAREFUL_KEY_MS,
            InputMode::Human => HUMAN_KEY_MS,
        };
        let span = if matches!(mode, InputMode::Human) {
            beat * 4 / 10
        } else {
            0
        };
        let seed = text
            .chars()
            .map(u64::from)
            .fold(0x9e37_79b9_u64, |a, c| a.wrapping_mul(31).wrapping_add(c));
        Self {
            beat,
            span,
            jitter: seed | 1,
        }
    }

    /// Milliseconds a key is held down.
    #[must_use]
    pub(crate) fn hold_ms(&self) -> u64 {
        if self.beat == 0 {
            1
        } else {
            self.beat / 3
        }
    }

    /// Milliseconds to wait after a key's release, or `None` when typing
    /// robotically (no wait).
    pub(crate) fn next_wait_ms(&mut self) -> Option<u64> {
        if self.beat == 0 {
            return None;
        }
        self.jitter ^= self.jitter << 13;
        self.jitter ^= self.jitter >> 7;
        self.jitter ^= self.jitter << 17;
        let extra = if self.span == 0 {
            0
        } else {
            self.jitter % (self.span * 2 + 1)
        };
        Some(self.beat.saturating_sub(self.span).saturating_add(extra))
    }
}

#[cfg(test)]
mod tests {
    use super::{InputMode, KeyCadence};

    #[test]
    fn robotic_types_back_to_back() {
        let mut c = KeyCadence::new(InputMode::Robotic, "abc");
        assert_eq!(c.hold_ms(), 1);
        assert_eq!(c.next_wait_ms(), None);
    }

    #[test]
    fn careful_waits_a_fixed_beat() {
        let mut c = KeyCadence::new(InputMode::Careful, "abc");
        assert_eq!(c.hold_ms(), 40);
        assert_eq!(c.next_wait_ms(), Some(120));
        assert_eq!(c.next_wait_ms(), Some(120));
    }

    #[test]
    fn human_stays_within_forty_percent_and_replays() {
        let waits = |text| {
            let mut c = KeyCadence::new(InputMode::Human, text);
            (0..50)
                .map(|_| c.next_wait_ms().unwrap())
                .collect::<Vec<_>>()
        };
        let a = waits("hello world");
        assert!(a.iter().all(|w| (27..=63).contains(w)), "{a:?}");
        assert_eq!(a, waits("hello world"));
    }
}

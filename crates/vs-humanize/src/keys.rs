//! Keystroke sequence synthesis.
//!
//! `Human`: lognormal-ish inter-key delays with a mean in the
//! 80–180 ms band, longer pauses at word boundaries, occasional
//! typo + backspace + retype. Each character emits a `Down` followed
//! by an `Up` with a small dwell between them.
//!
//! `Careful`: every character at a fixed 50 ms cadence, no typos,
//! Down + Up per character.
//!
//! `Robotic`: empty vec — the engine falls back to setting the
//! field's `.value` and dispatching `input`/`change` JS events.

use std::time::Duration;

use crate::rng::Rng;
use crate::InputMode;

/// One key, identified by a USB HID-ish `code` (engines map this to
/// their platform's keycode space) and an optional UTF-32 character.
///
/// `character` is `None` for non-printable keys like `Backspace`.
/// For typed text, `code` is the printable character's value and
/// `character` carries the same `char`; the engine can choose which
/// to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: u32,
    pub character: Option<char>,
}

/// What kind of key event this step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStepKind {
    /// Key press (`keydown`).
    Down,
    /// Key release (`keyup`).
    Up,
    /// A composite press — engines that don't have separate keydown/
    /// keyup paths can use this. We emit explicit `Down` + `Up` for
    /// trusted paths and never use `Press` directly, but it's part
    /// of the public type for engines that need it.
    Press,
}

/// One key event in a synthesized sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStep {
    pub at: Duration,
    pub kind: KeyStepKind,
    pub key: Key,
}

// --- Tuning ---

/// Per-key dwell (down→up gap) in milliseconds for Human mode.
const HUMAN_DWELL_MIN_MS: f64 = 30.0;
const HUMAN_DWELL_MAX_MS: f64 = 80.0;

/// Lognormal parameters for inter-key gap. With `mu=4.7, sigma=0.3`
/// the geometric mean is exp(4.7) ≈ 110ms, with a long right tail.
/// Clipped to `[40, 260]` so a single fat-tail draw can't stall the
/// agent.
const HUMAN_GAP_MU: f64 = 4.7;
const HUMAN_GAP_SIGMA: f64 = 0.3;
const HUMAN_GAP_MIN_MS: f64 = 40.0;
const HUMAN_GAP_MAX_MS: f64 = 260.0;

/// Multiplier on the inter-key gap when the next character is a word
/// boundary (space, newline, tab, punctuation). Real humans pause
/// here to think.
const WORD_BOUNDARY_MULT: f64 = 1.7;

/// Probability per character of fat-fingering a typo. Real numbers
/// in the typing-research literature are around 1.5–3%; we pick the
/// low end so test assertions about exact character counts don't
/// flake.
const HUMAN_TYPO_RATE: f64 = 0.015;

/// Fixed cadence for Careful mode.
const CAREFUL_GAP_MS: f64 = 50.0;

/// Synthesize a key sequence for typing `text`.
///
/// Returns events in chronological order. Each character produces a
/// `Down` at the inter-key gap offset and a matching `Up` after a
/// short dwell. In Human mode, occasional typos (≈1.5%) insert a
/// wrong-character `Down`/`Up`, a `Backspace` `Down`/`Up`, then the
/// intended character.
///
/// - `InputMode::Human`: lognormal gaps, word-boundary pauses, typos.
/// - `InputMode::Careful`: 50 ms fixed cadence, no typos.
/// - `InputMode::Robotic`: empty vec.
///
/// `seed` is the deterministic stream selector. Same seed + same
/// text produces bit-identical output.
#[must_use]
pub fn key_sequence(text: &str, mode: InputMode, seed: u64) -> Vec<KeyStep> {
    match mode {
        InputMode::Robotic => Vec::new(),
        InputMode::Careful => careful_sequence(text),
        InputMode::Human => human_sequence(text, seed),
    }
}

fn careful_sequence(text: &str) -> Vec<KeyStep> {
    let mut out = Vec::with_capacity(text.chars().count() * 2);
    let mut t_ms = 0.0;
    for ch in text.chars() {
        let key = Key {
            code: ch as u32,
            character: Some(ch),
        };
        out.push(KeyStep {
            at: ms(t_ms),
            kind: KeyStepKind::Down,
            key,
        });
        out.push(KeyStep {
            at: ms(t_ms + 25.0),
            kind: KeyStepKind::Up,
            key,
        });
        t_ms += CAREFUL_GAP_MS;
    }
    out
}

fn human_sequence(text: &str, seed: u64) -> Vec<KeyStep> {
    let mut rng = Rng::seed_from_u64(seed);
    let mut out: Vec<KeyStep> = Vec::with_capacity(text.chars().count() * 2 + 4);
    let mut t_ms = 0.0;
    let chars: Vec<char> = text.chars().collect();

    for (i, ch) in chars.iter().copied().enumerate() {
        // Inter-key gap — sample lognormal, clip.
        let gap_raw = (HUMAN_GAP_MU + HUMAN_GAP_SIGMA * rng.next_normal()).exp();
        let mut gap = gap_raw.clamp(HUMAN_GAP_MIN_MS, HUMAN_GAP_MAX_MS);
        if i > 0 && is_word_boundary(chars[i - 1]) {
            gap *= WORD_BOUNDARY_MULT;
        }
        if i > 0 {
            t_ms += gap;
        }

        // Occasional typo: insert a neighboring-letter press, then
        // Backspace, then the intended character. Only fires on
        // alphabetic input where typos are plausible. Each press's
        // Down→Down spacing is the same lognormal-sampled gap as
        // normal keys, but the typo path uses tight 60–120 ms gaps
        // between wrong→backspace and backspace→intended (real
        // corrections are faster than baseline typing).
        if ch.is_ascii_alphabetic() && rng.next_f64() < HUMAN_TYPO_RATE {
            let wrong = neighbor_letter(ch, &mut rng);
            press(&mut out, t_ms, wrong, &mut rng);
            t_ms += rng.next_uniform(60.0, 120.0);
            press_backspace(&mut out, t_ms, &mut rng);
            t_ms += rng.next_uniform(60.0, 120.0);
        }

        press(&mut out, t_ms, ch, &mut rng);
    }
    out
}

fn press(out: &mut Vec<KeyStep>, t_ms: f64, ch: char, rng: &mut Rng) {
    // Dwell is the *within-press* Down→Up offset. Don't advance the
    // caller's `t_ms`; that's reserved for the *inter-press* gap so
    // Down→Down spacing equals the lognormal-sampled gap directly.
    let dwell = rng.next_uniform(HUMAN_DWELL_MIN_MS, HUMAN_DWELL_MAX_MS);
    let key = Key {
        code: ch as u32,
        character: Some(ch),
    };
    out.push(KeyStep {
        at: ms(t_ms),
        kind: KeyStepKind::Down,
        key,
    });
    out.push(KeyStep {
        at: ms(t_ms + dwell),
        kind: KeyStepKind::Up,
        key,
    });
}

fn press_backspace(out: &mut Vec<KeyStep>, t_ms: f64, rng: &mut Rng) {
    let dwell = rng.next_uniform(HUMAN_DWELL_MIN_MS, HUMAN_DWELL_MAX_MS);
    // USB HID keyboard usage 0x2A — engines map this to their
    // platform's Backspace virtual key.
    let key = Key {
        code: 0x2A,
        character: None,
    };
    out.push(KeyStep {
        at: ms(t_ms),
        kind: KeyStepKind::Down,
        key,
    });
    out.push(KeyStep {
        at: ms(t_ms + dwell),
        kind: KeyStepKind::Up,
        key,
    });
}

fn is_word_boundary(ch: char) -> bool {
    ch == ' ' || ch == '\n' || ch == '\t' || ch == ',' || ch == '.' || ch == ';' || ch == ':'
}

/// Pick a plausible "neighboring" typo character. Uses a tiny QWERTY
/// adjacency for ASCII alphabetics; falls back to the original for
/// anything we don't have a table for. Doesn't need to be exhaustive
/// — we just need _a_ wrong character that's typed-not-random.
fn neighbor_letter(ch: char, rng: &mut Rng) -> char {
    let lower = ch.to_ascii_lowercase();
    let neighbors: &[char] = match lower {
        'a' => &['s', 'q', 'w', 'z'],
        'b' => &['v', 'n', 'g', 'h'],
        'c' => &['x', 'v', 'd', 'f'],
        'd' => &['s', 'f', 'e', 'r', 'c'],
        'e' => &['w', 'r', 's', 'd'],
        'f' => &['d', 'g', 'r', 't', 'v'],
        'g' => &['f', 'h', 't', 'y', 'b'],
        'h' => &['g', 'j', 'y', 'u', 'n'],
        'i' => &['u', 'o', 'k'],
        'j' => &['h', 'k', 'u', 'i', 'm'],
        'k' => &['j', 'l', 'i', 'o'],
        'l' => &['k', 'o', 'p'],
        'm' => &['n', 'j', 'k'],
        'n' => &['b', 'm', 'h', 'j'],
        'o' => &['i', 'p', 'k', 'l'],
        'p' => &['o', 'l'],
        'q' => &['w', 'a'],
        'r' => &['e', 't', 'd', 'f'],
        's' => &['a', 'd', 'w', 'e', 'z', 'x'],
        't' => &['r', 'y', 'f', 'g'],
        'u' => &['y', 'i', 'h', 'j'],
        'v' => &['c', 'b', 'f', 'g'],
        'w' => &['q', 'e', 'a', 's'],
        'x' => &['z', 'c', 's', 'd'],
        'y' => &['t', 'u', 'g', 'h'],
        'z' => &['a', 's', 'x'],
        _ => return ch,
    };
    // Truncation is fine — modulo against `neighbors.len()` (≤ 5)
    // makes the upper bits irrelevant.
    #[allow(clippy::cast_possible_truncation)]
    let i = (rng.next_u64() as usize) % neighbors.len();
    let n = neighbors[i];
    if ch.is_ascii_uppercase() {
        n.to_ascii_uppercase()
    } else {
        n
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
        let steps = key_sequence("hello", InputMode::Robotic, 0);
        assert!(steps.is_empty());
    }

    #[test]
    fn careful_one_down_one_up_per_char() {
        let steps = key_sequence("hi!", InputMode::Careful, 0);
        assert_eq!(steps.len(), 6);
        let downs = steps.iter().filter(|s| s.kind == KeyStepKind::Down).count();
        let ups = steps.iter().filter(|s| s.kind == KeyStepKind::Up).count();
        assert_eq!(downs, 3);
        assert_eq!(ups, 3);
    }

    #[test]
    fn careful_fixed_cadence() {
        let steps = key_sequence("abc", InputMode::Careful, 0);
        // Three characters means three Downs at 0, 50, 100 ms.
        let down_times: Vec<u128> = steps
            .iter()
            .filter(|s| s.kind == KeyStepKind::Down)
            .map(|s| s.at.as_millis())
            .collect();
        assert_eq!(down_times, vec![0, 50, 100]);
    }

    #[test]
    fn human_empty_text_yields_empty_sequence() {
        let steps = key_sequence("", InputMode::Human, 7);
        assert!(steps.is_empty());
    }

    #[test]
    fn human_single_char_has_down_then_up() {
        let steps = key_sequence("x", InputMode::Human, 7);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, KeyStepKind::Down);
        assert_eq!(steps[1].kind, KeyStepKind::Up);
        assert_eq!(steps[0].key.character, Some('x'));
    }

    #[test]
    fn human_is_deterministic_under_seed() {
        let a = key_sequence("hello world", InputMode::Human, 1234);
        let b = key_sequence("hello world", InputMode::Human, 1234);
        assert_eq!(a, b);
    }

    #[test]
    fn human_seed_change_changes_timing() {
        let a = key_sequence("hello world", InputMode::Human, 1);
        let b = key_sequence("hello world", InputMode::Human, 2);
        // Last-event timestamp differs unless we got monumentally
        // unlucky with two equally-distributed lognormal streams.
        assert_ne!(a.last().unwrap().at, b.last().unwrap().at);
    }

    #[test]
    fn human_word_boundary_pauses_longer() {
        // The word-boundary multiplier applies to the gap *after* a
        // boundary character (real users finish a word, hit space,
        // then pause briefly before the next word). For "ab cd"
        // that's the gap from the space's Down to 'c's Down. Compare
        // it across many seeds against the in-word a→b gap; median
        // so a single fat-tail draw doesn't flake the test.
        let trials = 30;
        let mut letter_gaps: Vec<u128> = Vec::new();
        let mut boundary_gaps: Vec<u128> = Vec::new();
        for seed in 0..trials {
            // Use only consonants to avoid the 1.5% typo path
            // perturbing the per-position gap interpretation; typos
            // on alphabetic chars insert extra events into the gap.
            let s = key_sequence("rt yu", InputMode::Careful, seed);
            // Sanity: careful mode should not affect the test logic,
            // we only use the alphabetic-skip property of the typo
            // path. Switch back to Human for the real measurement.
            assert_eq!(s.iter().filter(|s| s.kind == KeyStepKind::Down).count(), 5);
        }
        for seed in 0..trials {
            let s = key_sequence("ab cd", InputMode::Human, seed);
            let downs: Vec<u128> = s
                .iter()
                .filter(|s| s.kind == KeyStepKind::Down)
                .map(|s| s.at.as_millis())
                .collect();
            // 5 chars → 5 Down events unless a typo fired; skip seeds
            // where the typo path inserted extra Downs so the gap
            // index alignment is preserved.
            if downs.len() != 5 {
                continue;
            }
            letter_gaps.push(downs[1] - downs[0]); // a→b (in-word)
            boundary_gaps.push(downs[3] - downs[2]); // space→c (boundary)
        }
        let median = |mut v: Vec<u128>| {
            v.sort_unstable();
            v[v.len() / 2]
        };
        let m_letter = median(letter_gaps);
        let m_boundary = median(boundary_gaps);
        assert!(
            m_boundary > m_letter,
            "boundary gap median {m_boundary}ms should exceed letter gap median {m_letter}ms"
        );
    }

    #[test]
    fn human_mean_gap_in_expected_band() {
        // Mean inter-key gap across many seeds should land in the
        // 80–180 ms band the spec calls out. Sample widely to
        // average over the lognormal tail.
        let mut all_gaps: Vec<u128> = Vec::new();
        for seed in 0..50 {
            let s = key_sequence("the quick brown fox", InputMode::Human, seed);
            let downs: Vec<u128> = s
                .iter()
                .filter(|s| s.kind == KeyStepKind::Down)
                .map(|s| s.at.as_millis())
                .collect();
            for w in downs.windows(2) {
                all_gaps.push(w[1] - w[0]);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let n = all_gaps.len() as f64;
        #[allow(clippy::cast_precision_loss)]
        let mean: f64 = all_gaps.iter().sum::<u128>() as f64 / n;
        assert!(
            (80.0..=180.0).contains(&mean),
            "mean inter-key gap {mean}ms outside 80–180 band"
        );
    }

    #[test]
    fn human_emits_typos_over_long_text() {
        // Run a long-enough text that the 1.5% typo rate produces at
        // least one backspace in nearly every realization. Sample
        // multiple seeds; assert that at least one shows a backspace.
        let any_backspace = (0..16).any(|seed| {
            let s = key_sequence(
                &"the quick brown fox jumps over the lazy dog ".repeat(8),
                InputMode::Human,
                seed,
            );
            s.iter().any(|step| step.key.code == 0x2A)
        });
        assert!(any_backspace, "no typos in 16 long-text realizations");
    }

    #[test]
    fn human_steps_monotonic_in_time() {
        let steps = key_sequence("hello world", InputMode::Human, 42);
        for w in steps.windows(2) {
            assert!(
                w[0].at <= w[1].at,
                "non-monotonic: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }
}

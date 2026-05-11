# vs-humanize

Pure-math input synthesis for vibesurfer's three input modes. Returns sequences of timestamped `MouseStep` / `KeyStep` / `WheelStep` events; the engine crate consumes those and dispatches each step as a platform-native event (`NSEvent` on macOS, `GdkEvent` on Linux, `Input.dispatchMouseEvent` over CDP on Windows).

Splitting the math out has two virtues:

- The realism work concentrates in one place. Tightening the Bezier sampler or the keystroke distribution lifts every backend simultaneously.
- The crate is std-only and `cargo test -p vs-humanize` runs without any platform deps. No WebKit, no GTK, no WebView2 — just numbers.

## Modes

| Mode | `mouse_path` | `key_sequence` | `scroll_sequence` |
|------|--------------|----------------|-------------------|
| `Human` | Cubic Bezier sampled at 16 ms, Fitts arrival time, overshoot + correction, hover→press→release→click | Lognormal inter-key gaps, word-boundary pauses (1.7×), occasional typo + backspace + retype | Exponentially-decaying inertia ticks, 16–32 ms inter-tick gaps |
| `Careful` | Single `Move` then `Down`/`Up`/`Click` at `at = 0` | Fixed 50 ms cadence, no typos | One wheel event carrying the full delta |
| `Robotic` | Empty vec — caller falls back to JS synthetic dispatch | Empty vec | Empty vec |

The empty `Vec` for `Robotic` is intentional: callers always reach for `vs-humanize` and let the contract decide whether to dispatch the returned sequence or fall back to the JS path. No branching on the mode at the call site.

## Determinism

Every entry point takes a `seed: u64`. Same seed + same inputs produces byte-identical output. The daemon will persist a per-session seed so a given agent against the same site produces consistent typing patterns across reconnects.

The seeded PRNG is `xoshiro256**` (Blackman & Vigna, 2018), hand-rolled in `rng.rs`. We don't pull the `rand` crate because the engine crate already drags WebKit / WebKitGTK / WebView2 transitively; staying std-only here saves ~40 transitive deps and avoids version conflicts.

## Mouse path math

- Bezier: cubic curve with two perpendicular control points whose direction is decided by a coin flip and magnitude is uniformly drawn from `[0.05·L, 0.25·L]`. Sampled at 16 ms (≈ 60 Hz).
- Fitts' law (Fitts, 1954): total path time `t = a + b · log2(d / W + 1)` with `a = 100 ms`, `b = 150 ms`, `W = 32 px` (assumed target width — a typical desktop clickable region). Capped at 1.2 s so the agent doesn't stall on pathological distances; PR 5 of M7 surfaces a `?slow_dispatch` warning when the cap fires.
- Overshoot: paths longer than 16 px push 5 px past the endpoint along the direct-line direction, then correct, in the last 15% of the path. Visible in the sampled trace as a brief reverse `Move`.
- Click sequence: `Move...Move`, `Down`, `Up`, `Click` with a 80–250 ms hover before `Down` and a 30–90 ms press dwell.

## Keystroke math

- Inter-key gap: lognormal with `mu = 4.7, sigma = 0.3` (geometric mean ≈ 110 ms), clipped to `[40, 260] ms`. Down→Down spacing equals the sampled gap — dwell is intra-press only.
- Word-boundary multiplier: gap after `' '`, `'\n'`, `'\t'`, `','`, `'.'`, `';'`, `':'` is multiplied by 1.7. Real users pause before starting the next word.
- Typo rate: 1.5% per alphabetic character. A typo inserts a neighboring-QWERTY-letter press, a 60–120 ms wait, a Backspace press, a 60–120 ms wait, then the intended character. Backspace is HID usage 0x2A.
- Dwell: each press emits `Down` then `Up` with a 30–80 ms gap between them, independent of inter-key gap.

## Scroll math

- Inertia decay: each tick carries `(1 - 0.55)` of the remaining delta. Burns down in ~8 ticks for typical deltas; capped at 24 ticks to prevent pathological inputs from emitting hundreds of events.
- Inter-tick gap: uniform `[16, 32] ms`.
- Residual fold: when the remaining delta is sub-pixel, the leftover is folded into the previous tick so the sum is exact.

## References

- Fitts, P. M. (1954). The information capacity of the human motor system in controlling the amplitude of movement. *Journal of Experimental Psychology*, 47(6), 381–391.
- Blackman, D. & Vigna, S. (2018). xoshiro/xoroshiro generators and the PRNG shootout. <https://prng.di.unimi.it/>
- Lognormal keystroke timing: Killourhy, K. S. & Maxion, R. A. (2009). Comparing anomaly-detection algorithms for keystroke dynamics. *IEEE/IFIP DSN*.

## Testing

```
cargo test -p vs-humanize
```

34 unit tests covering: boundary cases (zero-distance moves, single-char fills, empty scrolls), mode comparisons (robotic is empty, careful is single-shot, human is multi-event), seeded determinism, monotonic timestamps, Fitts cap, Bezier-path bounding, lognormal mean within expected band, word-boundary pauses larger than in-word pauses, typos showing up over long text, and inertia-decay shape.

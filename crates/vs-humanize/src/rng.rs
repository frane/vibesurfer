//! Seeded deterministic PRNG.
//!
//! We use `xoshiro256**` (Blackman & Vigna, 2018). It's a small,
//! statistically excellent, non-cryptographic generator that fits in
//! ~30 lines of std-only Rust. We do not pull `rand` because
//! vibesurfer's engine crate is already heavy and `vs-humanize` is on
//! the engine's dependency chain — staying dep-free here saves ~40
//! transitive crates.
//!
//! `seed_from_u64` does one round of `splitmix64` to spread a single
//! u64 into the four words `xoshiro256**` needs, matching the standard
//! recipe so test seeds are interoperable with any external reference.

/// Stateful PRNG. Construct via [`Rng::seed_from_u64`].
pub(crate) struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Seed from a single u64 by running `splitmix64` four times.
    /// Matches the reference recipe so the same seed produces the
    /// same stream as any other `xoshiro256**` implementation.
    pub(crate) fn seed_from_u64(seed: u64) -> Self {
        let mut s = seed;
        let mut out = [0u64; 4];
        for slot in &mut out {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *slot = z ^ (z >> 31);
        }
        Self { s: out }
    }

    /// Next 64-bit word from the stream.
    pub(crate) fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform `f64` in `[0, 1)`. Uses the 53 high bits of one draw,
    /// which is the standard recipe — IEEE-754 doubles only have 53
    /// bits of mantissa, so wider draws don't add information.
    pub(crate) fn next_f64(&mut self) -> f64 {
        // 2^53 is exact in f64; this is the standard recipe.
        // `9_007_199_254_740_992.0` is `1u64 << 53` written as an
        // exact f64 literal, avoiding the cast clippy correctly
        // warns about for `u64 as f64`.
        const SCALE: f64 = 9_007_199_254_740_992.0;
        let bits = self.next_u64() >> 11;
        #[allow(clippy::cast_precision_loss)]
        let f = bits as f64;
        f / SCALE
    }

    /// Sample from a standard normal via Box-Muller. Single draw —
    /// the second normal from the pair is discarded since callers
    /// don't typically need pairs and caching would complicate the
    /// state machine.
    pub(crate) fn next_normal(&mut self) -> f64 {
        // Avoid u=0 to keep `ln` finite.
        let mut u1 = self.next_f64();
        while u1 <= f64::EPSILON {
            u1 = self.next_f64();
        }
        let u2 = self.next_f64();
        let mag = (-2.0 * u1.ln()).sqrt();
        mag * (2.0 * core::f64::consts::PI * u2).cos()
    }

    /// Uniform `f64` in `[lo, hi)`. Convenience.
    pub(crate) fn next_uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn deterministic_same_seed() {
        let mut a = Rng::seed_from_u64(42);
        let mut b = Rng::seed_from_u64(42);
        for _ in 0..32 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::seed_from_u64(1);
        let mut b = Rng::seed_from_u64(2);
        // First word must differ for any reasonable PRNG.
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn f64_in_unit_range() {
        let mut r = Rng::seed_from_u64(7);
        for _ in 0..1000 {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v), "f64 out of [0,1): {v}");
        }
    }

    #[test]
    fn normal_distribution_roughly_centered() {
        // 10k Box-Muller draws should average near 0 with stdev ~1.
        let mut r = Rng::seed_from_u64(13);
        let n: i32 = 10_000;
        let n_f = f64::from(n);
        let samples: Vec<f64> = (0..n).map(|_| r.next_normal()).collect();
        let mean: f64 = samples.iter().sum::<f64>() / n_f;
        let var: f64 = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n_f;
        assert!(mean.abs() < 0.1, "mean too far from 0: {mean}");
        assert!(
            (var.sqrt() - 1.0).abs() < 0.1,
            "stdev too far from 1: {}",
            var.sqrt()
        );
    }
}

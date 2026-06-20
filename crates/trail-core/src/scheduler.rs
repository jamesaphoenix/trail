//! Scoring: turns (staleness, static weight, strategy) into the per-folder
//! priority that is frozen into a sweep at open time. Higher score = handed out
//! first. Every function is pure and takes its inputs explicitly (no clock, no
//! global RNG), so it is trivially testable and reproducible.

use crate::model::Strategy;

/// Recency priority in `[0, 1]` from staleness via exponential decay.
///
/// A never-visited folder (or one visited a long time ago) approaches 1.0; a
/// just-visited folder approaches 0.0. After exactly one half-life, a folder
/// sits at ~0.5.
pub fn recency_priority(staleness_secs: i64, half_life_secs: u64) -> f64 {
    if staleness_secs <= 0 {
        return 0.0; // visited "now" or with a skewed clock: lowest priority.
    }
    let hl = half_life_secs.max(1) as f64;
    1.0 - (-std::f64::consts::LN_2 * staleness_secs as f64 / hl).exp()
}

/// Normalize a raw static signal to `[0, 1]` against the sweep's max value.
pub fn normalize(value: i64, max: i64) -> f64 {
    if max <= 0 {
        0.0
    } else {
        (value as f64 / max as f64).clamp(0.0, 1.0)
    }
}

/// Deterministic pseudo-random unit value in `[0, 1)` for `(seed, sweep, path)`.
///
/// Used by the `random` strategy and as a tiny tie-break elsewhere. Built from
/// an FNV-1a pass over the path mixed with a splitmix64 finalizer, so it needs
/// no RNG state and is reproducible across runs and machines.
pub fn deterministic_unit(seed: u64, sweep: i64, path: &str) -> f64 {
    let mut h: u64 =
        seed ^ 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(sweep as u64 ^ 0xD1B5_4A32_D192_ED03);
    for b in path.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3); // FNV-1a prime
    }
    // splitmix64 finalizer for good avalanche.
    let mut z = h.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 53 bits -> [0, 1).
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Final priority for a folder in a sweep.
///
/// - `round-robin`: pure recency (least-recently-visited first).
/// - `weighted`: `alpha * recency + (1 - alpha) * weight`.
/// - `random`: the deterministic unit value (recency/weight ignored).
///
/// For round-robin and weighted a vanishing tie-break (seeded by the sweep
/// number) is added so the order is total and the set of folders surfaced first
/// rotates from sweep to sweep, instead of always truncating the same tail.
pub fn score(
    strategy: Strategy,
    recency: f64,
    weight: f64,
    alpha: f64,
    seed: u64,
    sweep: i64,
    path: &str,
) -> f64 {
    match strategy {
        Strategy::Random => deterministic_unit(seed, sweep, path),
        Strategy::RoundRobin => recency + tie_break(seed, sweep, path),
        Strategy::Weighted => {
            let a = alpha.clamp(0.0, 1.0);
            a * recency + (1.0 - a) * weight + tie_break(seed, sweep, path)
        }
    }
}

/// A negligible, sweep-varying nudge to make orderings total and rotate the
/// truncation tail across sweeps.
fn tie_break(seed: u64, sweep: i64, path: &str) -> f64 {
    1e-9 * deterministic_unit(seed.wrapping_add(sweep as u64), 0, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_monotonic_and_bounded() {
        let hl = 100;
        let never = recency_priority(i64::MAX / 2, hl);
        let old = recency_priority(1000, hl);
        let recent = recency_priority(10, hl);
        let now = recency_priority(0, hl);
        assert!((0.0..=1.0).contains(&never));
        assert!(never > old, "older = higher priority");
        assert!(old > recent);
        assert!(recent > now);
        assert_eq!(now, 0.0);
    }

    #[test]
    fn recency_half_life_midpoint() {
        // One half-life of staleness -> ~0.5.
        let r = recency_priority(100, 100);
        assert!((r - 0.5).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn normalize_bounds() {
        assert_eq!(normalize(0, 0), 0.0);
        assert_eq!(normalize(5, 10), 0.5);
        assert_eq!(normalize(20, 10), 1.0); // clamped
    }

    #[test]
    fn deterministic_unit_is_stable_and_in_range() {
        for sweep in 0..5 {
            for path in ["a", "src/api", "crates/trail-core"] {
                let v = deterministic_unit(42, sweep, path);
                assert!((0.0..1.0).contains(&v), "v={v}");
                // Reproducible.
                assert_eq!(v, deterministic_unit(42, sweep, path));
            }
        }
    }

    #[test]
    fn random_strategy_changes_order_across_sweeps() {
        let paths = ["a", "b", "c", "d", "e", "f"];
        let order = |sweep: i64| {
            let mut v: Vec<_> = paths
                .iter()
                .map(|p| (score(Strategy::Random, 1.0, 1.0, 0.6, 1, sweep, p), *p))
                .collect();
            v.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap());
            v.into_iter().map(|(_, p)| p).collect::<Vec<_>>()
        };
        assert_ne!(order(1), order(2), "seeded random should rotate by sweep");
    }

    #[test]
    fn weighted_blends_recency_and_weight() {
        // alpha=1.0 -> pure recency; weight irrelevant.
        let a = score(Strategy::Weighted, 0.9, 0.1, 1.0, 1, 1, "x");
        assert!((a - (0.9 + tie_break(1, 1, "x"))).abs() < 1e-12);
        // alpha=0.0 -> pure weight.
        let b = score(Strategy::Weighted, 0.9, 0.1, 0.0, 1, 1, "x");
        assert!((b - (0.1 + tie_break(1, 1, "x"))).abs() < 1e-12);
    }

    proptest::proptest! {
        #[test]
        fn score_is_always_finite(
            recency in 0.0f64..=1.0,
            weight in 0.0f64..=1.0,
            alpha in -1.0f64..=2.0,
            sweep in 1i64..10_000,
        ) {
            for strat in [Strategy::RoundRobin, Strategy::Weighted, Strategy::Random] {
                let s = score(strat, recency, weight, alpha, 7, sweep, "some/path/here");
                proptest::prop_assert!(s.is_finite(), "score not finite: {}", s);
            }
        }
    }
}

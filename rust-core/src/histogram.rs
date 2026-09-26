//! Log-linear latency histogram: exact below 16 ns, then 8 sub-buckets per
//! power of two (≤ 12.5 % relative error), up to ~2^43 ns.

use crate::layout::HIST_BUCKETS;

#[inline]
pub fn bucket(ns: u64) -> usize {
    if ns < 16 {
        return ns as usize;
    }
    let e = 63 - ns.leading_zeros() as usize; // >= 4
    let sub = ((ns >> (e - 3)) & 7) as usize;
    (16 + (e - 4) * 8 + sub).min(HIST_BUCKETS - 1)
}

/// Upper bound (exclusive) of a bucket, in nanoseconds.
pub fn upper(idx: usize) -> u64 {
    if idx < 16 {
        return idx as u64 + 1;
    }
    let e = (idx - 16) / 8 + 4;
    let sub = ((idx - 16) % 8) as u64;
    (1u64 << e) + ((sub + 1) << (e - 3))
}

/// Value at quantile `q` (0..=1) of an aggregated histogram.
pub fn quantile(h: &[u64], q: f64) -> Option<u64> {
    let total: u64 = h.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = ((total as f64) * q).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, &c) in h.iter().enumerate() {
        seen += c;
        if seen >= rank {
            return Some(upper(i));
        }
    }
    Some(upper(h.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_monotonic_and_bounded() {
        let mut last = 0;
        for ns in [
            0u64,
            1,
            15,
            16,
            17,
            31,
            32,
            100,
            1_000,
            10_000,
            1 << 30,
            u64::MAX,
        ] {
            let b = bucket(ns);
            assert!(b >= last);
            assert!(b < HIST_BUCKETS);
            if b < HIST_BUCKETS - 1 {
                assert!(ns < upper(b), "{ns} < upper({b})={}", upper(b));
            }
            last = b;
        }
    }

    #[test]
    fn quantiles() {
        let mut h = vec![0u64; HIST_BUCKETS];
        for ns in 1..=100u64 {
            h[bucket(ns * 10)] += 1;
        }
        let p50 = quantile(&h, 0.5).unwrap();
        assert!((450..=600).contains(&p50), "{p50}");
        let p99 = quantile(&h, 0.99).unwrap();
        assert!(p99 >= 990, "{p99}");
    }
}

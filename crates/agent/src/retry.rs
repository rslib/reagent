//! Backoff scheduling for retried provider turns.

use std::time::Duration;

/// How many times a failed turn is retried after its first failure.
pub const MAX_RETRIES: u32 = 4;
/// The cap on a provider's `retry-after` delay.
const MAX_PROVIDER_WAIT: Duration = Duration::from_secs(60);
/// The exponential backoff base: half a second.
const BASE: Duration = Duration::from_millis(500);
/// The cap on the exponentiated backoff before jitter.
const MAX_BACKOFF: Duration = Duration::from_secs(8);
/// The low end of the jitter factor.
const JITTER_MIN: f64 = 0.75;

/// The delay before retry `attempt` (the first retry is attempt 0), where
/// `provider_wait` is the delay the provider asked for, if any, and `jitter` a
/// factor in 0.75..=1.0.
///
/// A provider-sent delay wins and is capped at 60 s. Otherwise the delay is
/// `0.5 s * 2^attempt`, capped at 8 s, times `jitter`.
pub fn delay(attempt: u32, provider_wait: Option<Duration>, jitter: f64) -> Duration {
    if let Some(wait) = provider_wait {
        return wait.min(MAX_PROVIDER_WAIT);
    }
    let factor = 2_u32.saturating_pow(attempt);
    let base = BASE
        .checked_mul(factor)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF);
    let jitter = jitter.clamp(JITTER_MIN, 1.0);
    Duration::from_nanos((base.as_nanos() as f64 * jitter) as u64)
}

/// A random jitter factor in 0.75..=1.0 from a cheap, non-cryptographic
/// source: the standard library's [`std::hash::RandomState`], whose per-hasher
/// seed varies with every call.
pub fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let state = std::hash::RandomState::new();
    let bits = state.build_hasher().finish();
    let unit = (bits >> 11) as f64 / (1u64 << 53) as f64;
    JITTER_MIN + (1.0 - JITTER_MIN) * unit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_are_capped_at_four() {
        assert_eq!(MAX_RETRIES, 4);
    }

    #[test]
    fn provider_wait_wins_and_is_capped_at_a_minute() {
        assert_eq!(
            delay(2, Some(Duration::from_secs(3)), 1.0),
            Duration::from_secs(3)
        );
        assert_eq!(
            delay(2, Some(Duration::from_secs(120)), 1.0),
            Duration::from_secs(60),
            "a long provider delay is capped at 60 s"
        );
    }

    #[test]
    fn backoff_doubles_and_caps_at_eight_seconds() {
        assert_eq!(delay(0, None, 1.0), Duration::from_millis(500));
        assert_eq!(delay(1, None, 1.0), Duration::from_millis(1000));
        assert_eq!(delay(2, None, 1.0), Duration::from_millis(2000));
        assert_eq!(delay(3, None, 1.0), Duration::from_millis(4000));
        assert_eq!(delay(10, None, 1.0), Duration::from_secs(8));
    }

    #[test]
    fn jitter_scales_the_delay_down_but_not_up() {
        let base = delay(1, None, 1.0);
        let low = delay(1, None, 0.75);
        let mid = delay(1, None, 0.875);
        assert!(low < base);
        assert!(mid > low && mid < base);
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let value = jitter();
            assert!((0.75..=1.0).contains(&value), "{value}");
        }
    }
}

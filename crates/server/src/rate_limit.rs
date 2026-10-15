//! In-memory token bucket rate limiter, keyed by API token id or client address.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// A token bucket per key.
pub struct RateLimiter<K> {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<K, Bucket>>,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    /// `per_minute` sustained requests, up to `burst` at once.
    pub fn new(per_minute: u32, burst: u32) -> Self {
        Self {
            capacity: f64::from(burst.max(1)),
            refill_per_sec: f64::from(per_minute.max(1)) / 60.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token. On refusal returns how long to wait.
    pub fn check(&self, key: &K) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &K, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.buckets.lock().expect("rate limiter lock poisoned");
        let bucket = buckets.entry(key.clone()).or_insert(Bucket {
            tokens: self.capacity,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let missing = 1.0 - bucket.tokens;
            Err(Duration::from_secs_f64(missing / self.refill_per_sec))
        }
    }

    /// Drops buckets that are full again, so the map does not grow forever.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock poisoned");
        buckets.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
            b.tokens + elapsed * self.refill_per_sec < self.capacity
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refill() {
        let limiter = RateLimiter::new(60, 3);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at(&"a", start).is_ok());
        }
        let wait = limiter.check_at(&"a", start).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        // Other keys are independent.
        assert!(limiter.check_at(&"b", start).is_ok());
        // One token per second refills.
        assert!(limiter.check_at(&"a", start + Duration::from_millis(1100)).is_ok());
    }
}

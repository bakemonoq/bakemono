use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

// per-client token bucket applied only to cold cache misses: a miss joins a swarm, so a flood of them
// stampedes the engine. cache hits serve from disk and are never limited
pub struct ColdLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

// prune idle (full) buckets once the table grows past this, so tracking stays bounded under an IP flood
const MAX_TRACKED: usize = 50_000;

impl ColdLimiter {
    // BAKEMONO_COLD_MISS_BURST is the bucket size (default 20, 0 disables); BAKEMONO_COLD_MISS_REFILL is
    // tokens added per second (default 1), the sustained cold-fetch rate a single client may hold
    pub fn from_env() -> Self {
        Self::new(
            env_f64("BAKEMONO_COLD_MISS_BURST", 20.0),
            env_f64("BAKEMONO_COLD_MISS_REFILL", 1.0),
        )
    }

    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    // true if the client may drive one more cold miss now; false once its bucket is empty
    pub fn allow(&self, client: IpAddr) -> bool {
        if self.capacity <= 0.0 {
            return true; // disabled
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        if buckets.len() > MAX_TRACKED {
            buckets.retain(|_, b| b.tokens < self.capacity);
        }
        let b = buckets.entry(client).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_after_burst_and_isolates_clients() {
        let l = ColdLimiter::new(2.0, 0.0); // no refill, so the burst is all a client gets
        let a: IpAddr = "1.2.3.4".parse().unwrap();
        let b: IpAddr = "5.6.7.8".parse().unwrap();
        assert!(l.allow(a));
        assert!(l.allow(a));
        assert!(!l.allow(a), "third cold miss over a burst of 2 is denied");
        assert!(l.allow(b), "a different client has its own bucket");
    }

    #[test]
    fn zero_capacity_disables_the_limit() {
        let l = ColdLimiter::new(0.0, 0.0);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..100 {
            assert!(l.allow(ip));
        }
    }
}

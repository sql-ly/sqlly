use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TokenBucket {
    capacity: u32,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn per_minute(n: u32) -> Self {
        Self {
            capacity: n,
            tokens: n as f64,
            refill_per_sec: n as f64 / 60.0,
            last: Instant::now(),
        }
    }

    pub fn try_take(&mut self, now: Instant) -> Result<(), Duration> {
        if self.capacity == 0 {
            return Err(Duration::from_secs(3600));
        }
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity as f64);
        self.last = now;
        if self.tokens < 1.0 {
            let needed = 1.0 - self.tokens;
            let wait_secs = needed / self.refill_per_sec;
            return Err(Duration::from_secs_f64(wait_secs));
        }
        self.tokens -= 1.0;
        Ok(())
    }
}

pub enum RateClass {
    Query,
    Write,
    Ddl,
}

pub struct RateLimiter {
    pub queries: Mutex<TokenBucket>,
    pub writes: Mutex<TokenBucket>,
    pub ddl: Mutex<TokenBucket>,
}

impl RateLimiter {
    pub fn from_policy(p: &crate::mcp::policy::McpPolicy) -> Self {
        Self {
            queries: Mutex::new(TokenBucket::per_minute(p.max_queries_per_minute)),
            writes: Mutex::new(TokenBucket::per_minute(p.max_writes_per_minute)),
            ddl: Mutex::new(TokenBucket::per_minute(p.max_ddl_per_minute)),
        }
    }

    pub fn check(&self, class: RateClass, now: Instant) -> Result<(), Duration> {
        match class {
            RateClass::Query => self.queries.lock().unwrap().try_take(now),
            RateClass::Write => self.writes.lock().unwrap().try_take(now),
            RateClass::Ddl => self.ddl.lock().unwrap().try_take(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::policy::McpPolicy;

    #[test]
    fn bucket_allows_capacity_then_refuses() {
        let mut bucket = TokenBucket::per_minute(2);
        let now = Instant::now();
        assert!(bucket.try_take(now).is_ok());
        assert!(bucket.try_take(now).is_ok());
        assert!(bucket.try_take(now).is_err());
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut bucket = TokenBucket::per_minute(60);
        let start = Instant::now();
        for _ in 0..60 {
            assert!(bucket.try_take(start).is_ok());
        }
        assert!(bucket.try_take(start).is_err());
        let later = start + Duration::from_secs(30);
        for _ in 0..30 {
            assert!(bucket.try_take(later).is_ok());
        }
    }

    #[test]
    fn zero_capacity_always_refuses() {
        let mut bucket = TokenBucket::per_minute(0);
        let now = Instant::now();
        assert!(bucket.try_take(now).is_err());
        let later = now + Duration::from_secs(60);
        assert!(bucket.try_take(later).is_err());
    }

    #[test]
    fn limiter_classes_are_independent() {
        let policy = McpPolicy {
            max_queries_per_minute: 1,
            max_writes_per_minute: 1,
            max_ddl_per_minute: 0,
            ..Default::default()
        };
        let limiter = RateLimiter::from_policy(&policy);
        let now = Instant::now();
        assert!(limiter.check(RateClass::Query, now).is_ok());
        assert!(limiter.check(RateClass::Query, now).is_err());
        assert!(limiter.check(RateClass::Write, now).is_ok());
        assert!(limiter.check(RateClass::Ddl, now).is_err());
    }
}

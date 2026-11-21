//! Speed limits: one token bucket per direction, shared by every transfer
//! going that way, so that the total stays under the limit however many run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::lock;

/// The longest nap between two looks at the bucket: a limit raised or
/// lifted takes effect at least this fast.
const MAX_NAP: Duration = Duration::from_millis(200);
/// What may go through at full speed after a pause, in seconds of the rate.
const BURST_SECONDS: f64 = 0.25;
/// Never less than this in the bucket, so that one read of a stream fits.
const MIN_BURST: f64 = 64.0 * 1024.0;

#[derive(Default)]
pub struct Limiter {
    /// Bytes per second; 0 means no limit.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

impl Limiter {
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Changes the limit, running transfers included.
    pub fn set_rate(&self, bytes_per_second: u64) {
        self.rate.store(bytes_per_second, Ordering::Relaxed);
    }

    /// Waits until `bytes` may go through. Returns at once without a limit.
    /// Each waits for its own turn: those who took earlier go first.
    pub async fn take(&self, bytes: u64) {
        let rate = self.rate();
        if rate == 0 || bytes == 0 {
            return;
        }
        let Some(target) = lock(&self.bucket).take(Instant::now(), bytes, rate) else {
            return;
        };
        loop {
            // The limit may change meanwhile: its pace is looked at again.
            let rate = self.rate();
            if rate == 0 {
                lock(&self.bucket).clear();
                return;
            }
            let wait = lock(&self.bucket).until(Instant::now(), rate, target);
            if wait.is_zero() {
                return;
            }
            tokio::time::sleep(wait.min(MAX_NAP)).await;
        }
    }
}

/// Bytes allowed through, refilled at the rate. Below zero, what was taken
/// in advance and must be waited for.
#[derive(Default)]
struct Bucket {
    tokens: f64,
    /// When the tokens were last counted; `None` before the first use.
    at: Option<Instant>,
    /// Everything ever refilled: a waiter goes once it reaches the point
    /// its debt ends at.
    refilled: f64,
}

impl Bucket {
    fn capacity(rate: u64) -> f64 {
        (rate as f64 * BURST_SECONDS).max(MIN_BURST)
    }

    fn refill(&mut self, now: Instant, rate: u64) {
        let capacity = Self::capacity(rate);
        match self.at {
            None => self.tokens = capacity,
            Some(at) => {
                let elapsed = now.saturating_duration_since(at).as_secs_f64();
                let before = self.tokens;
                self.tokens = (before + elapsed * rate as f64).min(capacity);
                self.refilled += (self.tokens - before).max(0.0);
            }
        }
        self.at = Some(now);
    }

    /// Takes `bytes` at `now`. `None` when they may go at once, otherwise
    /// how far the refill must go before they may.
    fn take(&mut self, now: Instant, bytes: u64, rate: u64) -> Option<f64> {
        self.refill(now, rate);
        self.tokens -= bytes as f64;
        (self.tokens < 0.0).then_some(self.refilled - self.tokens)
    }

    /// How long until the refill reaches `target`, at today's rate.
    fn until(&mut self, now: Instant, rate: u64, target: f64) -> Duration {
        self.refill(now, rate);
        let missing = target - self.refilled;
        if missing <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(missing / rate as f64)
        }
    }

    fn clear(&mut self) {
        self.tokens = 0.0;
        self.at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends `total` bytes in `chunk` pieces through a bucket, on a clock
    /// that only moves when told to wait. Returns the simulated duration.
    fn simulate(rate: u64, total: u64, chunk: u64) -> f64 {
        let start = Instant::now();
        let mut now = start;
        let mut bucket = Bucket::default();
        let mut sent = 0;
        while sent < total {
            if let Some(target) = bucket.take(now, chunk, rate) {
                loop {
                    let wait = bucket.until(now, rate, target);
                    if wait.is_zero() {
                        break;
                    }
                    now += wait.min(MAX_NAP);
                }
            }
            sent += chunk;
        }
        now.duration_since(start).as_secs_f64()
    }

    #[test]
    fn the_bucket_keeps_to_the_rate() {
        // 10 Mo at 1 Mo/s: ten seconds, less the burst allowed at first.
        let rate = 1024 * 1024;
        let elapsed = simulate(rate, 10 * rate, 32 * 1024);
        let burst = Bucket::capacity(rate) / rate as f64;
        assert!((elapsed - (10.0 - burst)).abs() < 0.05, "took {elapsed} s");

        // Big chunks at a low rate: still the rate, on average.
        let rate = 50 * 1024;
        let elapsed = simulate(rate, 1024 * 1024, 256 * 1024);
        let expected = (1024.0 * 1024.0 - Bucket::capacity(rate)) / rate as f64;
        assert!((elapsed - expected).abs() < 0.05, "took {elapsed} s, not {expected}");
    }

    #[test]
    fn a_quiet_bucket_only_saves_up_a_short_burst() {
        let rate = 100_000;
        let mut bucket = Bucket::default();
        let start = Instant::now();
        assert!(bucket.take(start, 1, rate).is_none());
        // A minute later, no more than the burst is allowed through at once.
        let later = start + Duration::from_secs(60);
        let target = bucket.take(later, 10 * rate, rate).expect("a wait");
        let wait = bucket.until(later, rate, target);
        let expected = 10.0 - Bucket::capacity(rate) / rate as f64;
        assert!((wait.as_secs_f64() - expected).abs() < 0.01, "{wait:?}");
    }

    #[test]
    fn each_waits_for_its_own_turn_and_follows_a_new_rate() {
        let rate = 1000;
        let mut bucket = Bucket::default();
        let start = Instant::now();
        // The burst (64 Kio here) goes at once; then each in its turn.
        assert!(bucket.take(start, 64 * 1024, rate).is_none());
        let first = bucket.take(start, 1000, rate).unwrap();
        let second = bucket.take(start, 3000, rate).unwrap();
        assert_eq!(bucket.until(start, rate, first), Duration::from_secs(1));
        assert_eq!(bucket.until(start, rate, second), Duration::from_secs(4));
        // Half a second later the rate doubles: what is left goes twice as fast.
        let later = start + Duration::from_millis(500);
        assert_eq!(bucket.until(later, rate, first), Duration::from_millis(500));
        assert_eq!(bucket.until(later, 2 * rate, first), Duration::from_millis(250));
        assert_eq!(bucket.until(later, 2 * rate, second), Duration::from_millis(1750));
    }

    /// The real thing: two transfers share one limit, which is then lifted.
    #[tokio::test]
    async fn transfers_share_the_limit_and_follow_its_changes() {
        let limiter = std::sync::Arc::new(Limiter::default());
        limiter.set_rate(2 * 1024 * 1024);
        let start = Instant::now();
        let send = |limiter: std::sync::Arc<Limiter>| async move {
            for _ in 0..32 {
                limiter.take(64 * 1024).await;
            }
        };
        // 2 × 2 Mo at 2 Mo/s, minus the first burst.
        tokio::join!(send(limiter.clone()), send(limiter.clone()));
        let elapsed = start.elapsed().as_secs_f64();
        assert!((1.6..2.4).contains(&elapsed), "took {elapsed} s");

        // A transfer waiting on a low limit goes on as soon as it is lifted.
        limiter.set_rate(10 * 1024);
        let waiting = tokio::spawn({
            let limiter = limiter.clone();
            async move { limiter.take(1024 * 1024).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "a 100 s wait cannot be over yet");
        let lifted = Instant::now();
        limiter.set_rate(0);
        waiting.await.unwrap();
        assert!(lifted.elapsed() < MAX_NAP * 2);
    }
}

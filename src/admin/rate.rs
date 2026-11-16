//! Bounded sliding-window counters, never trusting client-supplied forwarded IPs.

use super::AdminError;
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct RateLimits(Mutex<HashMap<String, Window>>);
struct Window {
    entries: VecDeque<Instant>,
    period: Duration,
}

impl RateLimits {
    pub fn check(&self, key: String, limit: usize, period: Duration) -> Result<(), AdminError> {
        let now = Instant::now();
        let mut counters = self.0.lock().map_err(|_| AdminError::Unavailable)?;
        counters.retain(|_, window| {
            while window
                .entries
                .front()
                .is_some_and(|time| now.duration_since(*time) >= window.period)
            {
                window.entries.pop_front();
            }
            !window.entries.is_empty()
        });
        if counters.len() >= 4096 && !counters.contains_key(&key) {
            return Err(AdminError::RateLimited);
        }
        let window = counters.entry(key).or_insert_with(|| Window {
            entries: VecDeque::new(),
            period,
        });
        if window.entries.len() >= limit {
            return Err(AdminError::RateLimited);
        }
        window.entries.push_back(now);
        Ok(())
    }
}

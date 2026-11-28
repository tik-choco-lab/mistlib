use std::time::{Duration, Instant};

/// Two ProbeBW cycles, in virtual (probe) time.
#[derive(Clone, Default)]
pub(super) struct MaxBw {
    previous: u64,
    current: u64,
}

impl MaxBw {
    pub fn observe(&mut self, rate: u64, app_limited: bool) {
        if !app_limited || rate >= self.get() {
            self.current = self.current.max(rate);
        }
    }
    pub fn advance(&mut self) {
        if self.current != 0 {
            self.previous = std::mem::take(&mut self.current);
        }
    }
    pub fn get(&self) -> u64 {
        self.previous.max(self.current)
    }
}

/// The draft's two timestamped minima (UpdateMinRTT), not lifetime RTT.
#[derive(Clone)]
pub(super) struct MinRtt {
    pub value: Duration,
    stamp: Instant,
    probe_min: Duration,
    pub probe_stamp: Instant,
}

impl MinRtt {
    pub fn new(now: Instant) -> Self {
        Self {
            value: Duration::MAX,
            stamp: now,
            probe_min: Duration::MAX,
            probe_stamp: now,
        }
    }
    pub fn update(&mut self, now: Instant, rtt: Duration) -> bool {
        let expired = now - self.probe_stamp > Duration::from_secs(5);
        if rtt < self.probe_min || expired {
            self.probe_min = rtt;
            self.probe_stamp = now;
        }
        if self.probe_min < self.value || now - self.stamp > Duration::from_secs(10) {
            self.value = self.probe_min;
            self.stamp = self.probe_stamp;
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_covers_two_cycles_without_low_app_limited_samples() {
        let mut f = MaxBw::default();
        f.observe(90, false);
        f.advance();
        f.observe(10, true);
        f.observe(70, false);
        assert_eq!(f.get(), 90);
        f.advance();
        assert_eq!(f.get(), 70);
        f.observe(100, true);
        assert_eq!(f.get(), 100);
    }

    #[test]
    fn rtt_minimum_and_probe_expiry_have_separate_windows() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut f = MinRtt::new(t);
        assert!(!f.update(t, ms(110)));
        assert!(!f.update(t + ms(4000), ms(100)));
        assert!(f.update(t + ms(9100), ms(121)));
        assert_eq!(f.value, ms(100));
        assert!(f.update(t + ms(14200), ms(120)));
        assert_eq!(f.value, ms(120));
    }
}

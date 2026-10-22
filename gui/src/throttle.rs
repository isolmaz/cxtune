//! Lets a value that changes many times a second through at a steady pace, and never loses the
//! last one.

use std::time::{Duration, Instant};

pub struct Throttle<T> {
    interval: Duration,
    last_sent: Option<Instant>,
    waiting: Option<T>,
}

impl<T> Throttle<T> {
    pub fn new(interval: Duration) -> Throttle<T> {
        Throttle {
            interval,
            last_sent: None,
            waiting: None,
        }
    }

    /// Takes the newest value. It is returned if it may be sent now; otherwise it waits, replacing
    /// an older one that waited, until [`Throttle::due_in`] has passed or [`Throttle::flush`].
    pub fn offer(&mut self, now: Instant, value: T) -> Option<T> {
        if self.waiting.is_none() && self.remaining(now).is_zero() {
            self.last_sent = Some(now);
            return Some(value);
        }
        self.waiting = Some(value);
        None
    }

    /// How long until the waiting value may be sent; `None` when nothing waits.
    pub fn due_in(&self, now: Instant) -> Option<Duration> {
        self.waiting.is_some().then(|| self.remaining(now))
    }

    fn remaining(&self, now: Instant) -> Duration {
        self.last_sent.map_or(Duration::ZERO, |last| {
            self.interval
                .saturating_sub(now.saturating_duration_since(last))
        })
    }

    /// The waiting value, which is sent now whatever the pace.
    pub fn flush(&mut self, now: Instant) -> Option<T> {
        let value = self.waiting.take()?;
        self.last_sent = Some(now);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: Duration = Duration::from_millis(100);

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn the_first_value_goes_through_at_once() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(STEP);
        assert_eq!(throttle.offer(t0, 1), Some(1));
        assert_eq!(throttle.due_in(t0), None);
    }

    #[test]
    fn values_that_come_too_soon_wait_and_only_the_newest_is_kept() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(STEP);
        assert_eq!(throttle.offer(t0, 1), Some(1));
        assert_eq!(throttle.offer(at(t0, 10), 2), None);
        assert_eq!(throttle.offer(at(t0, 20), 3), None);
        assert_eq!(throttle.offer(at(t0, 30), 4), None);
        assert!(throttle.due_in(at(t0, 30)).is_some());
        assert_eq!(throttle.flush(at(t0, 40)), Some(4));
        assert_eq!(throttle.due_in(at(t0, 40)), None);
        assert_eq!(throttle.flush(at(t0, 50)), None);
    }

    #[test]
    fn the_waiting_value_says_how_long_it_waits() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(STEP);
        assert_eq!(throttle.due_in(t0), None);
        throttle.offer(t0, 1);
        assert_eq!(throttle.offer(at(t0, 30), 2), None);
        assert_eq!(throttle.due_in(at(t0, 30)), Some(Duration::from_millis(70)));
        assert_eq!(throttle.due_in(at(t0, 100)), Some(Duration::ZERO));
        throttle.flush(at(t0, 100));
        assert_eq!(throttle.due_in(at(t0, 100)), None);
    }

    #[test]
    fn a_value_after_the_interval_goes_through_unless_one_still_waits() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(STEP);
        throttle.offer(t0, 1);
        assert_eq!(throttle.offer(at(t0, 150), 2), Some(2));
        assert_eq!(throttle.offer(at(t0, 160), 3), None);
        // 3 is overdue by now but was not sent; the newer value takes its place
        assert_eq!(throttle.offer(at(t0, 400), 4), None);
        assert_eq!(throttle.flush(at(t0, 400)), Some(4));
    }

    #[test]
    fn a_flush_restarts_the_interval() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(STEP);
        throttle.offer(t0, 1);
        throttle.offer(at(t0, 10), 2);
        assert_eq!(throttle.flush(at(t0, 20)), Some(2));
        assert_eq!(throttle.offer(at(t0, 60), 3), None);
        assert_eq!(throttle.due_in(at(t0, 60)), Some(Duration::from_millis(60)));
    }
}

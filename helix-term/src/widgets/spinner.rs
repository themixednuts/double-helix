use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Spinner {
    frames: &'static [&'static str],
    interval: Duration,
    started_at: Instant,
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new(&["◐", "◓", "◑", "◒"], Duration::from_millis(120))
    }
}

impl Spinner {
    pub fn new(frames: &'static [&'static str], interval: Duration) -> Self {
        assert!(!frames.is_empty());
        Self {
            frames,
            interval: interval.max(Duration::from_nanos(1)),
            started_at: Instant::now(),
        }
    }

    pub fn dots(interval: Duration) -> Self {
        Self::new(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"], interval)
    }

    pub fn restart(&mut self) {
        self.started_at = Instant::now();
    }

    pub fn frame_at(&self, now: Instant) -> &'static str {
        self.frame_for_elapsed(now.saturating_duration_since(self.started_at))
    }

    pub fn frame_for_elapsed(&self, elapsed: Duration) -> &'static str {
        let step = (elapsed.as_nanos() / self.interval.as_nanos()) % self.frames.len() as u128;
        let step = step as usize;
        self.frames[step]
    }

    pub fn next_redraw_at(&self, now: Instant) -> Instant {
        let elapsed = now.saturating_duration_since(self.started_at);
        let interval_nanos = self.interval.as_nanos();
        let elapsed_in_frame = elapsed.as_nanos() % interval_nanos;
        let remaining = interval_nanos - elapsed_in_frame;
        let remaining = Duration::from_nanos(remaining.min(u64::MAX as u128) as u64);
        now + remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_uses_configured_frames() {
        let spinner = Spinner::new(&["a", "b"], Duration::from_millis(1));
        assert!(["a", "b"].contains(&spinner.frame_at(Instant::now())));
    }

    #[test]
    fn spinner_can_render_deterministic_elapsed_frames() {
        let spinner = Spinner::new(&["a", "b"], Duration::from_millis(10));

        assert_eq!(spinner.frame_for_elapsed(Duration::from_millis(0)), "a");
        assert_eq!(spinner.frame_for_elapsed(Duration::from_millis(10)), "b");
        assert_eq!(spinner.frame_for_elapsed(Duration::from_millis(20)), "a");
    }

    #[test]
    fn next_redraw_tracks_the_next_elapsed_time_boundary() {
        let spinner = Spinner::new(&["a", "b"], Duration::from_millis(10));
        let now = spinner.started_at + Duration::from_millis(15);

        assert_eq!(
            spinner.next_redraw_at(now),
            spinner.started_at + Duration::from_millis(20)
        );
    }
}

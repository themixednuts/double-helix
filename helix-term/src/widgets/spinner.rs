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
        Self {
            frames,
            interval,
            started_at: Instant::now(),
        }
    }

    pub fn frame(&self) -> &'static str {
        let step =
            (self.started_at.elapsed().as_millis() / self.interval.as_millis().max(1)) as usize;
        self.frames[step % self.frames.len()]
    }

    pub fn next_redraw(&self) -> Instant {
        Instant::now() + self.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_uses_configured_frames() {
        let spinner = Spinner::new(&["a", "b"], Duration::from_millis(1));
        assert!(["a", "b"].contains(&spinner.frame()));
    }
}

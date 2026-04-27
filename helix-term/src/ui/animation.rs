use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationDirection {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationFillMode {
    None,
    Forwards,
    Backwards,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationIterationCount {
    Count(u32),
    Infinite,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimationTimingFunction {
    Linear,
    EaseOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationPlayState {
    Idle,
    Running,
    Paused,
}

#[derive(Debug, Clone, Copy)]
pub struct AnimationSpec {
    pub duration: Duration,
    pub delay: Duration,
    pub timing_function: AnimationTimingFunction,
    pub iteration_count: AnimationIterationCount,
    pub direction: AnimationDirection,
    pub fill_mode: AnimationFillMode,
    pub frame_interval: Duration,
}

impl AnimationSpec {
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            delay: Duration::ZERO,
            timing_function: AnimationTimingFunction::Linear,
            iteration_count: AnimationIterationCount::Count(1),
            direction: AnimationDirection::Normal,
            fill_mode: AnimationFillMode::Forwards,
            frame_interval: Duration::from_millis(16),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AnimationSample {
    pub progress: f32,
    pub next_redraw: Option<Instant>,
    pub play_state: AnimationPlayState,
    pub finished: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Animation {
    spec: AnimationSpec,
    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    play_state: AnimationPlayState,
}

impl Animation {
    pub fn new(spec: AnimationSpec) -> Self {
        Self {
            spec,
            started_at: None,
            paused_at: None,
            play_state: AnimationPlayState::Idle,
        }
    }

    pub fn restart(&mut self) {
        self.started_at = Some(Instant::now());
        self.paused_at = None;
        self.play_state = AnimationPlayState::Running;
    }

    pub fn stop(&mut self) {
        self.started_at = None;
        self.paused_at = None;
        self.play_state = AnimationPlayState::Idle;
    }

    pub fn pause(&mut self) {
        if self.play_state == AnimationPlayState::Running {
            self.paused_at = Some(Instant::now());
            self.play_state = AnimationPlayState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.play_state != AnimationPlayState::Paused {
            return;
        }

        if let (Some(started_at), Some(paused_at)) = (self.started_at, self.paused_at) {
            let pause_duration = Instant::now().saturating_duration_since(paused_at);
            self.started_at = Some(started_at + pause_duration);
        }

        self.paused_at = None;
        self.play_state = AnimationPlayState::Running;
    }

    pub fn is_running(&self) -> bool {
        self.play_state == AnimationPlayState::Running
    }

    pub fn sample(&self) -> AnimationSample {
        let now = Instant::now();
        self.sample_at(now)
    }

    pub fn sample_at(&self, now: Instant) -> AnimationSample {
        let Some(started_at) = self.started_at else {
            return AnimationSample {
                progress: 0.0,
                next_redraw: None,
                play_state: AnimationPlayState::Idle,
                finished: false,
            };
        };

        let duration = self.spec.duration.max(Duration::from_millis(1));

        if self.play_state == AnimationPlayState::Paused {
            let elapsed = self
                .paused_at
                .unwrap_or(now)
                .saturating_duration_since(started_at);
            return AnimationSample {
                progress: self.progress_for_elapsed(elapsed, duration),
                next_redraw: None,
                play_state: AnimationPlayState::Paused,
                finished: false,
            };
        }

        let elapsed = now.saturating_duration_since(started_at);

        if elapsed < self.spec.delay {
            return AnimationSample {
                progress: self.initial_progress(),
                next_redraw: Some(started_at + self.spec.delay),
                play_state: self.play_state,
                finished: false,
            };
        }

        let active_elapsed = elapsed - self.spec.delay;
        let active_secs = active_elapsed.as_secs_f32();
        let duration_secs = duration.as_secs_f32().max(f32::EPSILON);
        let raw_cycles = active_secs / duration_secs;

        let (finished, cycle_index, cycle_progress) = match self.spec.iteration_count {
            AnimationIterationCount::Infinite => {
                let cycle_index = raw_cycles.floor() as u32;
                let cycle_progress = raw_cycles.fract();
                (false, cycle_index, cycle_progress)
            }
            AnimationIterationCount::Count(count) => {
                if count == 0 {
                    return AnimationSample {
                        progress: self.initial_progress(),
                        next_redraw: None,
                        play_state: AnimationPlayState::Idle,
                        finished: true,
                    };
                }

                let total_duration = duration.mul_f32(count as f32);
                if active_elapsed >= total_duration {
                    return AnimationSample {
                        progress: self.final_progress(count),
                        next_redraw: None,
                        play_state: AnimationPlayState::Idle,
                        finished: true,
                    };
                }

                let cycle_index = raw_cycles.floor() as u32;
                let cycle_progress = raw_cycles.fract();
                (false, cycle_index, cycle_progress)
            }
        };

        let progress = self.apply_timing(self.progress_for_cycle(cycle_index, cycle_progress));

        AnimationSample {
            progress,
            next_redraw: if finished || self.play_state != AnimationPlayState::Running {
                None
            } else {
                Some(now + self.spec.frame_interval)
            },
            play_state: self.play_state,
            finished,
        }
    }

    fn progress_for_elapsed(&self, elapsed: Duration, duration: Duration) -> f32 {
        let t = (elapsed.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON)).fract();
        self.apply_timing(self.progress_for_cycle(0, t))
    }

    fn progress_for_cycle(&self, cycle_index: u32, cycle_progress: f32) -> f32 {
        let reverse = match self.spec.direction {
            AnimationDirection::Normal => false,
            AnimationDirection::Reverse => true,
            AnimationDirection::Alternate => cycle_index % 2 == 1,
            AnimationDirection::AlternateReverse => cycle_index.is_multiple_of(2),
        };

        if reverse {
            1.0 - cycle_progress
        } else {
            cycle_progress
        }
    }

    fn initial_progress(&self) -> f32 {
        match self.spec.fill_mode {
            AnimationFillMode::Backwards | AnimationFillMode::Both => match self.spec.direction {
                AnimationDirection::Reverse | AnimationDirection::AlternateReverse => 1.0,
                AnimationDirection::Normal | AnimationDirection::Alternate => 0.0,
            },
            AnimationFillMode::None | AnimationFillMode::Forwards => 0.0,
        }
    }

    fn final_progress(&self, count: u32) -> f32 {
        match self.spec.fill_mode {
            AnimationFillMode::Forwards | AnimationFillMode::Both => {
                let last_cycle = count.saturating_sub(1);
                let terminal_progress = match self.spec.direction {
                    AnimationDirection::Normal => 1.0,
                    AnimationDirection::Reverse => 0.0,
                    AnimationDirection::Alternate => {
                        if last_cycle.is_multiple_of(2) {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    AnimationDirection::AlternateReverse => {
                        if last_cycle.is_multiple_of(2) {
                            0.0
                        } else {
                            1.0
                        }
                    }
                };
                self.apply_timing(terminal_progress)
            }
            AnimationFillMode::None | AnimationFillMode::Backwards => 0.0,
        }
    }

    fn apply_timing(&self, progress: f32) -> f32 {
        match self.spec.timing_function {
            AnimationTimingFunction::Linear => progress,
            AnimationTimingFunction::EaseOut => 1.0 - (1.0 - progress).powi(3),
        }
        .clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_animation_finishes_and_holds_forwards() {
        let mut animation = Animation::new(AnimationSpec::new(Duration::from_millis(100)));
        animation.restart();
        let start = animation.started_at.expect("animation should have started");

        let mid = animation.sample_at(start + Duration::from_millis(50));
        assert!(mid.progress > 0.0 && mid.progress < 1.0);
        assert!(mid.next_redraw.is_some());

        let end = animation.sample_at(start + Duration::from_millis(120));
        assert_eq!(end.progress, 1.0);
        assert!(end.finished);
        assert!(end.next_redraw.is_none());
    }

    #[test]
    fn alternate_reverse_starts_from_end() {
        let mut spec = AnimationSpec::new(Duration::from_millis(100));
        spec.direction = AnimationDirection::AlternateReverse;
        spec.fill_mode = AnimationFillMode::Backwards;
        let mut animation = Animation::new(spec);
        animation.restart();
        let start = animation.started_at.expect("animation should have started");
        let sample = animation.sample_at(start);
        assert_eq!(sample.progress, 1.0);
    }
}

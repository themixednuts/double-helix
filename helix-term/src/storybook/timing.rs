use std::time::Duration;

pub(super) fn tick_elapsed(tick: u64) -> Duration {
    Duration::from_millis(tick.saturating_mul(super::STORYBOOK_TICK_MS))
}

pub(super) fn storybook_tick(elapsed: Duration) -> u64 {
    let ticks = elapsed.as_millis() / u128::from(super::STORYBOOK_TICK_MS);
    ticks.min(u128::from(u64::MAX)) as u64
}

pub(super) fn pulse(tick: u64, period: u64) -> f32 {
    let period = period.max(2);
    let phase = tick % period;
    let half = period / 2;
    if phase <= half {
        phase as f32 / half.max(1) as f32
    } else {
        (period - phase) as f32 / (period - half).max(1) as f32
    }
}

pub(super) fn cycling_scroll_offset(total: usize, visible: usize, tick: u64, step: usize) -> usize {
    let max_offset = total.saturating_sub(visible);
    if max_offset == 0 {
        return 0;
    }
    (tick as usize).saturating_mul(step.max(1)) % (max_offset + 1)
}

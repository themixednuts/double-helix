//! Shared benchmark infrastructure for the `:bench` command.
//!
//! Contains [`BenchState`] (live run tracker), [`BenchSnapshot`] (periodic metrics),
//! the weighted action table, and the deterministic content generator.  All items
//! are used by both `helix-view` (editor model) and `helix-term` (event loop).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(feature = "bench")]
const SLOW_COMMAND_PHASE_US: u64 = 5_000;
#[cfg(feature = "bench")]
const SLOW_RUN_PHASE_US: u64 = 2_000;

thread_local! {
    static BENCH_COMMAND_CONTEXT: RefCell<Option<BenchCommandContext>> = const { RefCell::new(None) };
    static BENCH_RUN_CONTEXT: RefCell<Option<BenchRunContext>> = const { RefCell::new(None) };
}

/// Periodic snapshot of bench performance, logged to file.
#[derive(Clone)]
pub struct BenchSnapshot {
    pub elapsed_secs: f64,
    pub actions: u64,
    pub fps: f64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub buf_lines: usize,
    pub buf_bytes: usize,
    // Per-phase timing (averages over the snapshot window)
    pub avg_select_us: u64,
    pub avg_reset_us: u64,
    pub avg_action_us: u64,
    pub avg_render_us: u64,
    pub avg_tick_us: u64,
    pub avg_actions_per_frame: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BenchResetStats {
    pub before_lines: usize,
    pub before_bytes: usize,
    pub after_lines: usize,
    pub after_bytes: usize,
    pub layers_popped: usize,
    pub escapes_sent: usize,
    pub undo_steps: usize,
    pub reopened_scratch: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct BenchTickTrace {
    pub action_index: u64,
    pub elapsed_secs: f64,
    pub category: &'static str,
    pub macro_str: &'static str,
    pub force_insert: bool,
    pub reset: BenchResetStats,
    pub post_action_lines: usize,
    pub post_action_bytes: usize,
    pub reset_us: u64,
    pub action_us: u64,
}

#[derive(Clone, Debug)]
pub struct BenchCommandContext {
    pub seed: u64,
    pub event_log_path: Option<PathBuf>,
    pub action_index: u64,
    pub elapsed_secs: f64,
    pub category: &'static str,
    pub macro_str: &'static str,
    pub force_insert: bool,
}

#[derive(Clone, Debug)]
pub struct BenchRunContext {
    pub seed: u64,
    pub event_log_path: PathBuf,
}

pub struct BenchCommandGuard;
pub struct BenchRunGuard;

impl Drop for BenchCommandGuard {
    fn drop(&mut self) {
        BENCH_COMMAND_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = None;
        });
    }
}

impl Drop for BenchRunGuard {
    fn drop(&mut self) {
        BENCH_RUN_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = None;
        });
    }
}

pub fn enter_bench_command(ctx: BenchCommandContext) -> Option<BenchCommandGuard> {
    #[cfg(not(feature = "bench"))]
    {
        let _ = ctx;
        None
    }

    #[cfg(feature = "bench")]
    {
        ctx.event_log_path.as_ref()?;

        BENCH_COMMAND_CONTEXT.with(|slot| {
            *slot.borrow_mut() = Some(ctx);
        });
        Some(BenchCommandGuard)
    }
}

pub fn enter_bench_run(ctx: BenchRunContext) -> BenchRunGuard {
    #[cfg(not(feature = "bench"))]
    {
        let _ = ctx;
        BenchRunGuard
    }

    #[cfg(feature = "bench")]
    {
        BENCH_RUN_CONTEXT.with(|slot| {
            *slot.borrow_mut() = Some(ctx);
        });
        BenchRunGuard
    }
}

pub fn current_bench_command_context() -> Option<BenchCommandContext> {
    #[cfg(not(feature = "bench"))]
    {
        None
    }

    #[cfg(feature = "bench")]
    {
        BENCH_COMMAND_CONTEXT.with(|ctx| ctx.borrow().clone())
    }
}

pub fn log_command_phase<F>(
    command: &'static str,
    phase: &'static str,
    elapsed: Duration,
    details: F,
) where
    F: FnOnce() -> String,
{
    #[cfg(not(feature = "bench"))]
    {
        let _ = (command, phase, elapsed);
        let _ = details;
    }

    #[cfg(feature = "bench")]
    {
        let elapsed_us = elapsed.as_micros() as u64;
        if elapsed_us < SLOW_COMMAND_PHASE_US {
            return;
        }

        BENCH_COMMAND_CONTEXT.with(|ctx| {
            let binding = ctx.borrow();
            let Some(ctx) = binding.as_ref() else {
                return;
            };
            let Some(path) = &ctx.event_log_path else {
                return;
            };

            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let macro_name = if ctx.macro_str.is_empty() {
                    "<insert>"
                } else {
                    ctx.macro_str
                };
                let _ = writeln!(
                    f,
                    concat!(
                        "phase",
                        " seed={}",
                        " elapsed_s={:.3}",
                        " action_index={}",
                        " category={:?}",
                        " macro={:?}",
                        " force_insert={}",
                        " command={:?}",
                        " phase={:?}",
                        " elapsed_us={}",
                        " details={:?}"
                    ),
                    ctx.seed,
                    ctx.elapsed_secs,
                    ctx.action_index,
                    ctx.category,
                    macro_name,
                    ctx.force_insert,
                    command,
                    phase,
                    elapsed_us,
                    details(),
                );
            }
        });
    }
}

pub fn log_run_event<F>(event: &'static str, details: F)
where
    F: FnOnce() -> String,
{
    #[cfg(not(feature = "bench"))]
    {
        let _ = event;
        let _ = details;
    }

    #[cfg(feature = "bench")]
    {
        BENCH_RUN_CONTEXT.with(|run_ctx| {
            let run_binding = run_ctx.borrow();
            let Some(run_ctx) = run_binding.as_ref() else {
                return;
            };

            BENCH_COMMAND_CONTEXT.with(|cmd_ctx| {
                let cmd_binding = cmd_ctx.borrow();

                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&run_ctx.event_log_path)
                {
                    if let Some(cmd_ctx) = cmd_binding.as_ref() {
                        let macro_name = if cmd_ctx.macro_str.is_empty() {
                            "<insert>"
                        } else {
                            cmd_ctx.macro_str
                        };
                        let _ = writeln!(
                            f,
                            concat!(
                                "event",
                                " seed={}",
                                " elapsed_s={:.3}",
                                " action_index={}",
                                " category={:?}",
                                " macro={:?}",
                                " force_insert={}",
                                " event={:?}",
                                " details={:?}"
                            ),
                            cmd_ctx.seed,
                            cmd_ctx.elapsed_secs,
                            cmd_ctx.action_index,
                            cmd_ctx.category,
                            macro_name,
                            cmd_ctx.force_insert,
                            event,
                            details(),
                        );
                    } else {
                        let _ = writeln!(
                            f,
                            concat!("event", " seed={}", " event={:?}", " details={:?}"),
                            run_ctx.seed,
                            event,
                            details(),
                        );
                    }
                }
            });
        });
    }
}

pub fn log_run_phase<F>(component: &'static str, phase: &'static str, elapsed: Duration, details: F)
where
    F: FnOnce() -> String,
{
    #[cfg(not(feature = "bench"))]
    {
        let _ = (component, phase, elapsed);
        let _ = details;
    }

    #[cfg(feature = "bench")]
    {
        let elapsed_us = elapsed.as_micros() as u64;
        if elapsed_us < SLOW_RUN_PHASE_US {
            return;
        }

        BENCH_RUN_CONTEXT.with(|run_ctx| {
            let run_binding = run_ctx.borrow();
            let Some(run_ctx) = run_binding.as_ref() else {
                return;
            };

            BENCH_COMMAND_CONTEXT.with(|cmd_ctx| {
                let cmd_binding = cmd_ctx.borrow();

                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&run_ctx.event_log_path)
                {
                    if let Some(cmd_ctx) = cmd_binding.as_ref() {
                        let macro_name = if cmd_ctx.macro_str.is_empty() {
                            "<insert>"
                        } else {
                            cmd_ctx.macro_str
                        };
                        let _ = writeln!(
                            f,
                            concat!(
                                "phase",
                                " seed={}",
                                " elapsed_s={:.3}",
                                " action_index={}",
                                " category={:?}",
                                " macro={:?}",
                                " force_insert={}",
                                " command={:?}",
                                " phase={:?}",
                                " elapsed_us={}",
                                " details={:?}"
                            ),
                            cmd_ctx.seed,
                            cmd_ctx.elapsed_secs,
                            cmd_ctx.action_index,
                            cmd_ctx.category,
                            macro_name,
                            cmd_ctx.force_insert,
                            component,
                            phase,
                            elapsed_us,
                            details(),
                        );
                    } else {
                        let _ = writeln!(
                            f,
                            concat!(
                                "phase",
                                " seed={}",
                                " command={:?}",
                                " phase={:?}",
                                " elapsed_us={}",
                                " details={:?}"
                            ),
                            run_ctx.seed,
                            component,
                            phase,
                            elapsed_us,
                            details(),
                        );
                    }
                }
            });
        });
    }
}

/// Live benchmark state, driven by `:bench` command.
pub struct BenchState {
    /// Seeded PRNG for deterministic action selection.
    pub seed: u64,
    /// Index into a simple xorshift state (avoids needing `rand` in helix-view).
    pub rng_state: u64,
    /// When the bench run ends.
    pub deadline: Instant,
    /// Total duration requested.
    pub duration: Duration,
    /// Frame render durations (populated by the event loop after each render).
    pub frame_times: Vec<Duration>,
    /// Per-action durations with category labels.
    pub action_times: Vec<(&'static str, Duration)>,
    /// Rolling FPS (updated every N frames for statusline display).
    pub rolling_fps: f64,
    /// Timestamp of the last FPS calculation.
    pub last_fps_update: Instant,
    /// Frame count since last FPS update.
    pub frames_since_update: u32,
    /// Total actions executed.
    pub actions_executed: u64,
    /// Periodic snapshots for diagnostics.
    pub snapshots: Vec<BenchSnapshot>,
    /// Last time a snapshot was taken.
    pub last_snapshot: Instant,
    /// Log file path (if any).
    pub log_path: Option<std::path::PathBuf>,
    /// Structured slow-event log path (if any).
    pub event_log_path: Option<std::path::PathBuf>,
    // Per-phase timing accumulators (reset each snapshot window).
    pub phase_select: Vec<Duration>,
    pub phase_reset: Vec<Duration>,
    pub phase_action: Vec<Duration>,
    pub phase_render: Vec<Duration>,
    pub phase_tick: Vec<Duration>,
    /// Timestamp of the last tick end (for measuring select overhead).
    pub last_tick_end: Instant,
    /// Duration of the most recent bench_reset_state call (set by bench_tick).
    pub last_reset_dur: Duration,
    /// Actions executed since last snapshot (for actions-per-frame calc).
    pub actions_since_snapshot: u64,
    /// Frames rendered since last snapshot.
    pub frames_since_snapshot: u64,
    // Render sub-phase accumulators (cleared each snapshot window).
    pub render_setup: Vec<Duration>,
    pub render_compositor: Vec<Duration>,
    pub render_flush: Vec<Duration>,
}

impl BenchState {
    pub fn new(seed: u64, duration: Duration) -> Self {
        let now = Instant::now();

        // Create log files in helix log directory.
        let log_dir = helix_loader::log_file()
            .parent()
            .map(std::path::Path::to_path_buf);
        let log_path = if cfg!(feature = "bench") {
            log_dir.as_ref().map(|dir| dir.join("bench.log"))
        } else {
            None
        };
        let event_log_path = if cfg!(feature = "bench") {
            log_dir.as_ref().map(|dir| dir.join("bench-events.log"))
        } else {
            None
        };

        let state = Self {
            seed,
            rng_state: seed.wrapping_add(0x9E3779B97F4A7C15),
            deadline: now + duration,
            duration,
            frame_times: Vec::with_capacity(4096),
            action_times: Vec::with_capacity(4096),
            rolling_fps: 0.0,
            last_fps_update: now,
            frames_since_update: 0,
            actions_executed: 0,
            snapshots: Vec::new(),
            last_snapshot: now,
            log_path,
            event_log_path,
            phase_select: Vec::with_capacity(256),
            phase_reset: Vec::with_capacity(256),
            phase_action: Vec::with_capacity(256),
            phase_render: Vec::with_capacity(256),
            phase_tick: Vec::with_capacity(256),
            last_tick_end: now,
            last_reset_dur: Duration::ZERO,
            actions_since_snapshot: 0,
            frames_since_snapshot: 0,
            render_setup: Vec::with_capacity(256),
            render_compositor: Vec::with_capacity(256),
            render_flush: Vec::with_capacity(256),
        };

        state.log_run_start();
        state
    }

    /// Simple xorshift64 PRNG (no external deps needed).
    pub fn next_rand(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }

    /// Random u32 in `[0, max)`.
    pub fn rand_range(&mut self, max: u32) -> u32 {
        (self.next_rand() % max as u64) as u32
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Record a frame and update rolling FPS.
    pub fn record_frame(&mut self, dur: Duration) {
        self.frame_times.push(dur);
        self.frames_since_update += 1;
        self.frames_since_snapshot += 1;

        let elapsed = self.last_fps_update.elapsed();
        if elapsed >= Duration::from_millis(500) {
            self.rolling_fps = self.frames_since_update as f64 / elapsed.as_secs_f64();
            self.frames_since_update = 0;
            self.last_fps_update = Instant::now();
        }
    }

    pub fn record_action(&mut self, category: &'static str, dur: Duration) {
        self.action_times.push((category, dur));
        self.actions_executed += 1;
        self.actions_since_snapshot += 1;
    }

    /// Record per-phase timing for a single bench tick.
    pub fn record_phases(
        &mut self,
        select: Duration,
        reset: Duration,
        action: Duration,
        render: Duration,
        tick: Duration,
    ) {
        self.phase_select.push(select);
        self.phase_reset.push(reset);
        self.phase_action.push(action);
        self.phase_render.push(render);
        self.phase_tick.push(tick);
    }

    /// Take a periodic snapshot (every 2s) and append to log file.
    /// Called from the event loop with current buffer stats.
    pub fn maybe_snapshot(&mut self, buf_lines: usize, buf_bytes: usize) {
        let since_last = self.last_snapshot.elapsed();
        if since_last < Duration::from_secs(2) {
            return;
        }
        self.last_snapshot = Instant::now();

        // Compute percentiles over the last window of frames
        let window_start = self.frame_times.len().saturating_sub(200);
        let mut window: Vec<_> = self.frame_times[window_start..].to_vec();
        if window.is_empty() {
            return;
        }
        window.sort();
        let n = window.len();
        let p50 = window[n * 50 / 100].as_micros() as u64;
        let p95 = window[n * 95 / 100].as_micros() as u64;
        let p99 = window[n.saturating_sub(1) * 99 / 100].as_micros() as u64;
        let max = window[n - 1].as_micros() as u64;

        // Compute per-phase averages from accumulated data
        fn avg_us(v: &[Duration]) -> u64 {
            if v.is_empty() {
                return 0;
            }
            let total: Duration = v.iter().sum();
            (total.as_micros() / v.len() as u128) as u64
        }
        let avg_select_us = avg_us(&self.phase_select);
        let avg_reset_us = avg_us(&self.phase_reset);
        let avg_action_us = avg_us(&self.phase_action);
        let avg_render_us = avg_us(&self.phase_render);
        let avg_tick_us = avg_us(&self.phase_tick);
        let avg_r_setup_us = avg_us(&self.render_setup);
        let avg_r_comp_us = avg_us(&self.render_compositor);
        let avg_r_flush_us = avg_us(&self.render_flush);

        let avg_actions_per_frame = if self.frames_since_snapshot > 0 {
            self.actions_since_snapshot as f64 / self.frames_since_snapshot as f64
        } else {
            0.0
        };

        // Clear phase accumulators for next window
        self.phase_select.clear();
        self.phase_reset.clear();
        self.phase_action.clear();
        self.phase_render.clear();
        self.phase_tick.clear();
        self.render_setup.clear();
        self.render_compositor.clear();
        self.render_flush.clear();
        self.actions_since_snapshot = 0;
        self.frames_since_snapshot = 0;

        let snap = BenchSnapshot {
            elapsed_secs: self.elapsed().as_secs_f64(),
            actions: self.actions_executed,
            fps: self.rolling_fps,
            p50_us: p50,
            p95_us: p95,
            p99_us: p99,
            max_us: max,
            buf_lines,
            buf_bytes,
            avg_select_us,
            avg_reset_us,
            avg_action_us,
            avg_render_us,
            avg_tick_us,
            avg_actions_per_frame,
        };

        // Write to log file
        if let Some(ref path) = self.log_path {
            use std::io::Write;
            let header = self.snapshots.is_empty();
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                if header {
                    let _ = writeln!(
                        f,
                        "elapsed_s,actions,fps,p50_us,p95_us,p99_us,max_us,buf_lines,buf_bytes,select_us,reset_us,action_us,render_us,tick_us,apf,r_setup,r_comp,r_flush"
                    );
                }
                let _ = writeln!(
                    f,
                    "{:.1},{},{:.1},{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{}",
                    snap.elapsed_secs,
                    snap.actions,
                    snap.fps,
                    snap.p50_us,
                    snap.p95_us,
                    snap.p99_us,
                    snap.max_us,
                    snap.buf_lines,
                    snap.buf_bytes,
                    snap.avg_select_us,
                    snap.avg_reset_us,
                    snap.avg_action_us,
                    snap.avg_render_us,
                    snap.avg_tick_us,
                    snap.avg_actions_per_frame,
                    avg_r_setup_us,
                    avg_r_comp_us,
                    avg_r_flush_us,
                );
            }
        }

        self.snapshots.push(snap);
    }

    /// Elapsed time since bench started.
    pub fn elapsed(&self) -> Duration {
        self.duration
            .saturating_sub(self.deadline.saturating_duration_since(Instant::now()))
    }

    /// Generate the final report string.
    pub fn report(&mut self) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        if self.frame_times.is_empty() {
            return "No frames recorded.".to_string();
        }

        self.frame_times.sort();
        let count = self.frame_times.len();
        let total: Duration = self.frame_times.iter().sum();
        let avg = total / count as u32;
        let p50 = self.frame_times[count * 50 / 100];
        let p95 = self.frame_times[count * 95 / 100];
        let p99 = self.frame_times[count.saturating_sub(1) * 99 / 100];
        let min = self.frame_times[0];
        let max = self.frame_times[count - 1];
        let fps = if avg.as_secs_f64() > 0.0 {
            1.0 / avg.as_secs_f64()
        } else {
            f64::INFINITY
        };

        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "═══════════════════════════════════════════════════════════"
        );
        let _ = writeln!(out, "  HELIX BENCH REPORT  (seed: {})", self.seed);
        let _ = writeln!(
            out,
            "═══════════════════════════════════════════════════════════"
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "  Frames rendered:  {count}");
        let _ = writeln!(out, "  Actions executed: {}", self.actions_executed);
        let _ = writeln!(out, "  Total time:       {:.2?}", self.duration);
        let _ = writeln!(out, "  Avg frame time:   {avg:.2?}");
        let _ = writeln!(out, "  Effective FPS:    {fps:.1}");
        let _ = writeln!(out);
        let _ = writeln!(out, "  Frame time distribution:");
        let _ = writeln!(out, "    min:  {min:.2?}");
        let _ = writeln!(out, "    p50:  {p50:.2?}");
        let _ = writeln!(out, "    p95:  {p95:.2?}");
        let _ = writeln!(out, "    p99:  {p99:.2?}");
        let _ = writeln!(out, "    max:  {max:.2?}");

        // Per-category breakdown
        let mut categories: HashMap<&str, Vec<Duration>> = HashMap::new();
        for (cat, dur) in &self.action_times {
            categories.entry(cat).or_default().push(*dur);
        }
        let mut cats: Vec<_> = categories.into_iter().collect();
        cats.sort_by_key(|(name, _)| *name);

        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  {:<20} {:>6} {:>10} {:>10} {:>10}",
            "category", "count", "avg", "p95", "max"
        );
        let _ = writeln!(out, "  {}", "─".repeat(58));

        for (name, mut times) in cats {
            times.sort();
            let n = times.len();
            let avg: Duration = times.iter().sum::<Duration>() / n as u32;
            let p95 = times[n * 95 / 100];
            let max = times[n - 1];
            let _ = writeln!(
                out,
                "  {name:<20} {n:>6} {avg:>10.2?} {p95:>10.2?} {max:>10.2?}"
            );
        }

        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "═══════════════════════════════════════════════════════════"
        );
        out
    }

    pub fn log_slow_tick(&self, trace: &BenchTickTrace) {
        const SLOW_ACTION_US: u64 = 25_000;
        const SLOW_RESET_US: u64 = 10_000;

        let action_slow = trace.action_us >= SLOW_ACTION_US;
        let reset_slow = trace.reset_us >= SLOW_RESET_US;
        let reset_unusual = trace.reset.undo_steps > 0
            || trace.reset.layers_popped > 0
            || trace.reset.escapes_sent > 0
            || trace.reset.reopened_scratch;

        if !(action_slow || reset_slow || reset_unusual) {
            return;
        }

        let Some(path) = &self.event_log_path else {
            return;
        };

        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let macro_name = if trace.macro_str.is_empty() {
                "<insert>"
            } else {
                trace.macro_str
            };
            let _ = writeln!(
                f,
                concat!(
                    "tick",
                    " seed={}",
                    " elapsed_s={:.3}",
                    " action_index={}",
                    " category={:?}",
                    " macro={:?}",
                    " force_insert={}",
                    " action_us={}",
                    " reset_us={}",
                    " pre_lines={}",
                    " pre_bytes={}",
                    " post_reset_lines={}",
                    " post_reset_bytes={}",
                    " post_action_lines={}",
                    " post_action_bytes={}",
                    " layers_popped={}",
                    " escapes_sent={}",
                    " undo_steps={}",
                    " reopened_scratch={}",
                    " action_slow={}",
                    " reset_slow={}"
                ),
                self.seed,
                trace.elapsed_secs,
                trace.action_index,
                trace.category,
                macro_name,
                trace.force_insert,
                trace.action_us,
                trace.reset_us,
                trace.reset.before_lines,
                trace.reset.before_bytes,
                trace.reset.after_lines,
                trace.reset.after_bytes,
                trace.post_action_lines,
                trace.post_action_bytes,
                trace.reset.layers_popped,
                trace.reset.escapes_sent,
                trace.reset.undo_steps,
                trace.reset.reopened_scratch,
                action_slow,
                reset_slow,
            );
        }
    }

    fn log_run_start(&self) {
        if !cfg!(feature = "bench") {
            return;
        }

        let Some(path) = &self.event_log_path else {
            return;
        };

        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|dur| dur.as_millis())
            .unwrap_or_default();

        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                f,
                "run_start seed={} duration_s={} started_unix_ms={}",
                self.seed,
                self.duration.as_secs(),
                started_ms
            );
        }
    }
}

pub struct BenchAction {
    pub weight: u32,
    pub category: &'static str,
    pub macros: &'static [&'static str],
}

// Code snippets for direct-transaction insertion (bypasses compositor).
// These are realistic Rust-like code lines to keep the buffer populated.
pub const BENCH_INSERT_SNIPPETS: &[&str] = &[
    "    let value = compute_result(input, &config);\n",
    "    if count > threshold {\n        total += delta;\n    }\n",
    "    for item in collection.iter() {\n        process(item);\n    }\n",
    "    match status {\n        Status::Active => handle_active(),\n        Status::Idle => handle_idle(),\n        _ => {}\n    }\n",
    "    let mut buffer = Vec::with_capacity(capacity);\n",
    "    // TODO: optimize this hot path\n    let result = expensive_computation(&data);\n",
    "    fn helper(x: usize, y: usize) -> usize {\n        x.saturating_add(y)\n    }\n",
    "    struct Entry {\n        key: String,\n        value: i64,\n        timestamp: u64,\n    }\n",
    "    impl Display for Error {\n        fn fmt(&self, f: &mut Formatter) -> fmt::Result {\n            write!(f, \"{}: {}\", self.kind, self.message)\n        }\n    }\n",
    "    pub fn new(name: &str, capacity: usize) -> Self {\n        Self {\n            name: name.to_owned(),\n            items: Vec::with_capacity(capacity),\n        }\n    }\n",
    "    while let Some(event) = receiver.recv().await {\n        dispatcher.handle(event);\n    }\n",
    "    assert_eq!(expected, actual, \"mismatch at index {i}\");\n",
    "    #[derive(Debug, Clone, PartialEq)]\n    enum Token {\n        Ident(String),\n        Number(f64),\n        Operator(char),\n    }\n",
    "    use std::collections::HashMap;\n    let mut map: HashMap<&str, Vec<usize>> = HashMap::new();\n",
    "    let output = input\n        .lines()\n        .filter(|l| !l.is_empty())\n        .map(|l| l.trim())\n        .collect::<Vec<_>>();\n",
];

// Actions are designed to be self-contained: each one starts and ends in
// normal mode without relying on <esc> working through compositor overlays.
// No multi-key insert-mode sequences or prompt-based commands.
// "insert" category is handled specially via direct Transaction, not macros.
pub const BENCH_ACTIONS: &[BenchAction] = &[
    // Normal movement (25%) — pure cursor motion, always safe
    BenchAction {
        weight: 25,
        category: "movement",
        macros: &[
            "j", "j", "k", "h", "l", // basic (extra j to bias downward)
            "w", "b", "e", // word
            "W", "B", "E", // WORD
            "0", "$", // line boundaries
            "gg", "ge", // file start/end
            "}", "{", // paragraph
            "%", // matching bracket
        ],
    },
    // Selection (10%) — enter+exit select in same macro
    BenchAction {
        weight: 10,
        category: "selection",
        macros: &[
            "x",  // select whole line
            ";",  // collapse selection
            ",",  // keep primary
            "xx", // select 2 lines
        ],
    },
    // Goto/scroll (10%) — viewport changes, always safe
    BenchAction {
        weight: 10,
        category: "goto",
        macros: &[
            "10gg", "50gg", "100gg", "500gg", "1000gg", "<C-d>", "<C-u>", // half page
            "<C-f>", "<C-b>", // full page
            "zz", "zt", "zb", // align viewport
        ],
    },
    // Mutations (10%) — buffer changes that don't enter insert mode
    // Lower weight to avoid destroying buffer content too fast
    BenchAction {
        weight: 10,
        category: "mutation",
        macros: &[
            "~",  // swap case
            "u",  // undo
            "u",  // extra undo weight
            "U",  // redo
            ">",  // indent
            "<",  // dedent
            "xJ", // join lines
        ],
    },
    // Delete (5%) — destructive, kept low to avoid emptying buffer
    BenchAction {
        weight: 5,
        category: "delete",
        macros: &[
            "xd", // delete line
            "d",  // delete selection
            "wd", // delete word
        ],
    },
    // Yank/paste (10%) — clipboard + paste
    BenchAction {
        weight: 10,
        category: "yank_paste",
        macros: &[
            "xy",  // yank line
            "wy",  // yank word
            "p",   // paste after
            "P",   // paste before
            "xyp", // yank + paste (duplicate line)
        ],
    },
    // Search (5%) — uses `n`/`N` which work without a prompt
    BenchAction {
        weight: 5,
        category: "search",
        macros: &[
            "n", "n", "N", "N",  // next/prev match (safe even with no prior search)
            "*n", // search word under cursor + next
        ],
    },
    // Counted motions (8%) — exercises the count system
    BenchAction {
        weight: 8,
        category: "counted",
        macros: &["3j", "3k", "5w", "5b", "10j", "10k", "2x", "3e"],
    },
    // Window (2%) — splits (but balanced: open + close)
    BenchAction {
        weight: 2,
        category: "window",
        macros: &[
            "<C-w>v<C-w>q", // vsplit then close it (net zero)
            "<C-w>s<C-w>q", // hsplit then close it (net zero)
            "<C-w>h",
            "<C-w>l", // focus left/right
            "<C-w>j",
            "<C-w>k", // focus down/up
        ],
    },
];

// Weight for the "insert" pseudo-category (handled via Transaction, not macros).
pub const BENCH_INSERT_WEIGHT: u32 = 15;

/// Returns (category, macro_str). If category is "insert", macro_str is empty
/// and the caller should use direct Transaction insertion instead.
pub fn bench_pick_action(bench: &mut BenchState) -> (&'static str, &'static str) {
    let total_weight: u32 = BENCH_ACTIONS.iter().map(|a| a.weight).sum();
    let mut roll = bench.rand_range(total_weight + BENCH_INSERT_WEIGHT);

    // Check insert pseudo-category first
    if roll < BENCH_INSERT_WEIGHT {
        return ("insert", "");
    }
    roll -= BENCH_INSERT_WEIGHT;

    for action in BENCH_ACTIONS {
        if roll < action.weight {
            let idx = bench.rand_range(action.macros.len() as u32) as usize;
            return (action.category, action.macros[idx]);
        }
        roll -= action.weight;
    }

    ("movement", "j")
}

pub fn generate_bench_content(seed: u64, num_lines: usize) -> String {
    let keywords = [
        "fn", "let", "mut", "pub", "struct", "impl", "if", "for", "match", "use",
    ];
    let types = [
        "String",
        "Vec<T>",
        "Option<T>",
        "Result<T, E>",
        "usize",
        "bool",
        "&str",
    ];
    let idents = [
        "foo",
        "bar",
        "widget",
        "handler",
        "config",
        "editor",
        "buffer",
        "render",
        "process",
        "update",
        "dispatch",
        "execute",
        "transform",
    ];

    let mut buf = String::with_capacity(num_lines * 60);
    let mut indent = 0u32;
    let mut rng = seed.wrapping_add(0x9E3779B97F4A7C15);

    let mut next = || -> u64 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for _ in 0..num_lines {
        let kind = (next() % 20) as u8;
        let indent_str = "    ".repeat(indent as usize);

        match kind {
            0..=2 => {
                let name = idents[(next() % idents.len() as u64) as usize];
                let ret = types[(next() % types.len() as u64) as usize];
                buf.push_str(&format!(
                    "{indent_str}fn {name}_{kind}(&mut self) -> {ret} {{\n"
                ));
                indent = (indent + 1).min(5);
            }
            3..=5 => {
                let name = idents[(next() % idents.len() as u64) as usize];
                let kw = keywords[(next() % keywords.len() as u64) as usize];
                buf.push_str(&format!(
                    "{indent_str}let {kw}_{name} = Default::default();\n"
                ));
            }
            6..=7 => {
                let cond = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}if {cond}.is_some() {{\n"));
                indent = (indent + 1).min(5);
            }
            8..=9 => {
                indent = indent.saturating_sub(1);
                let indent_str = "    ".repeat(indent as usize);
                buf.push_str(&format!("{indent_str}}}\n"));
            }
            10..=11 => {
                let word = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}// TODO: refactor {word}\n"));
            }
            12..=13 => {
                let a = idents[(next() % idents.len() as u64) as usize];
                let b = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!(
                    "{indent_str}self.{a}.{b}().unwrap_or_default();\n"
                ));
            }
            14 => {
                let name = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}pub struct {name}State {{\n"));
                indent = (indent + 1).min(5);
            }
            15 => {
                let name = idents[(next() % idents.len() as u64) as usize];
                let ty = types[(next() % types.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}pub {name}: {ty},\n"));
            }
            16 => {
                let name = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}impl {name}Handler {{\n"));
                indent = (indent + 1).min(5);
            }
            17 => {
                let it = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}for item in {it}.iter() {{\n"));
                indent = (indent + 1).min(5);
            }
            18 => buf.push('\n'),
            _ => {
                // match arm with realistic line lengths
                let a = idents[(next() % idents.len() as u64) as usize];
                let b = idents[(next() % idents.len() as u64) as usize];
                buf.push_str(&format!("{indent_str}let {a} = self.{b}.clone();\n"));
            }
        }
    }
    while indent > 0 {
        indent -= 1;
        buf.push_str(&format!("{}}}\n", "    ".repeat(indent as usize)));
    }
    buf
}

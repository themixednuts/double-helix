use futures_util::{Stream, StreamExt};
use helix_view::bench::{
    enter_bench_command, enter_bench_run, log_command_phase, BenchResetStats, BENCH_INSERT_SNIPPETS,
};

use super::{Application, TerminalEvent};

impl Application {
    /// Render and return the frame duration. Used by benchmarks.
    #[cfg(feature = "integration")]
    pub async fn render_timed(&mut self) -> std::time::Duration {
        let start = std::time::Instant::now();
        self.render().await;
        start.elapsed()
    }

    /// Drive one benchmark action: pick a random action, feed its keys, render, record timing.
    /// Returns `true` if the bench is still running, `false` if it finished.
    fn bench_tick(&mut self) -> bool {
        use helix_view::input::Event as ViewEvent;

        if !self.editor.has_active_bench() {
            return false;
        }
        if let Some(report) = self.editor.finish_expired_bench() {
            eprintln!("{report}");
            self.editor
                .set_status("Bench complete. Report printed to stderr.");
            return false;
        }

        let reset_start = std::time::Instant::now();
        let reset_stats = self.bench_reset_state();
        let reset_dur = reset_start.elapsed();

        let force_insert = reset_stats.after_lines < 100;

        let (category, macro_str) = if force_insert {
            ("insert", "")
        } else {
            self.editor.pick_bench_action().unwrap_or(("insert", ""))
        };

        let action_start = std::time::Instant::now();
        let _bench_command_guard = self
            .editor
            .bench_command_context(category, macro_str, force_insert)
            .and_then(enter_bench_command);

        if category == "insert" {
            self.bench_insert_text();
        } else {
            let keys = match helix_view::input::parse_macro(macro_str) {
                Ok(k) => k,
                Err(_) => return true,
            };

            let ingress = self.ingress().tx.clone();
            let idle_reset = self.ingress().idle_reset_tx.clone();
            let notifier = crate::handlers::local::Notifier {
                ingress: ingress.clone(),
                plugin_events: self.ingress().plugin_event_tx.clone(),
            };
            let mut cx = Self::make_compositor_context(
                &mut self.editor,
                &mut self.exit.tasks,
                self.exit.work.clone(),
                notifier,
                ingress,
                idle_reset,
                self.plugin_manager.clone(),
            );

            for key in &keys {
                self.compositor.handle_event(&ViewEvent::Key(*key), &mut cx);
            }
        }

        let action_dur = action_start.elapsed();
        let (post_action_lines, post_action_bytes) = self.bench_buffer_stats();

        self.editor
            .update_bench_action(helix_view::editor::BenchActionUpdate {
                category,
                action_dur,
                reset_dur,
                reset: reset_stats,
                post_action_lines,
                post_action_bytes,
                force_insert,
                macro_str,
            });

        self.editor.request_redraw();
        true
    }

    fn bench_insert_text(&mut self) {
        let snippet_idx = self.editor.bench_snippet_index(BENCH_INSERT_SNIPPETS.len());
        let snippet = BENCH_INSERT_SNIPPETS[snippet_idx];
        let build_start = std::time::Instant::now();
        let Some((selection_count, before_lines, before_bytes, after_lines, after_bytes)) =
            self.editor.insert_into_focused_document(snippet)
        else {
            return;
        };
        let build_dur = build_start.elapsed();
        log_command_phase("bench_insert", "build_transaction", build_dur, || {
            format!(
                "snippet_idx={} snippet_bytes={} selections={} lines={} bytes={}",
                snippet_idx,
                snippet.len(),
                selection_count,
                before_lines,
                before_bytes
            )
        });
        let apply_start = std::time::Instant::now();
        let apply_dur = apply_start.elapsed();
        log_command_phase("bench_insert", "apply", apply_dur, || {
            format!(
                "snippet_idx={} snippet_bytes={} selections={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
                snippet_idx,
                snippet.len(),
                selection_count,
                before_lines,
                after_lines,
                before_bytes,
                after_bytes
            )
        });
    }

    fn bench_reset_state(&mut self) -> BenchResetStats {
        use helix_view::input::{Event as ViewEvent, KeyCode, KeyEvent as VKeyEvent, KeyModifiers};

        let (before_lines, before_bytes) = self.bench_buffer_stats();
        let mut stats = BenchResetStats {
            before_lines,
            before_bytes,
            ..BenchResetStats::default()
        };

        let esc = VKeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
        };

        while self.compositor.layer_count() > 1 {
            self.compositor.pop();
            stats.layers_popped += 1;
        }

        for _ in 0..5 {
            if self.editor.mode() == helix_view::document::Mode::Normal {
                break;
            }
            stats.escapes_sent += 1;
            let ingress = self.ingress().tx.clone();
            let idle_reset = self.ingress().idle_reset_tx.clone();
            let notifier = crate::handlers::local::Notifier {
                ingress: ingress.clone(),
                plugin_events: self.ingress().plugin_event_tx.clone(),
            };
            let mut cx = Self::make_compositor_context(
                &mut self.editor,
                &mut self.exit.tasks,
                self.exit.work.clone(),
                notifier,
                ingress,
                idle_reset,
                self.plugin_manager.clone(),
            );
            self.compositor.handle_event(&ViewEvent::Key(esc), &mut cx);
        }

        if self.editor.should_close() {
            let _ = self.editor.new_file(helix_view::editor::Action::Replace);
            stats.reopened_scratch = true;
        }

        stats.undo_steps += self.editor.undo_focused_document_to_line_limit(5_000, 50);

        let (after_lines, after_bytes) = self.bench_buffer_stats();
        stats.after_lines = after_lines;
        stats.after_bytes = after_bytes;
        stats
    }

    fn bench_buffer_stats(&self) -> (usize, usize) {
        self.editor.focused_buffer_stats()
    }

    /// Tight bench loop: batch actions within budget, render once per batch,
    /// poll for Ctrl+C periodically. Used by both `:bench` and `hx-bench`.
    pub async fn bench_run_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        let _bench_run_guard = self.editor.bench_run_context().map(enter_bench_run);

        let mut last_poll = std::time::Instant::now();

        while self.editor.has_active_bench() {
            const ACTION_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);

            let poll_dur = if last_poll.elapsed() >= std::time::Duration::from_millis(200) {
                let poll_start = std::time::Instant::now();
                if let Ok(Some(event)) =
                    tokio::time::timeout(std::time::Duration::ZERO, input_stream.next()).await
                {
                    self.handle_terminal_events(event).await;
                    if !self.editor.has_active_bench() {
                        break;
                    }
                }
                last_poll = std::time::Instant::now();
                poll_start.elapsed()
            } else {
                std::time::Duration::ZERO
            };

            let batch_start = std::time::Instant::now();
            let mut total_reset = std::time::Duration::ZERO;
            let mut bench_running = true;

            while batch_start.elapsed() < ACTION_BUDGET {
                if !self.bench_tick() {
                    bench_running = false;
                    break;
                }
                if let Some(last_reset) = self.editor.bench_last_reset_duration() {
                    total_reset += last_reset;
                }
            }

            let action_dur = batch_start.elapsed();

            if tokio::time::Instant::now() >= self.timers.idle.deadline() {
                self.service_idle_timeout(crate::runtime::IdleRender::Defer)
                    .await;
            }

            let render_start = std::time::Instant::now();
            self.render().await;
            let render_dur = render_start.elapsed();

            let tick_dur = action_dur + render_dur + poll_dur;

            let (buf_lines, buf_bytes) = self.bench_buffer_stats();
            self.editor
                .update_bench_frame(helix_view::editor::BenchFrameUpdate {
                    poll_dur,
                    total_reset,
                    action_dur,
                    render_dur,
                    tick_dur,
                    buf_lines,
                    buf_bytes,
                });

            if !bench_running {
                break;
            }
        }
    }
}

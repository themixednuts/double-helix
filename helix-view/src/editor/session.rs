use arc_swap::access::DynGuard;
use helix_core::completion::CompletionProvider;

use crate::DocumentId;

use super::{Config, Editor, WorkspaceDiagnosticCounts};

#[derive(Debug, Clone, Copy)]
pub struct BenchOverlay {
    pub rolling_fps: f64,
    pub actions_executed: usize,
    pub remaining_seconds: f64,
}

impl Editor {
    pub fn mode(&self) -> crate::document::Mode {
        self.mode
    }

    pub fn enter_insert_mode(&mut self) {
        self.mode = crate::document::Mode::Insert;
    }

    pub fn enter_select_mode(&mut self) {
        self.mode = crate::document::Mode::Select;
    }

    pub fn config(&self) -> DynGuard<Config> {
        self.config.load()
    }

    pub fn bump_config_generation(&mut self) {
        self.config_gen = self.config_gen.wrapping_add(1);
    }

    pub fn frontend(&self) -> &super::core::FrontendState {
        &self.frontend
    }

    pub fn frontend_mut(&mut self) -> &mut super::core::FrontendState {
        &mut self.frontend
    }

    pub fn is_redraw_pending(&self) -> bool {
        self.needs_redraw
    }

    pub fn clear_redraw_request(&mut self) {
        self.needs_redraw = false;
    }

    pub fn lifecycle(&self) -> std::sync::Arc<super::hooks::LifecycleBus> {
        self.lifecycle.clone()
    }

    pub fn refresh_modal_keymaps(
        &self,
        keymaps: std::collections::HashMap<crate::document::Mode, crate::keymap::ModalKeyTrie>,
    ) {
        let Some(modal_keymaps) = &self.frontend.modal_keymaps else {
            return;
        };
        modal_keymaps.store(std::sync::Arc::new(keymaps));
    }

    pub fn assistant_context_registry(&self) -> &crate::assistant::context::Registry {
        &self.assistant_services.context
    }

    pub fn dispatch_document_open(&mut self, doc: DocumentId, path: &std::path::PathBuf) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::DocumentDidOpen {
            editor: self,
            doc,
            path,
        };
        lifecycle.dispatch_document_open(&mut event);
    }

    pub fn dispatch_document_close(&mut self, doc: crate::Document) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::DocumentDidClose { editor: self, doc };
        lifecycle.dispatch_document_close(&mut event);
    }

    pub fn dispatch_document_focus_lost(&mut self, doc: DocumentId) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::DocumentFocusLost { editor: self, doc };
        lifecycle.dispatch_document_focus_lost(&mut event);
    }

    pub fn dispatch_diagnostics_change(&mut self, doc: DocumentId) {
        let lifecycle = self.lifecycle();
        let diagnostic_count = self
            .document(doc)
            .map(|document| document.diagnostics().len())
            .unwrap_or(0);
        let mut event = crate::events::DiagnosticsDidChange {
            editor: self,
            doc,
            diagnostic_count,
        };
        lifecycle.dispatch_diagnostics_change(&mut event);
    }

    pub fn dispatch_language_server_initialized(&mut self, server_id: helix_lsp::LanguageServerId) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::LanguageServerInitialized {
            editor: self,
            server_id,
        };
        lifecycle.dispatch_language_server_initialized(&mut event);
    }

    pub fn dispatch_language_server_exited(&mut self, server_id: helix_lsp::LanguageServerId) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::LanguageServerExited {
            editor: self,
            server_id,
        };
        lifecycle.dispatch_language_server_exited(&mut event);
    }

    pub fn dispatch_editor_config_change(&mut self, old_config: &Config) {
        let lifecycle = self.lifecycle();
        let mut event = crate::events::EditorConfigDidChange {
            old_config,
            editor: self,
        };
        lifecycle.dispatch_editor_config_change(&mut event);
    }

    pub fn has_active_bench(&self) -> bool {
        self.bench.is_some()
    }

    pub fn start_bench(
        &mut self,
        seed: u64,
        duration: std::time::Duration,
    ) -> Option<std::path::PathBuf> {
        let bench = crate::bench::BenchState::new(seed, duration);
        let log_path = bench.log_path.clone();
        self.bench = Some(bench);
        log_path
    }

    pub fn bench_overlay(&self) -> Option<BenchOverlay> {
        self.bench.as_ref().map(|bench| BenchOverlay {
            rolling_fps: bench.rolling_fps,
            actions_executed: bench.actions_executed as usize,
            remaining_seconds: bench.duration.saturating_sub(bench.elapsed()).as_secs_f64(),
        })
    }

    pub fn record_bench_render_phases(
        &mut self,
        setup: std::time::Duration,
        compositor: std::time::Duration,
        flush: std::time::Duration,
    ) {
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        bench.render_setup.push(setup);
        bench.render_compositor.push(compositor);
        bench.render_flush.push(flush);
    }

    pub fn bench_last_reset_duration(&self) -> Option<std::time::Duration> {
        self.bench.as_ref().map(|bench| bench.last_reset_dur)
    }

    pub fn bench_run_context(&self) -> Option<crate::bench::BenchRunContext> {
        let bench = self.bench.as_ref()?;
        Some(crate::bench::BenchRunContext {
            seed: bench.seed,
            event_log_path: bench.event_log_path.clone()?,
        })
    }

    pub fn bench_command_context(
        &self,
        category: &'static str,
        macro_str: &'static str,
        force_insert: bool,
    ) -> Option<crate::bench::BenchCommandContext> {
        let bench = self.bench.as_ref()?;
        Some(crate::bench::BenchCommandContext {
            seed: bench.seed,
            event_log_path: bench.event_log_path.clone(),
            action_index: bench.actions_executed + 1,
            elapsed_secs: bench.elapsed().as_secs_f64(),
            category,
            macro_str,
            force_insert,
        })
    }

    pub fn bench_snippet_index(&mut self, snippet_count: usize) -> usize {
        self.bench
            .as_mut()
            .map(|bench| bench.rand_range(snippet_count as u32) as usize)
            .unwrap_or(0)
    }

    pub fn pick_bench_action(&mut self) -> Option<(&'static str, &'static str)> {
        self.bench.as_mut().map(crate::bench::bench_pick_action)
    }

    pub fn update_bench_action(&mut self, update: super::BenchActionUpdate) {
        let super::BenchActionUpdate {
            category,
            action_dur,
            reset_dur,
            reset,
            post_action_lines,
            post_action_bytes,
            force_insert,
            macro_str,
        } = update;
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        bench.last_reset_dur = reset_dur;
        bench.record_action(category, action_dur);
        bench.log_slow_tick(&crate::bench::BenchTickTrace {
            action_index: bench.actions_executed,
            elapsed_secs: bench.elapsed().as_secs_f64(),
            category,
            macro_str,
            force_insert,
            reset,
            post_action_lines,
            post_action_bytes,
            reset_us: reset_dur.as_micros() as u64,
            action_us: action_dur.as_micros() as u64,
        });
    }

    pub fn update_bench_frame(&mut self, update: super::BenchFrameUpdate) {
        let super::BenchFrameUpdate {
            poll_dur,
            total_reset,
            action_dur,
            render_dur,
            tick_dur,
            buf_lines,
            buf_bytes,
        } = update;
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        bench.record_frame(render_dur);
        bench.record_phases(
            poll_dur,
            total_reset,
            action_dur.saturating_sub(total_reset),
            render_dur,
            tick_dur,
        );
        bench.last_tick_end = std::time::Instant::now();
        bench.maybe_snapshot(buf_lines, buf_bytes);
    }

    pub fn finish_expired_bench(&mut self) -> Option<String> {
        let bench = self.bench.as_mut()?;
        if !bench.is_expired() {
            return None;
        }
        let report = bench.report();
        self.bench = None;
        Some(report)
    }

    pub fn cancel_bench(&mut self) -> Option<String> {
        self.bench.take().map(|mut bench| bench.report())
    }

    pub fn begin_completion_request(
        &mut self,
    ) -> (crate::handlers::completion::RequestId, helix_runtime::Token) {
        self.handlers.completions.begin_request()
    }

    pub fn is_current_completion_request(
        &self,
        request: crate::handlers::completion::RequestId,
    ) -> bool {
        self.handlers.completions.is_current(request)
    }

    pub fn replace_completion_contexts(
        &mut self,
        contexts: std::collections::HashMap<
            CompletionProvider,
            crate::handlers::completion::ResponseContext,
        >,
    ) {
        self.handlers.completions.active_completions = contexts;
    }

    pub fn set_completion_context(
        &mut self,
        provider: CompletionProvider,
        context: crate::handlers::completion::ResponseContext,
    ) {
        self.handlers
            .completions
            .active_completions
            .insert(provider, context);
    }

    pub fn completion_context(
        &self,
        provider: CompletionProvider,
    ) -> Option<&crate::handlers::completion::ResponseContext> {
        self.handlers.completions.active_completions.get(&provider)
    }

    pub fn clear_completion_requests(&mut self) {
        self.handlers.completions.cancel_request();
        self.handlers.completions.active_completions.clear();
    }

    pub fn send_completion_event(&self, event: crate::handlers::completion::CompletionEvent) {
        self.handlers.completions.event(event);
    }

    pub fn active_completion_contexts(
        &self,
    ) -> impl Iterator<
        Item = (
            &helix_core::completion::CompletionProvider,
            &crate::handlers::completion::ResponseContext,
        ),
    > {
        self.handlers.completions.active_completions.iter()
    }

    pub fn signature_help_sender(
        &self,
    ) -> &helix_runtime::Sender<crate::handlers::lsp::SignatureHelpEvent> {
        &self.handlers.signature_hints
    }

    pub fn auto_save_sender(&self) -> &helix_runtime::Sender<crate::handlers::AutoSaveEvent> {
        &self.handlers.auto_save
    }

    pub fn auto_reload_sender(&self) -> &helix_runtime::Sender<crate::handlers::AutoReloadEvent> {
        &self.handlers.auto_reload
    }

    pub fn word_index(&self) -> &crate::handlers::word_index::WordIndex {
        self.handlers.word_index()
    }

    pub fn refresh_config(&mut self, old_config: &Config) {
        let config = self.config();
        self.auto_pairs = (&config.auto_pairs).into();
        self._refresh();
        let lifecycle = self.lifecycle();
        let mut event = crate::events::ConfigDidChange {
            editor: self,
            old: old_config,
            new: &config,
        };
        lifecycle.dispatch_config_change(&mut event)
    }

    pub fn refresh_workspace_diagnostic_counts(&mut self) {
        let mut counts = WorkspaceDiagnosticCounts::default();
        for (diagnostic, _) in self.diagnostics.values().flatten() {
            match diagnostic.severity {
                Some(helix_lsp::lsp::DiagnosticSeverity::WARNING) => counts.warnings += 1,
                Some(helix_lsp::lsp::DiagnosticSeverity::ERROR) => counts.errors += 1,
                Some(helix_lsp::lsp::DiagnosticSeverity::HINT) => counts.hints += 1,
                Some(helix_lsp::lsp::DiagnosticSeverity::INFORMATION) => counts.info += 1,
                _ => counts.hints += 1,
            }
        }
        self.workspace_diagnostic_counts = counts;
    }

    pub fn remove_language_server_diagnostics(
        &mut self,
        language_server_id: helix_lsp::LanguageServerId,
    ) {
        for diags in self.diagnostics.values_mut() {
            diags.retain(|(_, provider)| provider.language_server_id() != Some(language_server_id));
        }
        self.diagnostics.retain(|_, diags| !diags.is_empty());
        self.refresh_workspace_diagnostic_counts();
    }

    pub fn apply_motion<F: Fn(&mut Self) + Send + Sync + 'static>(&mut self, motion: F) {
        motion(self);
        self.last_motion = Some(Box::new(motion));
    }

    pub fn repeat_last_motion(&mut self, count: usize) {
        if let Some(motion) = self.last_motion.take() {
            for _ in 0..count {
                motion(self);
            }
            self.last_motion = Some(motion);
        }
    }
}

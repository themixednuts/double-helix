use std::mem::take;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use ropey::RopeSlice;

use crate::config::LanguageLoader;
use crate::trace::log_trace_phase;
use crate::{Error, Layer, LayerData, Locals, Syntax};

const INTERACTIVE_QUERY_TIMEOUT_MAX_MS: u128 = 20;
const DEFER_ROOT_QUERIES_INJECTION_THRESHOLD: usize = 64;
const DEFER_ROOT_QUERIES_SOURCE_BYTES: usize = 48 * 1024;
const DEFER_ROOT_QUERIES_ENQUEUED_LAYERS_THRESHOLD: usize = 24;
const INTERACTIVE_UPDATE_VISITED_LAYERS_THRESHOLD: usize = 48;
const INTERACTIVE_UPDATE_FRESH_LAYERS_THRESHOLD: usize = 16;

#[derive(Clone, Copy, Debug)]
struct ParseCallStats {
    reused_old_tree: bool,
    had_tree_before: bool,
    tree_range_start: u32,
    tree_range_end: u32,
    included_range_start: u32,
    included_range_end: u32,
    included_range_bytes: u32,
}

#[derive(Clone, Copy, Debug)]
struct InteractiveUpdateStats {
    update_start: Instant,
    layers_before: usize,
    incomplete_before: usize,
    visited_layers: usize,
    skipped_empty_layers: usize,
    reset_incomplete_layers: usize,
    fresh_parse_layers: usize,
    reparsed_layers: usize,
    parse_timeout_layers: usize,
}

#[derive(Clone, Copy)]
struct UpdateRequest<'a> {
    source: RopeSlice<'a>,
    timeout: Duration,
    edits: &'a [tree_sitter::InputEdit],
}

fn summarize_edits(edits: &[tree_sitter::InputEdit]) -> Option<(u32, u32, u32, u32, i64)> {
    let first = edits.first()?;
    let mut min_start = first.start_byte;
    let mut max_old_end = first.old_end_byte;
    let mut max_new_end = first.new_end_byte;
    let mut total_old_bytes: u32 = 0;
    let mut total_new_bytes: u32 = 0;

    for edit in edits {
        min_start = min_start.min(edit.start_byte);
        max_old_end = max_old_end.max(edit.old_end_byte);
        max_new_end = max_new_end.max(edit.new_end_byte);
        total_old_bytes = total_old_bytes.saturating_add(edit.old_end_byte - edit.start_byte);
        total_new_bytes = total_new_bytes.saturating_add(edit.new_end_byte - edit.start_byte);
    }

    Some((
        min_start,
        max_old_end,
        max_new_end,
        total_old_bytes,
        i64::from(total_new_bytes) - i64::from(total_old_bytes),
    ))
}

impl Syntax {
    fn defer_interactive_update(
        &mut self,
        request: UpdateRequest<'_>,
        stats: InteractiveUpdateStats,
        reason: &str,
        layer_idx: usize,
        queue_len: usize,
    ) -> Result<(), Error> {
        {
            let root_data = self.layer_mut(self.root);
            root_data.query_stale = true;
            root_data.locals = Locals::default();
        }
        self.preserve_query_stale_subtree(self.root);

        let prune_start = Instant::now();
        let layers_before_prune = self.layers.len();
        self.prune_dead_layers();
        let prune_elapsed = prune_start.elapsed();
        let layers_after = self.layers.len();
        let incomplete_after = self
            .layers
            .iter()
            .filter(|(_, layer)| layer.parse_incomplete)
            .count();

        log_trace_phase("tree_house", "prune_dead_layers", prune_elapsed, || {
            format!(
                "layers_before_prune={} layers_after={} pruned_layers={}",
                layers_before_prune,
                layers_after,
                layers_before_prune.saturating_sub(layers_after)
            )
        });
        log_trace_phase("tree_house", "defer_interactive_update", stats.update_start.elapsed(), || {
            format!(
                "reason={} layer={} source_bytes={} edits={} timeout_ms={} layers_before={} layers_after={} visited_layers={} queue_len={} skipped_empty_layers={} reset_incomplete_layers={} fresh_parse_layers={} reparsed_layers={} parse_timeout_layers={} incomplete_before={} incomplete_after={}",
                reason,
                layer_idx,
                request.source.len_bytes(),
                request.edits.len(),
                request.timeout.as_millis(),
                stats.layers_before,
                layers_after,
                stats.visited_layers,
                queue_len,
                stats.skipped_empty_layers,
                stats.reset_incomplete_layers,
                stats.fresh_parse_layers,
                stats.reparsed_layers,
                stats.parse_timeout_layers,
                stats.incomplete_before,
                incomplete_after
            )
        });
        log_trace_phase("tree_house", "update_summary", stats.update_start.elapsed(), || {
            format!(
                "result=query_deferred_interactive_budget source_bytes={} edits={} timeout_ms={} layers_before={} layers_after={} visited_layers={} skipped_empty_layers={} reset_incomplete_layers={} fresh_parse_layers={} reparsed_layers={} parse_timeout_layers={} incomplete_before={} incomplete_after={}",
                request.source.len_bytes(),
                request.edits.len(),
                request.timeout.as_millis(),
                stats.layers_before,
                layers_after,
                stats.visited_layers,
                stats.skipped_empty_layers,
                stats.reset_incomplete_layers,
                stats.fresh_parse_layers,
                stats.reparsed_layers,
                stats.parse_timeout_layers,
                stats.incomplete_before,
                incomplete_after
            )
        });

        Err(Error::Timeout)
    }

    fn preserve_query_stale_subtree(&mut self, root: Layer) {
        let mut seen = HashSet::new();
        let mut stack = vec![root];

        while let Some(layer) = stack.pop() {
            if !seen.insert(layer.idx()) || !self.has_layer(layer) {
                continue;
            }

            let children = {
                let layer_data = self.layer_mut(layer);
                layer_data.flags.touched = true;
                layer_data
                    .injections
                    .iter()
                    .map(|injection| injection.layer)
                    .collect::<Vec<_>>()
            };

            stack.extend(children);
        }
    }

    pub fn update(
        &mut self,
        source: RopeSlice,
        timeout: Duration,
        edits: &[tree_sitter::InputEdit],
        loader: &impl LanguageLoader,
    ) -> Result<(), Error> {
        // size limit of 512MiB, TS just cannot handle files this big (too
        // slow). Furthermore, TS uses 32 (signed) bit indices so this limit
        // must never be raised above 2GiB
        if source.len_bytes() >= 512 * 1024 * 1024 {
            return Err(Error::ExceededMaximumSize);
        }

        let update_start = Instant::now();
        let request = UpdateRequest {
            source,
            timeout,
            edits,
        };
        let layers_before = self.layers.len();
        let incomplete_before = self
            .layers
            .iter()
            .filter(|(_, layer)| layer.parse_incomplete)
            .count();
        let mut queue = Vec::with_capacity(32);
        let root_flags = &mut self.layer_mut(self.root).flags;
        // The root layer is always considered.
        root_flags.touched = true;
        // If there was an edit then the root layer must've been modified.
        root_flags.modified = true;
        queue.push(self.root);

        let has_edits = !edits.is_empty();
        let interactive_update = has_edits && timeout.as_millis() <= INTERACTIVE_QUERY_TIMEOUT_MAX_MS;
        let mut visited_layers = 0usize;
        let mut skipped_empty_layers = 0usize;
        let mut reset_incomplete_layers = 0usize;
        let mut fresh_parse_layers = 0usize;
        let mut reparsed_layers = 0usize;
        let mut parse_timeout_layers = 0usize;

        if let Some((min_start, max_old_end, max_new_end, total_old_bytes, net_new_bytes)) =
            summarize_edits(edits)
        {
            log_trace_phase("tree_house", "edit_summary", Duration::from_micros(2_000), || {
                format!(
                    "source_bytes={} edits={} min_start={} max_old_end={} max_new_end={} total_old_bytes={} net_new_bytes={} timeout_ms={}",
                    source.len_bytes(),
                    edits.len(),
                    min_start,
                    max_old_end,
                    max_new_end,
                    total_old_bytes,
                    net_new_bytes,
                    timeout.as_millis()
                )
            });
        }

        while let Some(layer) = queue.pop() {
            visited_layers += 1;
            let layer_idx = layer.idx();
            let is_root = layer == self.root;
            let interactive_root = is_root && interactive_update;

            let (
                ranges_len,
                had_tree,
                defer_root_queries,
                parse_incomplete_before,
                flags_modified,
                flags_moved,
                flags_reused,
                injections_before,
                parse_elapsed,
                parse_outcome,
            ) = {
                let layer_data = self.layer_mut(layer);
                if layer_data.ranges.is_empty() {
                    skipped_empty_layers += 1;
                    continue;
                }

                let ranges_len = layer_data.ranges.len();
                let had_tree = layer_data.parse_tree.is_some();
                let parse_incomplete_before = layer_data.parse_incomplete;
                let flags_modified = layer_data.flags.modified;
                let flags_moved = layer_data.flags.moved;
                let flags_reused = layer_data.flags.reused;
                let injections_before = layer_data.injections.len();
                let defer_root_queries = interactive_root
                    && (injections_before >= DEFER_ROOT_QUERIES_INJECTION_THRESHOLD
                        || source.len_bytes() >= DEFER_ROOT_QUERIES_SOURCE_BYTES);

                if defer_root_queries {
                    (
                        ranges_len,
                        had_tree,
                        defer_root_queries,
                        parse_incomplete_before,
                        flags_modified,
                        flags_moved,
                        flags_reused,
                        injections_before,
                        Duration::ZERO,
                        "deferred_pre_parse",
                    )
                } else if interactive_root && has_edits && parse_incomplete_before {
                    if let Some(tree) = &mut layer_data.parse_tree {
                        if layer_data.flags.moved || layer_data.flags.modified {
                            for edit in edits.iter().rev() {
                                tree.edit(edit);
                            }
                        }
                    }
                    layer_data.parser = tree_sitter::Parser::new();

                    log_trace_phase(
                        "tree_house",
                        "skip_incomplete_root_restart",
                        Duration::ZERO,
                        || {
                            format!(
                                "layer={} root=true source_bytes={} edits={} timeout_ms={} injections_before={} parse_incomplete_before=true",
                                layer_idx,
                                source.len_bytes(),
                                edits.len(),
                                timeout.as_millis(),
                                injections_before,
                            )
                        },
                    );

                    (
                        ranges_len,
                        had_tree,
                        defer_root_queries,
                        parse_incomplete_before,
                        flags_modified,
                        flags_moved,
                        flags_reused,
                        injections_before,
                        Duration::ZERO,
                        "deferred_incomplete",
                    )
                } else {

                    if has_edits && layer_data.parse_incomplete {
                        reset_incomplete_layers += 1;
                        layer_data.parser = tree_sitter::Parser::new();
                        layer_data.parse_incomplete = false;
                    }
                    layer_data.parser.set_timeout(timeout);

                    let parse_start = Instant::now();
                    let parse_result = if let Some(tree) = &mut layer_data.parse_tree {
                        if layer_data.flags.moved || layer_data.flags.modified {
                            for edit in edits.iter().rev() {
                                // Apply the edits in reverse.
                                // If we applied them in order then edit 1 would disrupt the positioning
                                // of edit 2.
                                tree.edit(edit);
                            }
                        }
                        if layer_data.flags.modified {
                            reparsed_layers += 1;
                            layer_data.parse(source, loader)
                        } else {
                            Ok(ParseCallStats {
                                reused_old_tree: false,
                                had_tree_before: true,
                                tree_range_start: tree.root_node().start_byte(),
                                tree_range_end: tree.root_node().end_byte(),
                                included_range_start: layer_data
                                    .ranges
                                    .first()
                                    .map(|r| r.start_byte)
                                    .unwrap_or(0),
                                included_range_end: layer_data
                                    .ranges
                                    .last()
                                    .map(|r| r.end_byte)
                                    .unwrap_or(u32::MAX),
                                included_range_bytes: layer_data
                                    .ranges
                                    .iter()
                                    .map(|r| r.end_byte.saturating_sub(r.start_byte))
                                    .sum(),
                            })
                        }
                    } else {
                        fresh_parse_layers += 1;
                        layer_data.parse(source, loader)
                    };
                    let parse_elapsed = parse_start.elapsed();
                    let parse_outcome = match &parse_result {
                        Ok(_) => "ok",
                        Err(Error::Timeout) => {
                            parse_timeout_layers += 1;
                            "timeout"
                        }
                        Err(_) => "error",
                    };

                    log_trace_phase("tree_house", "layer_parse", parse_elapsed, || {
                        format!(
                            "layer={} root={} ranges={} had_tree={} parse_incomplete_before={} modified={} moved={} reused={} injections_before={} outcome={} source_bytes={} edits={}",
                            layer_idx,
                            is_root,
                            ranges_len,
                            had_tree,
                            parse_incomplete_before,
                            flags_modified,
                            flags_moved,
                            flags_reused,
                            injections_before,
                            parse_outcome,
                            source.len_bytes(),
                            edits.len(),
                        )
                    });

                    let parse_stats = parse_result?;

                    log_trace_phase("tree_house", "layer_parse_detail", parse_elapsed, || {
                        format!(
                            "layer={} root={} outcome={} reused_old_tree={} had_tree_before={} tree_range={}..{} included_range={}..{} included_range_bytes={}",
                            layer_idx,
                            is_root,
                            parse_outcome,
                            parse_stats.reused_old_tree,
                            parse_stats.had_tree_before,
                            parse_stats.tree_range_start,
                            parse_stats.tree_range_end,
                            parse_stats.included_range_start,
                            parse_stats.included_range_end,
                            parse_stats.included_range_bytes
                        )
                    });

                    (
                        ranges_len,
                        had_tree,
                        defer_root_queries,
                        parse_incomplete_before,
                        flags_modified,
                        flags_moved,
                        flags_reused,
                        injections_before,
                        parse_elapsed,
                        parse_outcome,
                    )
                }
            };

            let queue_len_before = queue.len();
            if defer_root_queries {
                let defer_start = Instant::now();
                self.map_injections(layer, None, edits);
                let defer_elapsed = defer_start.elapsed();
                let stats = InteractiveUpdateStats {
                    update_start,
                    layers_before,
                    incomplete_before,
                    visited_layers,
                    skipped_empty_layers,
                    reset_incomplete_layers,
                    fresh_parse_layers,
                    reparsed_layers,
                    parse_timeout_layers,
                };
                log_trace_phase("tree_house", "defer_root_queries", defer_elapsed, || {
                    format!(
                        "layer={} root=true source_bytes={} edits={} timeout_ms={} injections_before={} visited_layers={} parse_elapsed_us={} parse_outcome={}",
                        layer_idx,
                        source.len_bytes(),
                        edits.len(),
                        timeout.as_millis(),
                        injections_before,
                        visited_layers,
                        parse_elapsed.as_micros(),
                        parse_outcome,
                    )
                });
                return self.defer_interactive_update(
                    request,
                    stats,
                    "root_pre_query_threshold",
                    layer_idx,
                    queue.len(),
                );
            }

            let injection_start = Instant::now();
            self.run_injection_query(layer, edits, source, loader, |layer| queue.push(layer));
            let injection_elapsed = injection_start.elapsed();
            let injections_after = self.layer(layer).injections.len();
            let enqueued_layers = queue.len().saturating_sub(queue_len_before);
            log_trace_phase("tree_house", "injection_query", injection_elapsed, || {
                format!(
                    "layer={} root={} ranges={} had_tree={} parse_incomplete_before={} modified={} moved={} reused={} injections_before={} injections_after={} enqueued_layers={} parse_elapsed_us={} parse_outcome={}",
                    layer_idx,
                    is_root,
                    ranges_len,
                    had_tree,
                    parse_incomplete_before,
                    flags_modified,
                    flags_moved,
                    flags_reused,
                    injections_before,
                    injections_after,
                    enqueued_layers,
                    parse_elapsed.as_micros(),
                    parse_outcome,
                )
            });

            let defer_after_root_injection = interactive_root
                && enqueued_layers >= DEFER_ROOT_QUERIES_ENQUEUED_LAYERS_THRESHOLD;
            if defer_after_root_injection {
                let stats = InteractiveUpdateStats {
                    update_start,
                    layers_before,
                    incomplete_before,
                    visited_layers,
                    skipped_empty_layers,
                    reset_incomplete_layers,
                    fresh_parse_layers,
                    reparsed_layers,
                    parse_timeout_layers,
                };
                log_trace_phase("tree_house", "defer_root_queries_post_injection", injection_elapsed, || {
                    format!(
                        "layer={} root=true source_bytes={} edits={} timeout_ms={} injections_before={} injections_after={} enqueued_layers={} visited_layers={} parse_elapsed_us={} parse_outcome={}",
                        layer_idx,
                        source.len_bytes(),
                        edits.len(),
                        timeout.as_millis(),
                        injections_before,
                        injections_after,
                        enqueued_layers,
                        visited_layers,
                        parse_elapsed.as_micros(),
                        parse_outcome,
                    )
                });
                return self.defer_interactive_update(
                    request,
                    stats,
                    "root_post_injection_fanout",
                    layer_idx,
                    queue.len(),
                );
            }

            let local_start = Instant::now();
            self.run_local_query(layer, source, loader);
            let (local_scope_count, local_definition_count) = {
                let layer_data = self.layer(layer);
                (
                    layer_data.locals.scope_count(),
                    layer_data.locals.definition_count(),
                )
            };
            self.layer_mut(layer).query_stale = false;
            let local_elapsed = local_start.elapsed();
            log_trace_phase("tree_house", "local_query", local_elapsed, || {
                format!(
                    "layer={} root={} ranges={} had_tree={} injections_after={} local_scopes={} local_definitions={} source_bytes={}",
                    layer_idx,
                    is_root,
                    ranges_len,
                    had_tree,
                    injections_after,
                    local_scope_count,
                    local_definition_count,
                    source.len_bytes()
                )
            });
            if is_root {
                let query_total = injection_elapsed + local_elapsed;
                let total_layers = self.layers.len();
                log_trace_phase("tree_house", "root_query_summary", query_total, || {
                    format!(
                        "source_bytes={} parse_elapsed_us={} injection_elapsed_us={} local_elapsed_us={} injections_before={} injections_after={} enqueued_layers={} local_scopes={} local_definitions={} total_layers={} parse_outcome={}",
                        source.len_bytes(),
                        parse_elapsed.as_micros(),
                        injection_elapsed.as_micros(),
                        local_elapsed.as_micros(),
                        injections_before,
                        injections_after,
                        enqueued_layers,
                        local_scope_count,
                        local_definition_count,
                        total_layers,
                        parse_outcome,
                    )
                });
            }

            if interactive_update
                && !queue.is_empty()
                && (visited_layers >= INTERACTIVE_UPDATE_VISITED_LAYERS_THRESHOLD
                    || fresh_parse_layers >= INTERACTIVE_UPDATE_FRESH_LAYERS_THRESHOLD)
            {
                let stats = InteractiveUpdateStats {
                    update_start,
                    layers_before,
                    incomplete_before,
                    visited_layers,
                    skipped_empty_layers,
                    reset_incomplete_layers,
                    fresh_parse_layers,
                    reparsed_layers,
                    parse_timeout_layers,
                };
                let reason = if fresh_parse_layers >= INTERACTIVE_UPDATE_FRESH_LAYERS_THRESHOLD {
                    "interactive_fresh_layer_budget"
                } else {
                    "interactive_visited_layer_budget"
                };
                return self.defer_interactive_update(
                    request,
                    stats,
                    reason,
                    layer_idx,
                    queue.len(),
                );
            }
        }

        if self.layer(self.root).parse_tree.is_none() {
            log_trace_phase("tree_house", "update_summary", update_start.elapsed(), || {
                format!(
                    "result=no_root_config source_bytes={} edits={} timeout_ms={} layers_before={} visited_layers={} skipped_empty_layers={} reset_incomplete_layers={} fresh_parse_layers={} reparsed_layers={} parse_timeout_layers={} incomplete_before={}",
                    source.len_bytes(),
                    edits.len(),
                    timeout.as_millis(),
                    layers_before,
                    visited_layers,
                    skipped_empty_layers,
                    reset_incomplete_layers,
                    fresh_parse_layers,
                    reparsed_layers,
                    parse_timeout_layers,
                    incomplete_before
                )
            });
            return Err(Error::NoRootConfig);
        }

        let prune_start = Instant::now();
        let layers_before_prune = self.layers.len();
        self.prune_dead_layers();
        let prune_elapsed = prune_start.elapsed();
        let layers_after = self.layers.len();
        let incomplete_after = self
            .layers
            .iter()
            .filter(|(_, layer)| layer.parse_incomplete)
            .count();
        log_trace_phase("tree_house", "prune_dead_layers", prune_elapsed, || {
            format!(
                "layers_before_prune={} layers_after={} pruned_layers={}",
                layers_before_prune,
                layers_after,
                layers_before_prune.saturating_sub(layers_after)
            )
        });
        log_trace_phase("tree_house", "update_summary", update_start.elapsed(), || {
            format!(
                "result=ok source_bytes={} edits={} timeout_ms={} layers_before={} layers_after={} visited_layers={} skipped_empty_layers={} reset_incomplete_layers={} fresh_parse_layers={} reparsed_layers={} parse_timeout_layers={} incomplete_before={} incomplete_after={}",
                source.len_bytes(),
                edits.len(),
                timeout.as_millis(),
                layers_before,
                layers_after,
                visited_layers,
                skipped_empty_layers,
                reset_incomplete_layers,
                fresh_parse_layers,
                reparsed_layers,
                parse_timeout_layers,
                incomplete_before,
                incomplete_after
            )
        });
        Ok(())
    }

    /// Reset all `LayerUpdateFlags` and remove all untouched layers
    fn prune_dead_layers(&mut self) {
        self.layers
            .retain(|_, layer| take(&mut layer.flags).touched);
        self.cleanup_stale_layer_refs();
    }
}

impl LayerData {
    fn parse(
        &mut self,
        source: RopeSlice,
        loader: &impl LanguageLoader,
    ) -> Result<ParseCallStats, Error> {
        let Some(config) = loader.get_config(self.language) else {
            return Ok(ParseCallStats {
                reused_old_tree: false,
                had_tree_before: self.parse_tree.is_some(),
                tree_range_start: 0,
                tree_range_end: 0,
                included_range_start: self.ranges.first().map(|r| r.start_byte).unwrap_or(0),
                included_range_end: self.ranges.last().map(|r| r.end_byte).unwrap_or(u32::MAX),
                included_range_bytes: self
                    .ranges
                    .iter()
                    .map(|r| r.end_byte.saturating_sub(r.start_byte))
                    .sum(),
            });
        };
        if let Err(err) = self.parser.set_grammar(config.grammar) {
            return Err(Error::IncompatibleGrammar(self.language, err));
        }
        self.parser
            .set_included_ranges(&self.ranges)
            .map_err(|_| Error::InvalidRanges)?;

        // HACK:
        // This is a workaround for a bug within the lexer (in the C library) or maybe within
        // tree-sitter-markdown which needs more debugging. When adding a new range to a combined
        // injection and passing the old tree, if the old tree doesn't already cover a wider range
        // than the newly added range, some assumptions are violated in the lexer and it tries to
        // access some invalid memory, resulting in a segfault. This workaround avoids that
        // situation by avoiding passing the old tree when the old tree's range doesn't cover the
        // total range of `self.ranges`.
        //
        // See <https://github.com/helix-editor/helix/pull/12972#issuecomment-2725410409>.
        let included_range_start = self.ranges.first().map(|r| r.start_byte).unwrap_or(0);
        let included_range_end = self.ranges.last().map(|r| r.end_byte).unwrap_or(u32::MAX);
        let included_range_bytes = self
            .ranges
            .iter()
            .map(|r| r.end_byte.saturating_sub(r.start_byte))
            .sum();
        let had_tree_before = self.parse_tree.is_some();
        let tree_range = self
            .parse_tree
            .as_ref()
            .map(|tree| tree.root_node().byte_range())
            .unwrap_or(0..0);

        let tree = self.parse_tree.as_ref().filter(|tree| {
            let included_ranges_range = self.ranges.first().map(|r| r.start_byte).unwrap_or(0)
                ..self.ranges.last().map(|r| r.end_byte).unwrap_or(u32::MAX);
            // Allow re-parsing the root layer even though the range is larger. The root always
            // covers `0..u32::MAX`:
            if included_ranges_range == (0..u32::MAX) {
                return true;
            }
            let tree_range = tree.root_node().byte_range();
            tree_range.start <= included_ranges_range.start
                && tree_range.end >= included_ranges_range.end
        });
        let reused_old_tree = tree.is_some();

        match self.parser.parse(source, tree) {
            Some(tree) => {
                self.parse_tree = Some(tree);
                self.parse_incomplete = false;
                Ok(ParseCallStats {
                    reused_old_tree,
                    had_tree_before,
                    tree_range_start: tree_range.start,
                    tree_range_end: tree_range.end,
                    included_range_start,
                    included_range_end,
                    included_range_bytes,
                })
            }
            None => {
                self.parse_incomplete = true;
                Err(Error::Timeout)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct LayerUpdateFlags {
    pub reused: bool,
    pub modified: bool,
    pub moved: bool,
    pub touched: bool,
}

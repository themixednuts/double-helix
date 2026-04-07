//! Deterministic stress-test / benchmark harness for the Helix editor (headless).
//!
//! Uses the same tight-loop bench infrastructure as the live `:bench` command,
//! running through [`Application::bench_run_loop`] with shared action tables
//! and content generation from `helix_view::bench`.
//!
//! Run:
//!   cargo run --bin hx-bench --features integration --release -- [OPTIONS]
//!
//! Options:
//!   --seed <u64>       PRNG seed (default: 42)
//!   --duration <secs>  Benchmark duration in seconds (default: 30)
//!   --width <u16>      Terminal width (default: 120)
//!   --height <u16>     Terminal height (default: 50)

use std::time::Duration;

use anyhow::Context;
use helix_core::{syntax, Selection};
use helix_term::{application::Application, args::Args, config::Config};
use helix_view::bench::{
    enter_bench_command, generate_bench_content, log_run_event, BenchCommandContext, BenchState,
};
use helix_view::commands::editing;
use tokio_stream::wrappers::UnboundedReceiverStream;

// ---------------------------------------------------------------------------
// CLI argument parsing (minimal, no extra deps)
// ---------------------------------------------------------------------------

struct BenchArgs {
    seed: u64,
    duration_secs: u64,
    width: u16,
    height: u16,
    fixture: Option<String>,
    lines: usize,
    bytes_per_line: usize,
    renders: u32,
}

impl Default for BenchArgs {
    fn default() -> Self {
        Self {
            seed: 42,
            duration_secs: 30,
            width: 120,
            height: 50,
            fixture: None,
            lines: 100,
            bytes_per_line: 18_500,
            renders: 5,
        }
    }
}

fn parse_args() -> BenchArgs {
    let mut args = BenchArgs::default();
    let raw: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--seed" => {
                i += 1;
                args.seed = raw[i].parse().expect("invalid --seed value");
            }
            "--duration" => {
                i += 1;
                args.duration_secs = raw[i].parse().expect("invalid --duration value");
            }
            "--width" => {
                i += 1;
                args.width = raw[i].parse().expect("invalid --width value");
            }
            "--height" => {
                i += 1;
                args.height = raw[i].parse().expect("invalid --height value");
            }
            "--fixture" => {
                i += 1;
                args.fixture = Some(raw[i].clone());
            }
            "--lines" => {
                i += 1;
                args.lines = raw[i].parse().expect("invalid --lines value");
            }
            "--bytes-per-line" => {
                i += 1;
                args.bytes_per_line = raw[i].parse().expect("invalid --bytes-per-line value");
            }
            "--renders" => {
                i += 1;
                args.renders = raw[i].parse().expect("invalid --renders value");
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: hx-bench [--seed N] [--duration SECS] [--width N] [--height N] [--fixture giant-lines-render|giant-lines-collapse-render|document-change-fanout|document-change-fanout-matrix] [--lines N] [--bytes-per-line N] [--renders N]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    args
}

// ---------------------------------------------------------------------------
// Main driver
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("unable to build tokio runtime")?;
    let runtime = helix_runtime::Runtime::new(tokio_runtime.handle().clone());
    tokio_runtime.block_on(main_impl(runtime))
}

async fn main_impl(runtime: helix_runtime::Runtime) -> anyhow::Result<()> {
    let bench_args = parse_args();
    let duration = Duration::from_secs(bench_args.duration_secs);

    // Initialize helix-loader paths (needed for BenchState log file)
    helix_loader::initialize_config_file(None);
    helix_loader::initialize_log_file(None);

    eprintln!("Helix Benchmark (headless)");
    eprintln!("  seed:     {}", bench_args.seed);
    eprintln!("  duration: {}s", bench_args.duration_secs);
    eprintln!("  term:     {}x{}", bench_args.width, bench_args.height);

    // Build the application
    let config = Config::default();
    let lang_config = helix_loader::config::default_lang_config();
    let syn_loader = syntax::Loader::new(lang_config.try_into().unwrap()).unwrap();
    let mut app = Application::new(Args::default(), config, syn_loader, runtime)?;

    if let Some(fixture_name) = bench_args.fixture.as_deref() {
        let event_log_path = std::env::temp_dir().join(format!(
            "hx-bench-{}-{}-{}-{}.log",
            fixture_name,
            bench_args.lines,
            bench_args.bytes_per_line,
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&event_log_path);
        let _run_guard = helix_view::bench::enter_bench_run(helix_view::bench::BenchRunContext {
            seed: bench_args.seed,
            event_log_path: event_log_path.clone(),
        });

        let (fixture_content, requested_lines) = match fixture_name {
            "giant-lines-render" | "giant-lines-collapse-render" => (
                giant_lines_fixture(bench_args.lines, bench_args.bytes_per_line),
                bench_args.lines,
            ),
            "document-change-fanout" | "document-change-fanout-matrix" => {
                let content = generate_bench_content(bench_args.seed, bench_args.lines.max(500));
                let requested_lines = content.lines().count();
                (content, requested_lines)
            }
            other => {
                anyhow::bail!("unknown fixture: {other}");
            }
        };

        let (view_id, doc_id) =
            install_fixture_document(&mut app, &fixture_content, requested_lines);

        eprintln!("Helix Benchmark (deterministic render fixture)");
        eprintln!(
            "  fixture:  {}",
            bench_args
                .fixture
                .as_deref()
                .unwrap_or("giant-lines-render")
        );
        eprintln!("  lines:    {}", bench_args.lines);
        eprintln!("  bytes:    {}", bench_args.bytes_per_line);
        eprintln!("  renders:  {}", bench_args.renders);
        eprintln!("  term:     {}x{}", bench_args.width, bench_args.height);
        eprintln!("  eventlog: {}", event_log_path.display());

        if fixture_name == "document-change-fanout-matrix" {
            let scenarios = [
                ("fanout-small", 800usize),
                ("fanout-medium", 2000usize),
                ("fanout-large", 5000usize),
            ];

            for (label, scenario_lines) in scenarios {
                let content = generate_bench_content(
                    bench_args.seed.wrapping_add(scenario_lines as u64),
                    scenario_lines,
                );
                let scenario_requested_lines = content.lines().count();
                let (view_id, doc_id) =
                    install_fixture_document(&mut app, &content, scenario_requested_lines);

                let pre = app.render_timed().await;
                let snippet = generate_bench_content(
                    bench_args.seed.wrapping_add(scenario_lines as u64 + 1),
                    120,
                );
                let snippet_chars = snippet.chars().count();
                let mutation_context = BenchCommandContext {
                    seed: bench_args.seed,
                    event_log_path: Some(event_log_path.clone()),
                    action_index: scenario_lines as u64,
                    elapsed_secs: 0.0,
                    category: "fixture",
                    macro_str: label,
                    force_insert: false,
                };
                let _mutation_guard = enter_bench_command(mutation_context);
                let mutation_start = std::time::Instant::now();
                {
                    let doc = app.editor.documents.get_mut(&doc_id).unwrap();
                    let insert_at = doc.text().len_chars() / 2;
                    let trans = helix_core::Transaction::change(
                        doc.text(),
                        [(insert_at, insert_at, Some(snippet.clone().into()))].into_iter(),
                    );
                    doc.apply(&trans, view_id);
                    app.editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
                        doc.append_changes_to_history(view)
                    });
                }
                let mutation_elapsed = mutation_start.elapsed();
                let post = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "matrix {}: pre_render_us={} mutation_us={} post_render_us={} inserted_chars={} lines={} bytes={}",
                    label,
                    pre.as_micros(),
                    mutation_elapsed.as_micros(),
                    post.as_micros(),
                    snippet_chars,
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }
        } else if fixture_name == "document-change-fanout" {
            for render_idx in 0..bench_args.renders {
                let elapsed = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "pre-mutation render {}: elapsed_us={} lines={} bytes={}",
                    render_idx + 1,
                    elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }

            log_run_event("document_change_fixture", || {
                let doc = app.editor.documents.get(&doc_id).unwrap();
                format!(
                    "phase=before_mutation lines={} bytes={}",
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                )
            });

            let snippet = generate_bench_content(bench_args.seed.wrapping_add(1), 120);
            let snippet_chars = snippet.chars().count();
            let mutation_context = BenchCommandContext {
                seed: bench_args.seed,
                event_log_path: Some(event_log_path.clone()),
                action_index: 1,
                elapsed_secs: 0.0,
                category: "fixture",
                macro_str: "document_change_insert",
                force_insert: false,
            };
            let _mutation_guard = enter_bench_command(mutation_context);
            let mutation_start = std::time::Instant::now();
            {
                let doc = app.editor.documents.get_mut(&doc_id).unwrap();
                let insert_at = doc.text().len_chars() / 2;
                let trans = helix_core::Transaction::change(
                    doc.text(),
                    [(insert_at, insert_at, Some(snippet.clone().into()))].into_iter(),
                );
                doc.apply(&trans, view_id);
                app.editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
                    doc.append_changes_to_history(view)
                });
            }
            let mutation_elapsed = mutation_start.elapsed();
            let doc = app.editor.documents.get(&doc_id).unwrap();
            eprintln!(
                "mutation: elapsed_us={} inserted_chars={} lines={} bytes={}",
                mutation_elapsed.as_micros(),
                snippet_chars,
                doc.text().len_lines(),
                doc.text().len_bytes()
            );
            log_run_event("document_change_fixture", || {
                format!(
                    "phase=after_mutation elapsed_us={} inserted_chars={} lines={} bytes={}",
                    mutation_elapsed.as_micros(),
                    snippet_chars,
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                )
            });

            for render_idx in 0..bench_args.renders {
                let elapsed = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "post-mutation render {}: elapsed_us={} lines={} bytes={}",
                    render_idx + 1,
                    elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }
        } else if fixture_name == "giant-lines-collapse-render" {
            for render_idx in 0..bench_args.renders {
                let elapsed = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "pre-collapse render {}: elapsed_us={} lines={} bytes={}",
                    render_idx + 1,
                    elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }

            log_run_event("collapse_fixture", || {
                format!(
                    "phase=before_collapse lines={} bytes={}",
                    app.editor
                        .documents
                        .get(&doc_id)
                        .unwrap()
                        .text()
                        .len_lines(),
                    app.editor
                        .documents
                        .get(&doc_id)
                        .unwrap()
                        .text()
                        .len_bytes()
                )
            });

            let collapse_context = BenchCommandContext {
                seed: bench_args.seed,
                event_log_path: Some(event_log_path.clone()),
                action_index: 1,
                elapsed_secs: 0.0,
                category: "fixture",
                macro_str: "xJ",
                force_insert: false,
            };
            let _collapse_guard = enter_bench_command(collapse_context);
            let collapse_start = std::time::Instant::now();
            editing::join_selections(&mut app.editor, view_id, doc_id);
            let collapse_elapsed = collapse_start.elapsed();
            let doc = app.editor.documents.get(&doc_id).unwrap();
            eprintln!(
                "collapse: elapsed_us={} lines={} bytes={}",
                collapse_elapsed.as_micros(),
                doc.text().len_lines(),
                doc.text().len_bytes()
            );
            log_run_event("collapse_fixture", || {
                format!(
                    "phase=after_collapse elapsed_us={} lines={} bytes={}",
                    collapse_elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                )
            });

            for render_idx in 0..bench_args.renders {
                let elapsed = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "post-collapse render {}: elapsed_us={} lines={} bytes={}",
                    render_idx + 1,
                    elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }
        } else {
            for render_idx in 0..bench_args.renders {
                let elapsed = app.render_timed().await;
                let doc = app.editor.documents.get(&doc_id).unwrap();
                eprintln!(
                    "render {}: elapsed_us={} lines={} bytes={}",
                    render_idx + 1,
                    elapsed.as_micros(),
                    doc.text().len_lines(),
                    doc.text().len_bytes()
                );
            }
        }

        if let Ok(trace) = std::fs::read_to_string(&event_log_path) {
            eprintln!("{trace}");
        }

        let errs = app.close().await;
        for err in &errs {
            eprintln!("  Close error: {err}");
        }
        return Ok(());
    }

    // Generate synthetic content using the shared generator
    let content = generate_bench_content(bench_args.seed, 5000);
    let line_count = content.lines().count();
    eprintln!("  Generated {line_count} lines of synthetic Rust-like content");
    eprintln!();

    // Insert the synthetic content into the initial document
    {
        let view_id = app.editor.tree.focus;
        let view = app.editor.tree.get(view_id);
        let doc_id = view.doc;
        let doc = app.editor.documents.get_mut(&doc_id).unwrap();
        let sel = doc.selection(view_id).clone();
        let text_len = doc.text().len_chars();
        let trans = helix_core::Transaction::change_by_selection(doc.text(), &sel, |_| {
            (0, text_len, Some(content.clone().into()))
        });
        doc.apply(&trans, view_id);
    }

    // Set up a dummy input stream (bench_run_loop polls it for Ctrl+C)
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = UnboundedReceiverStream::new(rx);

    // Initial render
    app.render_timed().await;

    // Activate the bench — same as `:bench` does
    app.editor.bench = Some(BenchState::new(bench_args.seed, duration));

    eprintln!("  Running bench for {}s...", bench_args.duration_secs);

    // Run the same tight-loop as the live `:bench` command
    app.bench_run_loop(&mut rx_stream).await;

    // bench_run_loop prints the report to stderr when the bench expires.
    // If bench state is still around (shouldn't be), print report manually.
    if let Some(mut bench) = app.editor.bench.take() {
        let report = bench.report();
        eprintln!("{report}");
    }

    // Clean shutdown
    let errs = app.close().await;
    for err in &errs {
        eprintln!("  Close error: {err}");
    }

    Ok(())
}

fn giant_lines_fixture(lines: usize, bytes_per_line: usize) -> String {
    (0..lines)
        .map(|idx| {
            char::from(b'a' + (idx % 26) as u8)
                .to_string()
                .repeat(bytes_per_line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn install_fixture_document(
    app: &mut Application,
    content: &str,
    requested_lines: usize,
) -> (helix_view::ViewId, helix_view::DocumentId) {
    let view_id = app.editor.tree.focus;
    let view = app.editor.tree.get(view_id);
    let doc_id = view.doc;
    let doc = app.editor.documents.get_mut(&doc_id).unwrap();
    let text_len = doc.text().len_chars();
    let trans = helix_core::Transaction::change(
        doc.text(),
        [(0, text_len, Some(content.into()))].into_iter(),
    );
    doc.apply(&trans, view_id);
    let fixture_path = std::env::temp_dir().join("hx-bench-fixture.rs");
    doc.set_path(Some(&fixture_path));
    let loader = app.editor.syn_loader.load();
    doc.detect_language(&loader);
    let last_line = doc.text().len_lines().saturating_sub(1);
    let anchor_line = (requested_lines / 2).min(last_line);
    let anchor = doc.text().line_to_char(anchor_line);
    let end = doc.text().len_chars();
    doc.set_selection(view_id, Selection::single(0, end));
    doc.set_view_offset(
        view_id,
        helix_view::view::ViewPosition {
            anchor,
            vertical_offset: 0,
            horizontal_offset: 0,
        },
    );
    app.editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        doc.append_changes_to_history(view)
    });
    (view_id, doc_id)
}

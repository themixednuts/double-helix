use std::time::Duration;

#[cfg(feature = "bench")]
use crate::{SLOW_TRACE_US, TRACE_CONTEXT};

pub(crate) fn log_trace_phase<F>(
    component: &'static str,
    phase: &'static str,
    elapsed: Duration,
    details: F,
) where
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
        if elapsed_us < SLOW_TRACE_US {
            return;
        }

        TRACE_CONTEXT.with(|ctx| {
            let binding = ctx.borrow();
            let Some(ctx) = binding.as_ref() else {
                return;
            };

            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&ctx.log_path)
            {
                let macro_name = if ctx.macro_str.is_empty() {
                    "<insert>"
                } else {
                    ctx.macro_str
                };
                let _ = writeln!(
                    f,
                    concat!(
                        "syntax_phase",
                        " seed={}",
                        " elapsed_s={:.3}",
                        " action_index={}",
                        " category={:?}",
                        " macro={:?}",
                        " force_insert={}",
                        " component={:?}",
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
                    component,
                    phase,
                    elapsed_us,
                    details(),
                );
            }
        });
    }
}

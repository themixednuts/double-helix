use crate::editor::Breakpoint;

#[macro_export]
macro_rules! debugger {
    ($editor:expr) => {{
        let Some(debugger) = $editor.debug_adapters.get_active_client_mut() else {
            return;
        };
        debugger
    }};
}

pub fn dap_pos_to_pos(doc: &helix_core::Rope, line: usize, column: usize) -> Option<usize> {
    let line = doc.try_line_to_char(line.checked_sub(1)?).ok()?;
    Some(line + column.saturating_sub(1))
}

pub fn source_breakpoints(breakpoints: &[Breakpoint]) -> Vec<helix_dap::SourceBreakpoint> {
    breakpoints
        .iter()
        .map(|breakpoint| helix_dap::SourceBreakpoint {
            line: breakpoint.line.saturating_add(1),
            condition: breakpoint.condition.clone(),
            hit_condition: breakpoint.hit_condition.clone(),
            log_message: breakpoint.log_message.clone(),
            ..Default::default()
        })
        .collect()
}

pub fn apply_breakpoints_response(
    breakpoints: &mut [Breakpoint],
    response: Option<Vec<helix_dap::Breakpoint>>,
) {
    let Some(response) = response else {
        return;
    };
    for (breakpoint, dap_breakpoint) in breakpoints.iter_mut().zip(response) {
        breakpoint.id = dap_breakpoint.id;
        breakpoint.verified = dap_breakpoint.verified;
        breakpoint.message = dap_breakpoint.message;
        if let Some(line) = dap_breakpoint.line {
            breakpoint.line = line.saturating_sub(1);
        }
        breakpoint.column = dap_breakpoint.column;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dap_position_rejects_zero_line_without_underflow() {
        let text = helix_core::Rope::from("first\nsecond\n");
        assert_eq!(dap_pos_to_pos(&text, 0, 1), None);
        assert_eq!(dap_pos_to_pos(&text, 2, 1), Some(6));
    }

    #[test]
    fn source_breakpoints_preserve_conditions_and_log_messages() {
        let source = source_breakpoints(&[Breakpoint {
            line: 4,
            condition: Some("ready".into()),
            hit_condition: Some("3".into()),
            log_message: Some("value={value}".into()),
            ..Default::default()
        }]);

        assert_eq!(source[0].line, 5);
        assert_eq!(source[0].condition.as_deref(), Some("ready"));
        assert_eq!(source[0].hit_condition.as_deref(), Some("3"));
        assert_eq!(source[0].log_message.as_deref(), Some("value={value}"));
    }
}

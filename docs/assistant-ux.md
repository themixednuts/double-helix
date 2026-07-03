# Assistant UX Model

The assistant is a docked panel.

It is not a window, a modal editor, or a collection of nested widgets.

The panel participates in editor layout as chrome beside the editor.

The panel has one header line and one footer hint bar.

The header shows the active thread title, focus mode, assistant mode, model, token usage, and run state.

The footer is the interaction contract.

Every key shown in the footer is active for the current layer.

Every active panel key that should be discoverable is shown in the footer.

The panel has exactly two focus modes.

Input mode focuses the prompt editor.

Messages mode focuses the transcript list.

Tab toggles from Input to Messages.

Esc from Messages returns to Input.

Entering the panel lands in Input when the assistant is idle.

Entering the panel lands in Messages when a run is active.

Input mode uses the normal EditRegion modal editing model.

Input normal mode sends with Enter.

Input insert mode edits text and uses Enter as a newline.

The prompt supports `@` mention triggers and `/` command triggers.

The mode/model/config selector is opened as a standard picker.

Messages mode uses one list grammar.

`j` and `k` move the selected entry.

`gg` moves to the first entry.

`G` moves to the newest entry.

`Tab` expands or folds the selected entry.

`Enter` runs the selected entry's primary action.

`y` yanks the selected entry or selected request URL.

`t` follows output or jumps to a selected subagent target.

Cards are list entries.

Tool calls, diffs, terminals, thoughts, review summaries, and subagents do not own focus.

A selected card can expose extra keys.

Those extra keys appear in the footer while that card is selected.

Review cards expose accept and reject keys.

Foldable cards expose the shared fold key.

Subagent cards expose the shared primary and jump keys.

Card-transient states are the only in-panel sub-states.

Editing an elicitation form field pushes one transient layer.

Choosing an auth method pushes one transient layer.

Esc pops that transient layer back to Messages.

Tab and Shift-Tab move within the transient.

Enter submits or confirms the transient.

Ctrl-c is the explicit cancel path for pending assistant work.

Popups are standard editor components.

Mode, model, config, session history, permission requests, and confirmations use standard picker or confirm behavior.

Popups stack above the docked panel.

Esc closes only the top popup when a popup is open.

The panel does not implement assistant-specific popup key grammar.

The Esc rule always removes one layer.

Popup Esc closes the popup.

Card-transient Esc returns to Messages.

Messages Esc returns to Input.

Input insert Esc returns to Input normal mode.

Input normal Esc follows the editor's normal focused-component convention.

The hint bar is tested against the binding tables.

If hints and dispatch diverge, it is a bug.

# Helix-Term Layering Violations

This document lists modules in `helix-term` that perform non-frontend work (commands, state transitions, selection logic, etc.) and their target homes for the frontend decoupling refactor.

## Summary

| Module | Violation | Target Home |
|--------|-----------|-------------|
| `commands.rs`, `commands/*` | Command definitions, execution, editor state mutations | helix-view |
| `keymap.rs`, `keymap/*` | Keymap structure, command lookup, infobox | helix-view |
| `ui/editor.rs` | Command dispatch, keymap handling, selection changes | helix-view (logic) + helix-term (render) |
| `ui/mod.rs` | `prompt`, `file_picker`, `raw_regex_prompt` — command orchestration | helix-view |
| `handlers/*` | Completion, auto-save, diagnostics — editor state + LSP | helix-view |
| `job.rs` | Job dispatch — editor operations | helix-view |

---

## Detailed Violations

### Commands (`commands.rs`, `commands/*.rs`)

**What:** All command definitions (static, typable, mappable), command execution, context manipulation.

**Violation:** Commands mutate editor state, selections, buffers, config, LSP requests. They orchestrate UI (picker, prompt, palette) via `compositor::Context` but the logic is frontend-agnostic.

**Target:** `helix-view`. Commands should live in helix-view; UI orchestration via `UiBridge` trait.

**Files:**
- `commands.rs` — main command registry, `Context`, `MappableCommand`, static commands
- `commands/typed.rs` — `:write`, `:quit`, `:open`, etc.
- `commands/lsp.rs` — LSP commands (goto_def, hover, etc.)
- `commands/dap.rs` — DAP commands
- `commands/syntax.rs` — syntax tree commands
- `commands/notification.rs` — notification commands

---

### Keymap (`keymap.rs`, `keymap/*.rs`)

**What:** KeyTrie structure, keymap lookup, command resolution, infobox generation.

**Violation:** Keymap logic is frontend-agnostic. The mapping from keys to commands does not depend on terminal rendering.

**Target:** `helix-view`. Keymap structure and lookup belong in helix-view. Terminal-specific key event handling stays in helix-term.

**Files:**
- `keymap.rs` — `Keymaps`, `KeyTrie`, `KeyTrieNode`, lookup
- `keymap/default.rs` — default keybindings
- `keymap/macros.rs` — key parsing

---

### EditorView (`ui/editor.rs`)

**What:** Command dispatch, keymap handling, selection changes, insert mode handling.

**Violation:** `EditorView::handle_event` dispatches commands and mutates editor state. The rendering logic (terminal buffer, surface) is terminal-specific; the command dispatch and selection logic is not.

**Target:** Split. Command dispatch logic → `helix-view`. Terminal rendering (`render`, `render_view`, `render_document`) → `helix-term` (or compositor trait impl).

---

### UI Module (`ui/mod.rs`)

**What:** `prompt`, `file_picker`, `raw_regex_prompt`, `directory_content` — helpers that create UI components and wire callbacks.

**Violation:** These orchestrate command flows (open file, prompt, regex). The orchestration logic (what to show, when) is frontend-agnostic; the concrete `Prompt`, `Picker` types are terminal-specific.

**Target:** `helix-view` for orchestration; `helix-term` for terminal `Prompt`/`Picker` implementations. Introduce `UiBridge` trait for "show prompt", "open picker".

---

### Handlers (`handlers/*.rs`)

**What:** Completion, auto-save, auto-reload, diagnostics, blame, snippet, etc.

**Violation:** Handlers react to editor events and mutate editor state. LSP/DAP integration, completion logic, etc. are frontend-agnostic.

**Target:** `helix-view`. Handlers may need to show UI (completion popup, etc.) — route via `UiBridge`.

**Files:**
- `handlers/completion/*` — completion request, resolve, path, word
- `handlers/auto_save.rs`
- `handlers/auto_reload.rs`
- `handlers/diagnostics.rs`
- `handlers/blame.rs`
- `handlers/snippet.rs`
- `handlers/prompt.rs`
- `handlers/document_colors.rs`
- `handlers/signature_help.rs`

---

### Job (`job.rs`)

**What:** Job dispatch, `Callback` types, `EditorCompositorCallback`, `EditorCallback`.

**Violation:** Jobs perform editor operations (save, etc.). The dispatch mechanism is async; the operations are frontend-agnostic.

**Target:** `helix-view`. Job dispatch and callback types belong in helix-view. Terminal-specific rendering in callbacks stays in helix-term.

---

## What Stays in helix-term

| Module | Reason |
|--------|--------|
| `application.rs` | Event loop, terminal backend, resize handling |
| `compositor.rs` | Terminal compositor (layer stack, render) |
| `ui/*` (render) | Terminal buffer, surface, rendering |
| `shutdown.rs` | Windows/Unix console handling |
| `config.rs` | CLI/config loading (could move to helix-view) |
| `args.rs` | CLI argument parsing |
| `health.rs` | CLI health check |

---

## Migration Order

1. **Phase 1:** Define `helix-frontend` (or `helix-ui`) crate with traits.
2. **Phase 2:** Move commands to helix-view; introduce `CommandContext`; route UI via `UiBridge`.
3. **Phase 3:** Move keymap to helix-view; move handlers; split EditorView.

# UI Architecture Deepening Tasks

Status: active implementation plan.

This list tracks the remaining deepening work after the Ratatui port. The goal is to make each Module expose a smaller Interface with more Implementation depth behind it, improving Locality for maintenance and Leverage for callers.

## 1. File Explorer

- [x] Split file explorer model and preview cache into dedicated Modules.
- [x] Split `helix-term/src/ui/file_explorer.rs` further into input, actions, refresh, and render Modules.
- [x] Keep `FileExplorerPanel` as the public Interface and composition root.
- [x] Move filesystem scanning and cached tree construction behind a refresh Module.
- [x] Keep rendering backed by actual shared widgets, not story-only variants.

## 2. Lua Plugin Facade

- [x] Split workspace, documents, views, host, LSP, layout, and logging into domain Modules.
- [ ] Split remaining UI, events, commands, registers, splits, tabs, floats, assistant, config, surface, and conversion domains.
- [ ] Keep one registration Seam that installs all Lua modules.
- [ ] Keep the contract traits as the host-facing Interface.
- [ ] Avoid fallback paths or duplicate legacy APIs.

## 3. Render Frame Model

- [x] Move render frame data into a dedicated `compositor::render_frame` Module.
- [ ] Replace broad render query-bag access with narrower typed frame slices.
- [ ] Build immutable render models after `sync`.
- [ ] Pass each render Module only the data it needs.
- [ ] Preserve the option to parallelize render work safely.

## 4. Storybook

- [x] Split storybook model and shell Modules.
- [ ] Split story registry, story modules, dump output, and tests.
- [ ] Keep every story backed by runtime UI Modules.
- [ ] Support variants through real component state and style inputs.
- [ ] Keep the storybook shell clean and distinct from editor chrome.

## 5. Theme And Style

- [x] Add explicit file explorer style tokens.
- [ ] Centralize remaining theme lookup into explicit UI design tokens.
- [ ] Convert Helix styles to Ratatui styles at stable Seams.
- [ ] Reduce scattered `theme.get(...)` calls in render hot paths.
- [ ] Keep theme switching as a first-class storybook and runtime path.

## 6. Picker, Menu, Popup Primitives

- [x] Share selection viewport scroll math between menu, picker, and item list.
- [ ] Share prompt, preview, scroll-region, and table primitives.
- [ ] Keep picker/menu/popup composition data-driven.
- [ ] Avoid separate demo-only primitives.
- [ ] Add focused tests around shared behavior.

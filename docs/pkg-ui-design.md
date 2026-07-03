# :pkg Manager UI — Design

Status: implemented (2026-07-03). Reference point: mason.nvim's manager
buffer — kept: full-surface manager, sections, single-key ops, live rows.
Improved: real detail pane, update review, structured progress, registry
management, and consistency with this editor's interaction grammar.

## Shape

A full-area overlay layer (like the command palette / pickers — NOT a docked
panel; it is a transient management surface, esc-dismissable). Four regions:

```
┌ Browse │ Installed │ Updates(3) │ Registries ──────────── [/ search…] ┐
│                                                                       │
│  Language servers ─────────────────────────────────────────────────── │
│  ● rust-analyzer     2025-06-30   installed 2025-06-30    rust        │
│  ◍ lua-language-ser… ↓ 42%        installing…             lua         │
│  ○ gopls             1.28.0        —                      go          │
│  Debug adapters ────────────────────────────────────────────────────  │
│  ○ codelldb          1.11.0        —                      rust c cpp  │
│                                                                       │
├───────────────────────────────────────────────────────────────────── ┤
│ rust-analyzer  — Rust language server            language-server      │
│ installed 2025-06-30 (github-release, sha256 ✓)  latest 2025-06-30    │
│ source: github rust-lang/rust-analyzer → rust-analyzer.exe            │
│ languages: rust   aliases: ra   homepage ↗   doctor: ok               │
├───────────────────────────────────────────────────────────────────── ┤
│ ⣾ installing lua-language-server … 42%  (1 queued)                    │
└───────────────────────────────────────────────────────────────────── ┘
```

1. **Tab strip** (widgets::tabs — hit-ranges, overflow): Browse (whole
   registry), Installed, Updates (badge = count from `Ops::plan_update`),
   Registries (sources, cache age, update/validate). `1-4` jump, `[`/`]` cycle.
2. **List** (item_list/ListViewport — virtualized, multi-select): grouped by
   kind with sticky section headers. Row = state glyph (● installed,
   ◍ working w/ inline pct, ○ available, ⚠ problem, ↑ update available),
   name, version column (installed→candidate on Updates tab), status,
   language chips. Fuzzy filter via nucleo over `PackageSpec::search_terms`
   (name/aliases/categories/languages/schemas — already centralized).
   Filter chips: `f` cycles kind filter; search accepts `lang:rust`,
   `kind:dap`, `cat:formatter` prefixes.
3. **Detail pane** (bottom, ~6 rows; toggle zoom with `p`): full metadata for
   the selected entry — description, source per current platform, receipt
   (version/date/hash-verified), doctor state, homepage, schemas. On the
   Updates tab this becomes the review card: current → candidate, release
   age, source URL, per-package accept toggle. This is the vim.pack-style
   update review the engine's `UpdatePlan` was built for.
4. **Status row**: active op via progress_bar (structured pct from OpEvent —
   never parsed out of message strings) + queue depth. Errors land here AND
   as toasts (same ingress path as the CLI ops).

## Keys (one grammar, no chords)

Nav j/k gg/G, `/` search (esc: clear → close), space mark, `1-4`/`[`/`]`
tabs, `p` detail zoom.
Ops: `i`/enter install (marked or selected), `d` remove (PickerConfirmation
seam), `u` update selected, `U` apply the Updates tab's plan (review first —
never blind), `r` rollback, `!` doctor selected / all on empty selection,
`R` (Registries tab) update source.
`?` opens the standard Info popup generated from the binding table — NO key
hints in any status row (editor-wide ruling). Esc exits one layer:
search → selection/marks → close.

## Behavior rules

- Every op is async through the existing runtime/pkg ingress bridge; rows
  re-render from OpEvents (queued/downloading pct/building/activating/
  done/failed). The manager never blocks; closing it does not cancel ops.
- Multi-select batches respect policy failures per-package (one denial
  doesn't abort the batch; failed rows show ⚠ + reason in detail).
- Updates tab is plan-first: it renders `Ops::plan_update` output; `U`
  applies only what's accepted. Errors in the plan (`has_errors`) render as
  ⚠ rows, never silently dropped.
- Registries tab surfaces cache staleness (age since last update) and
  doctor validation state per source; `R` reuses the same op-event plumbing.
- Empty states per tab (dim, actionable: "no updates — u to check again").

## Implementation notes

Home: helix-term/src/ui/pkg.rs `PkgManager` (already a Component — keep).
Compose from the widget kit instead of bespoke helpers: widgets::tabs (tab
strip), item_list + ListViewport (virtualized list, marks), picker_table
style columns, ui::text_layout (wrap/truncate in detail pane), progress_bar
+ toast_queue (already wired), PickerConfirmation (remove confirm),
Info-from-binding-table (same generator as the assistant panel). Statusline
progress rendering belongs to the statusline module reading PkgProgressState
from the model — not computed inside ui/pkg.rs.

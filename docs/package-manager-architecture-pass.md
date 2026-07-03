# Package Manager Architecture Pass

Status: saved design pass, with obvious wins implemented in the same branch.
Date: 2026-07-03.

## Context

The package manager should be closer to Mason than to Neovim `vim.pack`.
Neovim's `vim.pack` is a good reference for lockfile-first UX and update
review, but it is a plugin manager. Mason is a closer fit because it manages
external tools: LSP servers, DAP adapters, formatters, linters, and their
per-platform executables.

The Helix package manager should keep `languages.toml` as the editor behavior
contract. Package metadata should satisfy the commands named by that contract;
it should not become a second language configuration format.

## Progressive Disclosure Of Abstractions

The package manager should reveal its modules in this order:

1. User surface: `:pkg` and `dhx pkg` expose names, kind, installed state,
   latest state, languages, and short metadata. The interface is package intent,
   not install mechanics.
2. Registry metadata: package entries describe what a package is: aliases,
   categories, language tags, schemas, homepage, description, source options,
   and per-platform artifacts. This module is declarative data, not executable
   install code.
3. Source selection: artifact order decides which source can satisfy the
   package on this host. System and native sources are probes; archives and
   package-ecosystem sources are store-managed installs; plugin sources cross
   the plugin backend seam.
4. Store and receipts: installed trees, shims, receipts, hashes, previous
   versions, and package locks are the operational interface. Callers should not
   need to know staging paths or activation details.
5. Policy: install policy checks source kind, source URL, build behavior,
   native installs, release age, and plugin backend allowlists before a backend
   can run.
6. Plugin backend seam: plugin backends are adapters for package sources Helix
   cannot know at compile time. They should remain explicit and allowlisted.

This gives users a small interface first, then allows power users and
maintainers to inspect deeper layers when something fails.

## Iterative Architecture Pass

### Candidate 1: Registry Metadata Module

Files: `helix-pkg/src/spec.rs`, `helix-pkg/src/registry.rs`, registry TOML.

Problem: registry entries only expose `languages` as searchable metadata.
That keeps the interface too narrow for Mason-like discovery: aliases,
categories, and schema links have nowhere to live. Without that module,
metadata either leaks into descriptions or gets added directly to UI code.

Solution: make `PackageSpec` own progressive-disclosure metadata and give the
registry a single search interface over those terms.

Benefits: search, UI, doctor, and future schema workflows get leverage from one
interface. Locality improves because metadata matching and validation live with
the package spec instead of spreading through CLI and UI callers.

Status: implemented in this pass.

### Candidate 2: Package-Local Mutation Locks

Files: `helix-pkg/src/store.rs`, `helix-pkg/src/ops.rs`.

Problem: installs are transactional once running, but concurrent install,
update, remove, or rollback operations for the same package can still interleave
before activation. Mason's per-package installer locks are the right shape.

Solution: add a store-owned package lock in `pkg/staging` and acquire it around
mutating package operations.

Benefits: the store module owns the concurrency invariant and callers keep using
the same operations interface. Tests can verify the lock interface directly.

Status: implemented in this pass.

### Candidate 3: Update Plan And Review Buffer

Files: `helix-pkg/src/ops.rs`, `helix-term/src/ui/pkg.rs`, editor command layer.

Problem: `update` applies immediately. Neovim `vim.pack` has a useful update
review step where users inspect the resolved changes before accepting them.

Solution: split update into a non-mutating update-plan interface and an apply
path. CLI can print the plan; the editor can later render a confirmation buffer
or picker preview.

Benefits: better trust and reproducibility. The interface becomes testable as
"given registry/lock/receipts, produce this update plan".

Status: plan interface and CLI output implemented after this pass with
`Ops::plan_update` and `dhx pkg update --plan`. Editor review UX remains
deferred.

### Candidate 4: Registry Source Cache

Files: `helix-pkg/src/registry.rs`, `helix-pkg/src/config.rs`, store layout.

Problem: builtin and local registries work, but there is not yet a first-class
registry cache/update module like Mason's registry refresh.

Solution: add named registry sources with an explicit update command and cached
checkout under the package store.

Benefits: user registries become reproducible and inspectable without requiring
a hosted service.

Status: implemented after this pass with `[[pkg.registry-sources]]`,
`dhx pkg registry list`, and `dhx pkg registry update [name]`. Hosted registry
APIs remain deferred.

### Candidate 5: Manifest-To-Lock Without Install

Files: `helix-pkg/src/ops.rs`, `helix-term/src/args.rs`, project `.helix`.

Problem: project manifests need a lock refresh path that does not also mutate
the package store. Without that, project reproducibility depends on running an
install just to write `.helix/pkg.lock`.

Solution: add a lock operation that resolves manifest entries into
`<config>/pkg.lock` or `<project>/.helix/pkg.lock` without installing. Named
arguments filter to manifest entries and preserve unrelated lock pins.

Benefits: per-project package intent becomes reviewable and commit-friendly.
`sync --project` can remain the applying operation while `lock --project`
updates reproducible input.

Status: implemented after this pass with `Ops::lock_manifest`,
`Ops::lock_project`, and `dhx pkg lock [--project <dir>] [name]...`.

### Candidate 6: Shared Registry Loading And Tool Resolution

Files: `helix-pkg/src/registry.rs`, `helix-view/src/editor/language.rs`,
`helix-view/src/document.rs`, `helix-pkg/src/resolve.rs`.

Problem: CLI operations and editor-side missing-server discovery can drift if
they assemble registries differently. Formatter commands also used PATH lookup
directly, so installed package shims helped LSP/DAP startup but not external
formatters.

Solution: make `Registry::from_config` the common entry point for direct
registries plus `[[pkg.registry-sources]]`. Add generic package-tool command
resolution and route formatter commands through it.

Benefits: editor suggestions, auto-install, CLI operations, and formatter
spawning now share the same package store and registry assumptions.

Status: implemented after this pass. `doctor` also reports missing or invalid
registry source caches.

## Design Rulings

- Do not mutate global PATH. Prefer explicit shim resolution before PATH.
- Do not copy Mason's dynamic installer scripts. Keep package specs
  declarative and policy-checkable.
- Do not copy `vim.pack` as the package model. Borrow lockfile and update-review
  UX from it instead.
- Do not move editor semantics out of `languages.toml`. The package manager can
  add metadata and installation affordances, but file detection, roots,
  comments, LSP client config, grammars, and language behavior stay in language
  config.
- Keep plugin backends behind an allowlisted seam. A plugin backend is powerful
  enough to run arbitrary install logic, so policy should make that explicit.

## Iffy Items To Continue Later

- Hosted registry API versus data-in-git registries.
- Editor affordances for creating and reviewing per-project package manifests.
- Native install prompts inside the editor.
- Editor update review UX: picker preview or scratch buffer.
- Whether package schemas should feed editor validation or remain only metadata.
- How much of tree-sitter grammar acquisition should move behind `pkg sync`.

## Remaining Work Queue

- Add `dhx pkg lock --fetch-hashes` so lock-only refresh can pin archive hashes
  without installing.
- Add first-class formatter and linter package metadata once there is enough
  registry data to justify new package kinds instead of generic tool shims.
- Add project-aware editor commands for `pkg lock --project` and
  `pkg sync --project` after the package manager UI direction is settled.
- Add native install confirmation flows inside the editor.
- Add update-review UX before broadening automatic updates.

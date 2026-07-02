# dhx Package Manager — Design

Status: approved design, implementation staged (2026-07).
Owner: architecture; implemented by staged codex waves.

## Problem

Today every runtime asset has a different acquisition story: grammars are
git-fetched and cc-built by helix-loader; LSP servers and DAP adapters must
be pre-installed on PATH by the user; plugins are folders copied into
`plugin_dirs`; remote plugin hosts assume the remote side is provisioned by
hand. There is no pinning, no reproducibility, no update story, and no
discoverability ("which server does this language want, and how do I get
it?").

## Goals

1. One surface — `dhx pkg <cmd>` on the CLI and `:pkg` inside the editor —
   for **LSP servers, DAP adapters, tree-sitter grammars, and plugins**
   (extensible to themes/queries later).
2. Reproducible: a per-user (and optionally per-project) manifest plus a
   lockfile with exact versions and content hashes.
3. Zero-config first run: opening a file whose language wants an
   uninstalled server yields a one-keystroke install path — never an
   automatic download unless the user opts in (`[pkg] auto-install = true`).
4. Offline-safe and non-blocking: all network/build work runs async through
   the runtime ingress; the editor never stalls.
5. Windows first-class (no symlink privilege assumptions; junctions or
   copy fallback).

## Non-goals (v1)

- A hosted central registry service. The registry is data-in-git.
- Sandboxed execution of installed tools (they already run unsandboxed as
  LSP/DAP servers today).
- Auto-provisioning remote plugin hosts over SSH (documented manual path;
  the transport already works once the remote side has `helix-plugin-host`).

## Architecture

New workspace crate **`helix-pkg`** (engine, no UI dependencies), consumed
by helix-term (CLI subcommand + typed commands + picker UI) and
helix-loader (grammar path resolution).

```
helix-pkg
├── registry   — package database parsing/merging (builtin + user registries)
├── spec       — PackageSpec: what to install and how (per-OS artifacts)
├── source     — install backends (github-release, archive, npm, cargo, pip, go, system)
├── store      — on-disk layout, receipts, junction/copy activation
├── lock       — manifest + lockfile round-trip
├── resolve    — "language/tool → package → installed binary" resolution
└── ops        — install/update/remove/doctor orchestration (async, progress events)
```

### Package identity and kinds

```toml
# registry entry (registry/<kind>/<name>.toml in-repo; user registries are
# git repos/dirs with the same layout, merged with later-wins precedence)
name = "rust-analyzer"
kind = "lsp"                     # lsp | dap | grammar | plugin
description = "Rust language server"
homepage = "https://rust-analyzer.github.io"
languages = ["rust"]             # what resolve uses to suggest it

[version]
tag-source = "github:rust-lang/rust-analyzer"   # where `update` finds versions

[[artifact]]                     # per-platform artifacts, first match wins
os = "windows"                   # windows | linux | macos
arch = "x86_64"                  # x86_64 | aarch64
source = { github-release = "rust-lang/rust-analyzer", asset = "rust-analyzer-x86_64-pc-windows-msvc.zip" }
bin = "rust-analyzer.exe"        # path inside the unpacked artifact

# alternative sources, by example:
# source = { npm = "typescript-language-server", bin-js = "lib/cli.mjs" }
# source = { cargo = "taplo-cli", features = ["lsp"] }
# source = { pip = "debugpy" }
# source = { go = "golang.org/x/tools/gopls" }
# source = { system = "clangd" }        # PATH detection only, never installed
# grammars: source = { git = "https://github.com/tree-sitter/tree-sitter-rust", rev = "..." }
```

Design rulings:
- **Data-in-git registry** (like mason-registry / Zed extensions), not a
  service: auditable, pinnable, forkable. The builtin registry ships in the
  repo under `registry/`; `[[pkg.registries]]` config adds user registries.
- **Per-OS artifact table** instead of install scripts. No arbitrary
  postinstall execution: `npm` runs with `--ignore-scripts`, archives are
  just unpacked, cargo/pip/go build in isolated roots. The only compiler
  invocation is the existing grammar cc path, folded in unchanged.
- **`system` backend** exists so the registry can also *describe* tools the
  user manages themselves; resolution treats a system match as satisfied.

### Store layout (data dir)

```
<data>/pkg/
  store/<kind>/<name>/<version>/...      # immutable install trees
  bin/<name>[.exe|.cmd]                  # activated shims (junction or thin launcher)
  receipts/<kind>-<name>.toml            # resolved version, source, sha256 set, install date
  runtimes/{node,py}/                    # shared npm prefix / venv, versioned
```

- Activation = writing a shim + receipt; deactivation = removing them. The
  store is content-addressed enough to keep two versions side by side and
  roll back instantly (`dhx pkg rollback <name>`).
- Windows: junctions for directories, generated `.cmd`/copied exe shims for
  binaries — no symlink privilege required. Unix: symlinks.
- Everything hash-verified: release assets against a sha256 recorded at
  lock time (TOFU on first install, then pinned; registry entries MAY
  pre-pin hashes).

### Manifest + lockfile

- `<config>/pkg.toml` — user intent: `lsp = ["rust-analyzer", ...]`,
  version ranges optional (default: track registry latest).
- Optional per-project `.helix/pkg.toml` merged on top (workspace-pinned
  toolchains).
- `<config>/pkg.lock` — exact resolved versions + hashes; `dhx pkg sync`
  reproduces the store from it on a new machine.

### Resolution (the integration seam)

`resolve::binary(kind, name)` returns the activated shim path. Language
configuration keeps working untouched: when languages.toml specifies a bare
command name, the lookup order becomes

1. explicit absolute path in config (always wins),
2. `<data>/pkg/bin/<command>` shim,
3. PATH.

So packages need no languages.toml edits, and users can always override.
helix-loader's grammar loading gains the same fallback into the pkg store.

### Async + UI

- `ops::*` functions are async, emit typed `PkgEvent::{Started, Progress
  {pct}, Log, Done, Failed}`; helix-term routes them through the existing
  runtime ingress → toast + statusline `progress_bar` (determinate when the
  download size is known).
- `:pkg` opens a picker (existing picker + multi-select plumbing): columns
  name/kind/installed/latest/languages; `space` marks, `enter` installs
  marked or selected; `u` update, `d` remove, `!` doctor. Hints via
  hint_bar.
- Missing-server nudge: when a document's language config references a
  command that resolution can't find but the registry can supply, show a
  dismissable statusline hint once per session; `:pkg install <name>`
  one-shot. `auto-install = true` makes it silent.
- CLI parity: `dhx pkg install|update|remove|list|search|outdated|sync|doctor|rollback`.

### Grammar fold-in

`hx --grammar fetch/build` remains as an alias; internally grammars become
`kind = "grammar"` packages whose source is the existing git+cc pipeline,
gaining receipts/locking for free. Prebuilt grammar artifacts per platform
can be added to the registry later without engine changes.

### Plugins

Plugins install from registry entries (`kind = "plugin"`, git or archive
source) into a pkg-managed plugin dir that is appended to `plugin_dirs`.
The existing manifest validation (min_api_version, capabilities) runs at
load exactly as today. Remote hosts: v1 documents `dhx pkg sync` run on the
remote side against the same lockfile; no editor-driven remote provisioning.

### Failure model

Every op is transactional per package: install into a temp dir in the
store, verify hashes, then activate; failures leave the previous activation
untouched. `doctor` re-verifies receipts against the store and re-runs
`--version` probes.

## Implementation waves

- **W-pkg-1**: helix-pkg crate: spec/registry/store/lock/resolve + backends
  github-release, archive, system; receipts; CLI `install/list/remove/sync/
  doctor`; command resolution seam in helix-term/loader; builtin registry
  seeded with ~15 flagship servers/adapters.
- **W-pkg-2**: npm/cargo/pip/go backends with shared runtimes; grammar
  fold-in; `update/outdated/search/rollback`; per-project manifests.
- **W-pkg-3**: editor UI (`:pkg` picker, statusline nudge, toasts/progress),
  registry expansion (~40 entries), book docs.

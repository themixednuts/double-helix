# dhx Package Manager — Design

Status: approved design, implementation staged (2026-07).
Owner: architecture; implemented by staged codex waves.

Implementation status (2026-07 W-pkg-4): `helix-pkg` engine, `dhx pkg` CLI,
editor `:pkg` UI, github-release, archive, system, npm, pip, cargo, go,
grammar git, native OS package-manager sources, first-class formatter/linter
metadata, install policy checks, and the plugin backend contract surface are
implemented.

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
   for **LSP servers, DAP adapters, formatters, linters, tree-sitter grammars,
   and plugins** (extensible to themes/queries later).
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
├── source     — install backends (github-release, archive, npm, cargo, pip, go, system, native, plugin)
├── store      — on-disk layout, receipts, junction/copy activation
├── lock       — manifest + lockfile round-trip
├── policy     — install policy checks before backend dispatch
├── resolve    — "language/tool → package → installed binary" resolution
└── ops        — install/update/remove/doctor orchestration (async, progress events)
```

### Package identity and kinds

```toml
# registry entry (registry/<kind>/<name>.toml in-repo; user registries are
# git repos/dirs with the same layout, merged with later-wins precedence)
name = "rust-analyzer"
kind = "lsp"                     # lsp | dap | formatter | linter | grammar | plugin
description = "Rust language server"
homepage = "https://rust-analyzer.github.io"
aliases = ["ra"]                 # optional search/discovery aliases
categories = ["language-server"] # optional extra discovery tags
languages = ["rust"]             # what resolve uses to suggest it

[version]
tag-source = "github:rust-lang/rust-analyzer"   # where `update` finds versions

# optional schema links used for discovery/validation later
[schemas]
lsp = "https://example.invalid/rust-analyzer-lsp-schema.json"

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
# source = { native = { winget = "Rustlang.rust-analyzer", brew = "rust-analyzer" } }
# source = { plugin = "corp-tool-cache", ref = "rust-analyzer" }
# grammars: source = { git = "https://github.com/tree-sitter/tree-sitter-rust", rev = "..." }
```

Design rulings:
- **Data-in-git registry** (like mason-registry / Zed extensions), not a
  service: auditable, pinnable, forkable. The builtin registry ships in the
  repo under `registry/`; `[pkg] registries = ["..."]` adds direct local
  registry dirs, and `[[pkg.registry-sources]]` adds named local or cached git
  registries.

```toml
[[pkg.registry-sources]]
name = "corp-tools"
git = "https://example.com/corp/helix-registry.git"
branch = "main"

[[pkg.registry-sources]]
name = "local-dev"
path = "E:/registries/local-dev"
```

Git registry sources are cached under `<data>/pkg/registries/<name>` and are
updated explicitly with `dhx pkg registry update [name]`. Startup merges cached
registries when present; missing caches are skipped until updated.
- **Per-OS artifact table** instead of install scripts. No arbitrary
  postinstall execution: `npm` runs with `--ignore-scripts`, archives are
  just unpacked, cargo/pip/go build in isolated roots. The only compiler
  invocation is the existing grammar cc path, folded in unchanged.
- **`system` backend** exists so the registry can also *describe* tools the
  user manages themselves; resolution treats a system match as satisfied.
- **Runtime backends are declarative**: npm runs `npm install --prefix <store>
  --ignore-scripts`; pip installs into an isolated venv per package version;
  cargo uses `cargo install --root <store> --locked --version <locked>`; go
  uses `GOBIN=<store> go install module@version`. Node/npm, Python, Cargo, and
  Go themselves are user prerequisites and are detected on PATH with actionable
  errors. The package manager does not install those runtimes.
- **Ordered artifact fallback** is literal registry file order among artifacts
  matching the host OS/arch. The first available backend wins. `system` is
  available only when the binary is already on PATH; `native` is available only
  when the configured manager is on PATH; plugin artifacts are available only
  after their backend is registered.

### Native OS package managers

`source = { native = { winget = "...", brew = "...", apt = "...", pacman = "...", dnf = "..." } }`
is a per-manager package ID map. Selection is host-specific: Windows uses
WinGet, macOS uses Homebrew, and Linux checks apt, dnf, then pacman. Native
managers install globally and may require elevation. `dhx pkg` never elevates
silently; it attempts the manager command directly and maps permission failures
to an actionable error containing the exact command the user can run, such as
`winget install --id LLVM.LLVM --exact` or `sudo apt install clangd`.

`[pkg] allow-native = true|false|prompt` controls whether native artifacts may
run. The default is `prompt`. Receipts for native installs record the manager,
manager package ID, and manager-reported version, but do not create shims:
resolution finds the binary on PATH like a system install. `remove` delegates to
the native manager using the same permission-error behavior. `doctor` re-queries
the manager. `outdated`/`update` query cheap manager metadata where practical;
otherwise they report the package as managed natively.

Verified builtin native IDs are intentionally sparse: `rust-analyzer` uses
WinGet `Rustlang.rust-analyzer` and Homebrew `rust-analyzer`; `clangd` uses
WinGet `LLVM.LLVM` and Homebrew `llvm`.

### Install policy

Policy is configured under `[pkg.policy]` in `config.toml` and is enforced in
`ops` before any backend install or plugin backend resolve/install call:

```toml
[pkg.policy]
run-scripts = false
allow-build = true
min-release-age-days = 0
allowed-backends = []
blocked-backends = []
allowed-sources = ["https://github.com/*"]
allowed-plugin-backends = []
```

`run-scripts = false` is the default; npm remains `--ignore-scripts`, and future
script-capable backends must check this key. `allow-build = false` rejects cargo,
go, and grammar git build backends, leaving prebuilt archives, native managers,
and system probes. Cargo `build.rs` can execute arbitrary code, so `allow-build`
is a trust decision even though it is compilation rather than a package script.

`min-release-age-days` refuses versions newer than the configured age when the
backend provides publish metadata. GitHub releases, npm, PyPI, and crates.io
provide timestamps. Native, system, direct archive, and other undated sources
skip the age check with a warning. `allowed-backends`/`blocked-backends` gate
backend names, and `allowed-sources` is a URL allowlist with `*` wildcards.

### Plugin package backends

Plugins can register package backends over the plugin contract capability
`PkgBackend`. The contract mirrors the package backend boundary: `probe`,
`resolve_version`, `install(staging_dir, progress)`, `remove`, and `doctor`.
Lua plugins expose this as:

```lua
helix.pkg.register_backend({
  name = "corp-tool-cache",
  probe = function() return true end,
  resolve = function(ref) return { version = "1.2.3" } end,
  install = function(staging_dir, progress) progress("copying") end,
  remove = function(ref) return true end,
  doctor = function(ref) return true end,
})
```

Registry entries reference a registered backend with
`source = { plugin = "corp-tool-cache", ref = "rust-analyzer" }`. Plugin
backends are never enabled by `run-scripts`; they always require an explicit
`[pkg.policy] allowed-plugin-backends = ["corp-tool-cache"]` entry.

### Store layout (data dir)

```
<data>/pkg/
  store/<kind>/<name>/<version>/...      # immutable install trees
  bin/<name>[.exe|.cmd]                  # activated shims (junction or thin launcher)
  receipts/<kind>-<name>.toml            # legacy receipt import source, preserved for compatibility
  runtimes/{node,py}/                    # shared npm prefix / venv, versioned
```

- Active package receipt records live in `<data-dir>/double-helix/state.sqlite3` in the
  `pkg_receipts` table. The first package-store open imports legacy TOML
  receipts transactionally and records the `pkg-receipts-toml-v1` marker; the
  TOML files are not deleted during that import.
- `dhx pkg lock --fetch-hashes` resolves the lockfile and downloads
  archive/github-release artifacts only long enough to compute sha256 pins;
  it writes the hash to the lock and discards the download without installing.
- Activation = writing a shim + receipt for store-managed packages; native
  packages write a receipt only. The store keeps versions side by side and
  records the previous active version so rollback reactivates the prior
  still-present tree (`dhx pkg rollback <name>`).
- Windows: junctions for directories, generated `.cmd`/copied exe shims for
  binaries — no symlink privilege required. Unix: symlinks.
- Archive downloads are hash-verified when the registry or lockfile supplies a
  sha256. Installs record the observed archive hash in receipts. Lock-only
  refreshes skip archive downloads unless `--fetch-hashes` is supplied.

### Manifest + lockfile

- `<config>/pkg.toml` — user intent: `lsp = ["rust-analyzer", ...]`,
  version ranges optional (default: track registry latest).
- Optional per-project `.helix/pkg.toml` merged on top (workspace-pinned
  toolchains). When a project manifest names a package, `.helix/pkg.lock` must
  pin that package; project pins take precedence over the user lock for the same
  package during `dhx pkg sync --project <dir>`. Use
  `dhx pkg lock --project <dir> [name]...` to refresh that project lock without
  installing anything.
- `<config>/pkg.lock` — exact resolved versions + hashes; `dhx pkg sync`
  reproduces the store from it on a new machine. Use `dhx pkg lock [name]...`
  to refresh the user lock from `<config>/pkg.toml` without installing.

### Resolution (the integration seam)

`resolve::binary(kind, name)` returns the activated shim path. Language
configuration keeps working untouched: when languages.toml specifies a bare
LSP, DAP, formatter, or linter command name, the lookup order becomes

1. explicit absolute path in config (always wins),
2. `<data>/pkg/bin/<command>` shim,
3. PATH.

So packages need no languages.toml edits, and users can always override.
helix-loader's grammar loading gains the same fallback into the pkg store.
Editor missing-server discovery and CLI commands load registries through the
same config path, including cached `[[pkg.registry-sources]]`.

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
- CLI parity: `dhx pkg install|update|update --plan|remove|list|search|outdated|lock|lock --project|sync|sync --project|doctor|rollback|registry list|registry update`.

### Grammar fold-in

`hx --grammar fetch/build` remains unchanged. `kind = "grammar"` packages now
drive the same helix-loader git+cc pipeline into the package store, gaining
receipts/locking. Grammar loading remains additive: legacy runtime grammar
paths are checked first, then the active package receipt's store path. Prebuilt
grammar artifacts per platform can be added to the registry later without
engine changes.

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
`--version` probes. It also checks registry sources, including missing git
registry caches that need `dhx pkg registry update`.

## Implementation waves

- **W-pkg-1**: helix-pkg crate: spec/registry/store/lock/resolve + backends
  github-release, archive, system; receipts; CLI `install/list/remove/sync/
  doctor`; command resolution seam in helix-term/loader; builtin registry
  seeded with ~15 flagship servers/adapters.
- **W-pkg-2**: npm/cargo/pip/go backends with isolated runtime roots; grammar
  fold-in; `update/outdated/search/rollback`; registry expansion.
- **W-pkg-3**: editor UI (`:pkg` picker, statusline nudge, toasts/progress),
  registry expansion (~40 entries), book docs.
- **W-pkg-4**: native OS package-manager backends, install policy engine, and
  plugin package-backend contract.

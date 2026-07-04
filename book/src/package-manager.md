# Package Manager

`dhx pkg` installs editor runtime tools into the Double Helix user data
directory. It supports LSP servers, DAP adapters, formatters, linters, and
package-managed grammars from the builtin registry using GitHub release assets,
direct archives, npm, pip, cargo, go, grammar git sources, system PATH probes,
native OS package managers, and explicitly allowed plugin backends.

```sh
dhx pkg search rust
dhx pkg install rust-analyzer marksman
dhx pkg outdated
dhx pkg update rust-analyzer
dhx pkg lock --fetch-hashes
dhx pkg rollback rust-analyzer
dhx pkg list
dhx pkg doctor
dhx pkg remove marksman
```

Installed tools are activated through shims under the package store's `bin`
directory. Receipts record the resolved version, source URL, archive hash, and
installed file hashes. `dhx pkg doctor` verifies those receipts and reports
corrupted or missing files. Active receipts are generated state stored in
`<data-dir>/double-helix/state.sqlite3`; `pkg.toml`, `pkg.lock`, registry specs,
shims, artifacts, runtime directories, and advisory lock files remain ordinary
files.

Updates install the new version side by side, activate it, update `pkg.lock`,
and keep the previous version available for `dhx pkg rollback <name>` while the
old store tree remains present.

## Manifests and Locks

User intent lives in `pkg.toml` under the Double Helix config directory:

```toml
lsp = ["rust-analyzer", "marksman"]
dap = ["codelldb"]
```

`dhx pkg install` updates `pkg.lock` with exact versions and hashes. On another
machine, copy the manifest and lockfile, then run:

```sh
dhx pkg sync
```

`sync` installs the exact locked artifacts. Network access is only needed when
the locked source is remote and not already available locally.

`dhx pkg lock --fetch-hashes` refreshes the lockfile without installing. For
archive and GitHub release packages, it downloads the artifact only long enough
to compute and write the archive sha256, then discards the download.

## Backends

- `github-release`: downloads a matching release asset and pins its hash.
- `archive`: downloads or reads a direct archive URL, including `file://` URLs.
- `npm`: uses system `node` and `npm`; installs with `npm install --prefix ...
  --ignore-scripts`.
- `pip`: uses system Python; creates an isolated venv for the package version.
- `cargo`: uses system Cargo; installs with `cargo install --root ... --locked`.
- `go`: uses system Go; installs with `GOBIN=... go install module@version`.
- `git` grammars: uses helix-loader's existing grammar fetch/build pipeline.
- `system`: verifies a command exists on `PATH` and records a receipt.
- `native`: delegates to WinGet, Homebrew, apt, dnf, or pacman and records the
  manager package as a global install.
- `plugin`: delegates to a plugin-registered backend allowed by policy.

Node/npm, Python, Cargo, and Go are prerequisites when installing packages that
use those backends; `dhx pkg` detects missing runtimes and reports the missing
tool. Registry entries are declarative and do not run arbitrary install scripts.
Artifacts are tried in registry order for the current OS and architecture; the
first available backend wins.

## Native Managers

Native package managers install globally and may need elevation. Double Helix
does not elevate silently. If a native manager reports a permission error, the
error includes the exact command to run yourself, for example:

```text
sudo apt install clangd
winget install --id LLVM.LLVM --exact
```

Enable or block native manager use with:

```toml
[pkg]
allow-native = "prompt" # "true", "false", or "prompt"
```

Native receipts record the manager, package ID, and reported version. Native
packages do not create shims; command resolution finds their binaries on `PATH`.

## Policy

Install policy lives under `[pkg.policy]`:

```toml
[pkg.policy]
run-scripts = false
allow-build = true
min-release-age-days = 0
blocked-backends = ["plugin"]
allowed-sources = ["https://github.com/*"]
allowed-plugin-backends = []
```

Policy is checked before a backend runs. `run-scripts = false` keeps npm
scripts disabled. `allow-build = false` rejects cargo, go, and grammar git
builds. `min-release-age-days` refuses fresh releases when publish timestamps
are available from GitHub, npm, PyPI, or crates.io; undated sources are skipped
with a warning.

Plugin package backends always require an explicit allowlist entry:

```toml
[pkg.policy]
allowed-plugin-backends = ["corp-tool-cache"]
```

## Editor UI

Inside the editor, `:pkg` opens the package manager overlay. It has Browse,
Installed, Updates, and Registries tabs; package rows show install/update/work
state, version, status, language chips, and a detail pane with source, receipt,
doctor, homepage, and schema metadata.

Use `1`-`4` or `[`/`]` to switch tabs, `/` to search, `lang:`, `kind:`, and
`cat:` prefixes to narrow results, `f` to cycle package kind filters, `Space`
to mark packages or accept update-plan rows, and `?` for the full key table.
`Enter` or `i` installs the marked or selected package, `u` updates the
selection or refreshes the Updates plan, `U` applies accepted update-plan rows,
`d` removes installed packages, `r` rolls back, `!` runs doctor, and `R` updates
the selected registry source. Direct commands are also available:

```text
:pkg-install rust-analyzer
:pkg-update rust-analyzer
:pkg-update
:pkg-sync
```

Package operations run in the background. Progress is delivered as editor
notifications and through the statusline spinner/progress slot. Closing the
picker or dropping the UI interest does not undo a package operation; installs
are transactional and either finish activation or leave the previous receipt in
place.

## Command Resolution

Bare language-server and debug-adapter commands are resolved in this order:

1. an explicit absolute path in `languages.toml`,
2. the active shim in `<data>/pkg/bin`,
3. `PATH`.

After `dhx pkg install rust-analyzer`, a language configuration that says
`command = "rust-analyzer"` starts using the package shim without further
configuration.

## Missing Servers

When a document uses a configured language server whose command is missing, the
editor checks the package registry. If the registry has an installable package
for the command or language, the statusline shows a one-time session hint such
as:

```text
rust-analyzer not installed - :pkg-install rust-analyzer
```

Packages that are `system`-only are not suggested when absent because the
package manager cannot install them. To install actionable missing servers
automatically, add this to `config.toml`:

```toml
[pkg]
auto-install = true
```

Additional registry directories can be merged after the builtin registry:

```toml
[pkg]
registries = ["C:/path/to/my-registry"]
```

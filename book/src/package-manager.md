# Package Manager

`dhx pkg` installs editor runtime tools into the Double Helix user data
directory. It supports LSP servers, DAP adapters, and package-managed grammars
from the builtin registry using GitHub release assets, direct archives, npm,
pip, cargo, go, grammar git sources, and system PATH probes.

```sh
dhx pkg search rust
dhx pkg install rust-analyzer marksman
dhx pkg outdated
dhx pkg update rust-analyzer
dhx pkg rollback rust-analyzer
dhx pkg list
dhx pkg doctor
dhx pkg remove marksman
```

Installed tools are activated through shims under the package store's `bin`
directory. Receipts record the resolved version, source URL, archive hash, and
installed file hashes. `dhx pkg doctor` verifies those receipts and reports
corrupted or missing files.

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

Node/npm, Python, Cargo, and Go are prerequisites when installing packages that
use those backends; `dhx pkg` detects missing runtimes and reports the missing
tool. Registry entries are declarative and do not run arbitrary install scripts.

## Editor UI

Inside the editor, `:pkg` opens the package picker. The picker shows the merged
registry and installed view with package name, kind, installed version, latest
source, and languages. Use `Space` to mark entries, `Enter` to install the
selected package, `u` to update, `d` to remove an installed package, and `!` to
run doctor. Direct commands are also available:

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

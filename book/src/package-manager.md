# Package Manager

`dhx pkg` installs editor runtime tools into the Double Helix user data
directory. Wave 1 supports LSP servers and DAP adapters from the builtin
registry using GitHub release assets, direct archives, and system PATH probes.

```sh
dhx pkg search rust
dhx pkg install rust-analyzer marksman
dhx pkg list
dhx pkg doctor
dhx pkg remove marksman
```

Installed tools are activated through shims under the package store's `bin`
directory. Receipts record the resolved version, source URL, archive hash, and
installed file hashes. `dhx pkg doctor` verifies those receipts and reports
corrupted or missing files.

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

Wave 1 supports:

- `github-release`: downloads a matching release asset and pins its hash.
- `archive`: downloads or reads a direct archive URL, including `file://` URLs.
- `system`: verifies a command exists on `PATH` and records a receipt.

npm, pip, cargo, and go package backends are planned for a later wave.


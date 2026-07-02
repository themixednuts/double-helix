# FFF Upstream Tracking

This directory vendors `fff-search`, the core crate from
`https://github.com/dmtrKovalenko/fff.nvim`.

## Current Mapping

- Upstream repo: `dmtrKovalenko/fff.nvim`
- Upstream crate path: `crates/fff-core`
- Vendored path: `vendor/fff-search`
- Current vendored crate version: `0.6.4`
- Latest nightly checked: `0.6.5-nightly.0f5ead1`

## Local Extension Contract

The vendored copy is not a pure upstream snapshot. Double Helix currently relies
on these local extension points:

- `FilePickerScanOptions` and `FilePickerOptions::scan` for preserving Helix file-picker ignore/hidden/depth/link semantics.
- `ContentOverlay`, `OwnedGrepMatch`, `OwnedGrepResult`, `FilePicker::grep_owned`, and `grep_bytes` for searching unsaved editor buffers without writing them to disk.

If upstream gains equivalent APIs, delete the local extension and migrate the
adapter. If upstream does not, keep the patch small and isolated so it can be
rebased cleanly.

## Drift Check

Run:

```sh
cargo xtask fff-upstream
```

To inspect a published nightly tag:

```sh
cargo xtask fff-upstream --ref 0.6.5-nightly.0f5ead1
```

For a scheduled/manual gate that fails when upstream drift exists:

```sh
cargo xtask fff-upstream --fail-on-drift
```

The check compares `vendor/fff-search/src` against upstream `crates/fff-core/src`
and reports which local extension symbols are absent upstream.

## Update Procedure

1. Run `cargo xtask fff-upstream --ref <tag-or-main>` and review the source drift.
2. Copy upstream `crates/fff-core` into `vendor/fff-search`.
3. Reapply only the local extension contract above, or replace it with upstream equivalents.
4. Run `cargo check -p helix-term --bin dhx --manifest-path E:/helix/helix-fork/Cargo.toml`.
5. Run `cargo test -p helix-term --manifest-path E:/helix/helix-fork/Cargo.toml --lib picker`.
6. Run `cargo clippy -p helix-term --lib --manifest-path E:/helix/helix-fork/Cargo.toml -- -D warnings`.


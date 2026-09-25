# Upstream Helix gaps

Checked 2026-09-25 against `upstream/master` (helix-editor/helix), from the
fork's merge-base `1a38979aa` (2026-02-18), 462 upstream commits.

The upstream bug fixes that still applied to the fork have been ported (see the
"Port upstream …" commits on `fix/cursor-explorer-upstream`). Two were
not taken: 06513deb6's injection-layer half (the fork already resolves comment
tokens from the document's root language) and f37bee941 (the fork's DAP
transport already turns `success: false` into an error).

What remains are features, keybindings and config options. Paths are the
fork's, where each would land.

## Keybindings and commands

| Upstream | What | Where in the fork | Effort |
|---|---|---|---|
| 3de3357ab (#15163) | `space .` opens the explorer in the buffer's directory (upstream dropped `space E`) | helix-term/src/keymap/default.rs:271 | S |
| 8f259df11 (#14932) | `:pushd` / `:popd` / `:show-directory-stack` | helix-term/src/commands/typed.rs; `dir_stack` next to `last_cwd` in helix-view/src/editor/core.rs | S |
| 98ad5dbf5 (#15410) | `%reg{x}` register expansion, with completion | helix-core/src/command_line.rs, helix-view/src/expansion.rs | S |
| 0475fdd97, 2d903f82b, 278b24389 | Workspace trust: `:workspace-trust` / `:workspace-untrust` / `:workspace-exclude`, trust prompt, `[⚠]` indicator. The fork loads `.helix/` config, LSP and DAP settings from any repo without asking (helix-term/src/config.rs:170-176) | new helix-loader/src/workspace_trust.rs plus gates | L |
| e42c1e7db | `:w --no-code-actions` | typed.rs next to `WRITE_NO_FORMAT_FLAG` (needs code actions on save) | S |
| 26a2d55de (#15548) | `hx --strict` for grammar fetch/build | helix-term/src/args.rs, helix-loader/src/grammar.rs | S |
| c1e14a891 (#15515) | `-g fetch/build` progress `(i/N)` | helix-loader/src/grammar.rs | S |

## Config options and behavior

| Upstream | What | Effort |
|---|---|---|
| 9a07a8267, 370787431 | `code-action-hint` gutter and statusline element | M |
| e42c1e7db, 9fc0e104a, 4ed0899b0, bc27d0113 | per-language `code-actions-on-save` | M-L |
| 9868e3ddf, d5afba926 | `[editor.lsp] auto-document-highlight` (the fork only has manual `space h`) | M |
| 6b2dccff1 (#11414) | `editor.mouse-yank-register` | S |
| c10406510 and follow-ups | Terminal background follows `ui.background` via OSC 11 (needs termina >= 0.2) | M |
| c3517470f (#13318) | Theme keys `{hint,info,warning,error}.diagnostic.inline` | S |
| e83e0d6b8 (#15235) | Underline LSP document links; `gf` over every link in the selection | S |
| 54ee9a486 (#12311) | Full-screen pickers below 200 columns | S |
| 5590a21a1, 416a0e098 | Continue-comment and join use the injection layer's comment tokens | M |
| d27856b04 | Injection-aware textobjects and `]f` / `[f` | S |
| 3013c107d and follow-ups | New indent engine (must move with ~40 `indents.scm` files) | L |
| 45c7c6a11 (#15010) | Auto-pair delete with selections; space inside `()` | M |
| dde092ca4 | `mm` matches Rust closure pipes | S |
| a85d92d95, 721dd13e2, aaf83db5f | tags `definition.enum` / `definition.field`, `@name` capture (needs tree-house 0.4) | S-M |
| 59f323140 | Infobox keeps the user keymap's order | S |
| 0673d4a2e, fe3b771df | Jumplist picker shows `file:line`; buffer picker styles the directory | S |
| b7a18f10d, 66528d335, ef64361fb, 59b2e2c82 | DAP progress events, capability-guarded `configurationDone`, breakpoint messages, thread state in the stack picker | S |
| 8cc80c5b2 | Linux clipboard provider order | S |
| ca2c4b512 | `call-hierarchy` language-server feature key (upstream configs using it fail to parse here) | S |

## Larger or deferred

- RFC 3986 file URIs (3d3aa794f): `[` and `]` in paths such as `app/[slug]/page.tsx` are not encoded. L.
- Non-blocking LSP shutdown (ad6a1b1bd). M.
- Upstream performance: word-index extraction in one pass (28de2c2de, d15ae8463), `FuturesUnordered` for multi-server requests (e7874bc69). The O(1) theme lookup and one-frame synchronized draws are already ported.
- 23 language servers upstream defines in `languages.toml` that the fork lacks (vtsls, deno, biome-lsp-proxy, zizmor, …), and upstream runtime queries that depend on the new indent engine.

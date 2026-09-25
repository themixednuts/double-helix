# Workspace trust

A workspace can carry configuration that runs code:

- `.double-helix/config.toml` and `.double-helix/languages.toml` can name language servers,
  formatters, debug adapters and grammar sources.
- A repository's `.git/config` can name filter programs that git integration runs.
- Debug adapters run whatever the language configuration says.

A freshly cloned repository or a checked-out pull request shouldn't get to do any of that just by
being opened, so these are gated behind trust granted per workspace. By default language servers
and debug adapters are allowed everywhere (their binaries come from your own configuration and
`$PATH`), while a workspace's own config and its `.git/config` need an explicit grant.

## Granting trust

Opening a file in a workspace where trust would change something (it has local config, or,
with `level = "none"`, a language server would start) asks once per session:

- **Trust**: trust the workspace from now on.
- **Never**: exclude the workspace and don't ask again.

`<Esc>` leaves the workspace untrusted for this session; the next session asks again.

While a workspace runs restricted in a way trust would change, `[⚠]` shows at the bottom right
of the editor, next to the macro recording indicator.

The same choices are commands:

| Command               | Effect                                                     |
| ---                   | ---                                                        |
| `:workspace-trust`    | Trust the current workspace, pinning its local config      |
| `:workspace-untrust`  | Forget the current workspace's grant or exclusion          |
| `:workspace-exclude`  | Never trust the current workspace, and don't ask again     |

Trusting or untrusting reloads the configuration, so local config and language servers apply
(or stop applying) right away. Language servers already running keep running after
`:workspace-untrust`; `:lsp-stop` stops them.

## Changes after trusting

Trusting a workspace records a hash of its `.double-helix/` directory, leaving out what the
editor writes there itself (`state/`, `transactions/`). If the configuration changes later
(a rebase, a pulled branch, a malicious checkout), the workspace becomes *stale*: its local
config isn't loaded and opening a file says so. Language servers still start, since their
binaries didn't change. Run `:workspace-trust` again to accept the new configuration.

Changes made while the editor runs are checked at the next `:config-reload`.

## Storage

Decisions live in the `workspace_trust` table of the state database, `state.sqlite3` in the data
directory (`%AppData%\double-helix\` on Windows, `~/.local/share/double-helix/` on Linux),
one row per workspace path.

## Configuration

Settings live under `[editor.workspace-trust]` in your user `config.toml`. A workspace's own
config can't change them.

| Key       | Values                              | Default     | Effect                                                    |
| ---       | ---                                 | ---         | ---                                                       |
| `level`   | `"none"`, `"servers"`, `"insecure"` | `"servers"` | What every workspace is trusted with, without a grant     |
| `prompt`  | `true`, `false`                     | `true`      | Ask when opening a restricted workspace. `[⚠]` shows either way |
| `trusted` | list of glob patterns               | `[]`        | Workspaces trusted without a grant (discouraged)          |

The levels:

- `"none"`: nothing is trusted implicitly. Language servers, debug adapters, local config and
  git config all wait for `:workspace-trust`.
- `"servers"`: language servers and debug adapters are trusted; local config and git config
  need a grant.
- `"insecure"`: everything is trusted, except excluded workspaces. This turns the protection
  off: a checked-out branch with a malicious `.double-helix/config.toml` gets its configuration
  loaded, with no prompt and no indicator.

For the strictest setup, trust each workspace by hand:

```toml
[editor.workspace-trust]
level = "none"
prompt = false
```

### Trusting by path (discouraged)

```toml
[editor.workspace-trust]
trusted = ["~/src/github.com/me/*"]
```

A workspace whose path matches is trusted as if you had run `:workspace-trust` there. `~` and
environment variables are expanded, `*` doesn't cross directories, and paths match
case-insensitively on Windows. This is weaker than a grant: local config changes are never
checked, and any repository that later lands under a matching path is trusted too. An explicit
`:workspace-exclude` still wins.

## Git

Untrusted workspaces open their repository with gix's `Trust::Reduced`: built-in conversions
such as `core.autocrlf` keep working, but settings from the repository's own `.git/config` that
run programs, like `filter.*.clean` and `filter.*.smudge`, are ignored. Trusted workspaces use
`Trust::Full`. The trust level comes from workspace trust, not from who owns the `.git`
directory.

## Grammars and the package manager

`--grammar fetch`/`build` and the package manager's list of configured tools read a workspace's
`languages.toml` only once it was explicitly trusted with `:workspace-trust`; configured implicit
trust doesn't count there, since grammar sources are cloned and compiled.

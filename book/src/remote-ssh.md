# Remote SSH

Double Helix can keep its terminal UI local while files, search, processes,
language servers, packages, and version-control operations run on an SSH host.
Open a remote workspace with an absolute path:

```sh
dhx --remote ssh://netcup3/home/jonfo
```

The host may be an alias from `~/.ssh/config`. A user and port can be included
when they are not supplied by SSH configuration:

```sh
dhx --remote ssh://jonfo@example.com:2222/home/jonfo/project
```

The local machine needs an OpenSSH-compatible `ssh` command. The remote host
must be a supported Linux or macOS target with a POSIX shell, `uname`, and
either `sha256sum` or `shasum`. Rust and `cargo-binstall` are not required on
the remote host.

## Server bootstrap

For a tagged build, the local client detects the remote platform and downloads
the matching `dhx-server` artifact over HTTPS. It verifies the release
archive against `SHA256SUMS`, accepts only the expected server executable from
the archive, and streams that executable over SSH. The remote install verifies
the executable again before an atomic install under:

```text
~/.cache/double-helix/server/<build-id>/dhx-server
```

Each connection checks the cached server's compiled identity and protocol
against the local client before starting it. A matching version and protocol
are accepted even when commit hashes drift or one build has no hash; the
client prints a warning and shares the cache entry for that version, protocol,
and platform. Remove `~/.cache/double-helix/server` to force a clean reinstall.

Developers testing an unpublished cross-platform build can provide a compatible
server executable through `DOUBLE_HELIX_SERVER`. The file is read on the local
machine and must target the remote platform; it is hashed and installed
through the same SSH verification path.

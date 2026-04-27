# Installing Helix

The typical way to install Helix is via [your operating system's package manager](./package-managers.md).

Note that:

- To get the latest nightly version of Helix, you need to
  [build from source](./building-from-source.md).

- To take full advantage of Helix, install the language servers for your
  preferred programming languages. See the
  [wiki](https://github.com/helix-editor/helix/wiki/Language-Server-Configurations)
  for instructions.

## Pre-built binaries

Download pre-built binaries from the [GitHub Releases page](https://github.com/themixednuts/double-helix/releases).
The tarball contents include a `dhx` binary and a `runtime` directory.
To set up Double Helix:

1. Add the `dhx` binary to your system's `$PATH` to allow it to be used from the command line.
2. Copy the `runtime` directory to a location that `dhx` searches for runtime files. A typical location on Linux/macOS is `~/.config/double-helix/runtime`.

To see the runtime directories that `dhx` searches, run `dhx --health`. If necessary, you can override the default runtime location by setting the `DOUBLE_HELIX_RUNTIME` environment variable.

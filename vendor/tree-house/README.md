# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.

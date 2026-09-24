---
title: Developers
description: Build Punktfunk from a fresh clone, find your way around the code, test a change and get it merged.
---

Build Punktfunk from a fresh clone, find your way around the code, and get a change merged. To
install and stream instead, start at the [Quick start](/docs/quickstart).

## First build

On Ubuntu 26.04 with [rustup](https://rustup.rs) and the
[workspace packages](/docs/developers/build-from-source#workspace-prerequisites):

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --workspace --locked
cargo test --workspace --locked
```

rustup installs the compiler version `rust-toolchain.toml` pins on the first build; don't override
it, or rustfmt reformats files you never touched. Then enable the git hooks:
`git config core.hooksPath scripts/git-hooks`.

## Pages

| Page | Read it when you |
|---|---|
| [Architecture](/docs/developers/architecture) | Need to know which crate does what, and how a frame and an input travel |
| [Build from source](/docs/developers/build-from-source) | Build the host, a client, the console, the docs site or the Windows drivers |
| [Testing](/docs/developers/testing) | Want to prove a change, or run a host from source against a client |
| [Contributing](/docs/developers/contributing) | Open a pull request: hooks, CI lanes, labels, generated files, docs rules |
| [Releasing](/docs/developers/releasing) | Cut a stable release (maintainers) |
| [Management API](/docs/developers/management-api) | Script the host over its REST API |
| [Writing plugins](/docs/developers/writing-plugins) | Add a library source or automation to the host |
| [Embedding](/docs/developers/embedding) | Link `punktfunk-core` into your own client |
| [Multi-seat contract](/docs/developers/multi-seat-contract) | Change how the Windows host shares one box between players |

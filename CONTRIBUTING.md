# Contributing to Punktfunk

Thanks for your interest in contributing! Building, testing and sending a change are in the
[developer docs](https://docs.punktfunk.unom.io/docs/developers). The short version:

- **Discuss features first.** Talk a new feature or design through with the maintainer, for
  example on [Discord](https://discord.gg/wzEGg9y45z), before you open an issue or a pull
  request. Security reports go to security@punktfunk.com ([SECURITY.md](SECURITY.md)).
- **Enable the hooks** once per clone: `git config core.hooksPath scripts/git-hooks`
  ([what they run](https://docs.punktfunk.unom.io/docs/developers/contributing#git-hooks)).
- **Run the full pass** before you push, plus the
  [checks for what you touched](https://docs.punktfunk.unom.io/docs/developers/testing):

  ```sh
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test  --workspace --locked
  ```

- **Open the pull request as a draft** (`WIP:` title prefix). CI lanes, the `ci:*` labels and the
  generated files CI checks:
  [Contributing](https://docs.punktfunk.unom.io/docs/developers/contributing#open-a-pull-request).
- **Update the docs page that owns a fact** in the same pull request:
  [where facts live](https://docs.punktfunk.unom.io/docs/developers/contributing#docs).
- **Commits, comments, error messages and the changelog** follow
  [docs/writing.md](docs/writing.md).

## Licensing of contributions (inbound = outbound)

Punktfunk is dual-licensed under **MIT OR Apache-2.0**.

> Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
> the work by you, as defined in the Apache-2.0 license, shall be dual licensed as **MIT OR
> Apache-2.0**, without any additional terms or conditions.

By opening a pull request you agree to license your contribution under these terms. This is the
standard Rust-ecosystem "inbound = outbound" model; it keeps the project's licensing unambiguous
(including the Apache-2.0 §5 contributor patent grant) and any future relicensing clean. You retain
the copyright to your contributions.

### Do not paste copyleft (or otherwise incompatibly-licensed) code

The single thing that could poison the permissive license is **copied source from a copyleft
project**. Several adjacent projects (Sunshine, Apollo, Moonlight) are GPL-3.0. You may study them
and reimplement a *technique*, protocol, or wire format — those are not copyrightable — but **never
paste their code**, and do not translate a GPL implementation line-by-line. When a comment credits
prior art, make clear it is an independent reimplementation, not a copy. The same applies to any
third party's code under a license incompatible with MIT/Apache.

If you add a new third-party dependency, it must be permissive (MIT / Apache-2.0 / BSD / ISC / Zlib /
Unicode-3.0 / etc.). `about.toml` holds the accepted-license allow-list; regenerate the attribution
file with `scripts/gen-third-party-notices.sh` when the dependency tree changes.

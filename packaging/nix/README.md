# punktfunk on NixOS / Nix

The repo's `flake.nix` builds the host and the Linux client reproducibly, ships a NixOS module that
wires up everything the RPM and deb do, and carries a dev shell with the pinned toolchain.

Using it — adding the cache, importing the module, enabling the host — is
[the docs site](https://docs.punktfunk.unom.io/docs/nixos)'s job, and every option is declared with
its own `description` in [`nixos-module.nix`](nixos-module.nix). This file is what neither of those
can hold: how the packages are built, and the traps.

> **Platform:** `x86_64-linux` only, NixOS 24.11+ for `hardware.graphics`.

## What the flake provides

| Output | Contents |
| --- | --- |
| `packages.…punktfunk-host` | `punktfunk-host` + `punktfunk-tray`, built with `nvenc` + `vulkan-encode` like CI |
| `packages.…punktfunk-client` | `punktfunk-client` (GTK4 shell) + `punktfunk-session` (without the Skia OSD — see below) |
| `packages.…punktfunk-web` | the management console (bun-built Nitro SSR bundle) |
| `packages.…punktfunk-scripting` | the plugin/script runner (bun-bundled Effect SDK) |
| `nixosModules.default` | `services.punktfunk.host` / `.client` / `.web` / `.scripting` |
| `devShells.…default` | pinned Rust from `rust-toolchain.toml` + every system library |
| `checks.…nixos-module` | evaluates the module against real nixpkgs and asserts on the rendered units |

One binary covers every GPU vendor: NVENC/CUDA entry points are `dlopen`'d at runtime, so the same
host runs on NVIDIA, AMD/Intel or software.

## Binary cache

CI publishes every punktfunk package to `https://nix.unom.io`. Without it, `nix build` here compiles
the whole Rust workspace *and* gamescope from source — about an hour. Only punktfunk's own store
paths are published; everything else is stock nixpkgs from `cache.nixos.org`, so adding the
substituter costs nothing on unrelated builds.

The version carries `+g<rev>`, so every commit is its own store path and the cache holds only the
commits CI published. Each channel is a branch that CI moves only after a publish succeeds, so
`nix flake update` never lands on an uncached commit:

| Flake input | Moves on |
| --- | --- |
| `git+https://git.unom.io/unom/punktfunk?ref=nix-stable` | each `v*` release tag |
| `git+https://git.unom.io/unom/punktfunk?ref=nix-canary` | nightly when `main` moved, or a dispatch of `nix.yml` on `main` |
| `…?ref=v<x.y.z>` | never; the tag is published too |

A bare URL follows `main`, which is almost never the published commit.

The cache serves its own public key at `https://nix.unom.io/punktfunk-cache.pub`, which is the
source of truth to check any pinned copy against.

**`nixpkgs.follows` turns the cache off.** Every store path is keyed by the exact inputs it was
built from, so pointing punktfunk's nixpkgs at yours makes every path miss. That is a real trade,
not a bug: `follows` buys one shared nixpkgs in the closure instead of two. Take it if closure size
matters more than build time.

**Why not cachix.** Punktfunk self-hosts every other channel, and a Nix cache is static files behind
a web server, so it rides the same box and deploy key as the rest. It speaks plain binary-cache
protocol, so any nix client works with it.

## Development

```sh
nix develop        # pinned toolchain + all system libs
cargo build --release -p punktfunk-host -p punktfunk-client-linux -p punktfunk-client-session
cargo build --release -p punktfunk-tray     # its OWN invocation — see below
```

The shell exports an `LD_LIBRARY_PATH` including `/run/opengl-driver/lib` so `cargo run` finds the
GPU driver. `nix fmt` formats the `.nix` files.

## Traps

- **The tray builds in its own derivation, on purpose.** `punktfunk-tray` uses `ksni`'s `async-io`
  zbus executor with no tokio runtime. Cargo unifies features across one `cargo build`, so
  co-building it with the host pulls the host's `ashpd → zbus/tokio` onto the tray's shared `zbus`,
  and the tray then panics at startup: *there is no reactor running*. A separate
  `-p punktfunk-tray` invocation keeps its `zbus` on async-io; the host package copies the binary
  into its `$out`. The rpm and Arch builds split it the same way — and the `.deb` did **not**,
  despite comments claiming otherwise, so this shipped as a real crash-at-launch on Debian/Ubuntu
  until 2026-07-27.

- **Build tool is [crane](https://github.com/ipetkov/crane).** The lockfile carries `windows 0.62.2`
  from both crates.io *and* a pinned `microsoft/windows-rs` git rev, which
  `rustPlatform.importCargoLock` cannot vendor (colliding `name-version`). Crane vendors per-source
  and fetches the git rev via `builtins.fetchGit`, so there is no output hash to maintain. Those
  crates are `cfg(windows)`-gated — vendored, never compiled on Linux.

- **First build compiles from scratch.** There is no split dep cache: `pyrowave-sys` builds a CMake
  tree in its `build.rs` that a crane "dummy" source would drop. `nix develop` gives incremental
  rebuilds.

- **`bun.nix` drifts, and the devDependency hook is not the guarantee.** The bun packages use
  [bun2nix](https://github.com/nix-community/bun2nix), fetching `node_modules` one `fetchurl` per
  package straight from the lockfile's integrity hashes via a generated-and-committed `bun.nix`.
  There is no aggregate deps hash to bump. But the regeneration hook fires only on a local
  `bun install` that runs lifecycle scripts — never under `--ignore-scripts` (which is every CI bun
  install), and never on a **merge or rebase**, where git carries someone else's `bun.lock` change
  past a `bun.nix` generated before it and reports no conflict. That is how `web/bun.nix` shipped on
  main holding `brace-expansion@5.0.7` against a lockfile saying `5.0.8` for **553 commits**, with
  `nix build .#punktfunk-web` broken the whole time, until an unrelated bump happened to close it by
  accident.

  The enforcement point is **`scripts/ci/check-bun-nix.sh`** (the `bun-nix` job in `ci.yml`,
  unfiltered so it sees the innocuous-looking commits drift arrives through). Fix any report with
  `scripts/ci/check-bun-nix.sh --fix`. **Never regenerate with a bare `bunx bun2nix`:** `bun.nix`
  has no schema stability across bun2nix versions, and an unpinned `bunx` uses whatever is newest.
  The flake input and the npm devDependency in `web/package.json` and `sdk/package.json` must name
  the same exact version — the script checks that too. Move all three together, then `--fix`.

- **`nix flake check` does NOT check the NixOS module.** For `nixosModules`, nix forces the value,
  asserts it is a lambda taking an open attribute set, and stops (its source still carries
  `// FIXME: if we have a 'nixpkgs' input, use it to check the module.`). Measured: a module setting
  a nonexistent option, referencing a nonexistent `pkgs` attribute **and** calling a nonexistent
  `lib` function passes clean, printing *all checks passed!*. So that line means nothing.
  [`module-check.nix`](module-check.nix) closes the gap by evaluating against real nixpkgs in four
  scenarios and asserting on the rendered systemd units. Two rules if you edit it: **keep every
  assertion pure Nix** (instantiating the derivation is what runs them, which is what lets the cheap
  `--no-build` CI leg cover it), and **assert list-valued unit fields on the evaluated lists**, not
  the rendered text — systemd renders `After=` as one space-separated line, so an `hasInfix` on it
  silently depends on ordering.

- **The session's Skia OSD is off under Nix.** `skia-safe`'s build *downloads* a prebuilt Skia,
  which the network-less build sandbox forbids, and a from-source build pulls the whole
  gn/ninja/python toolchain. The feature is explicitly droppable, so the Nix build compiles the
  session `--no-default-features --features pyrowave`. Everything streams; only the optional
  on-glass stats overlay and the console are absent. The GTK shell builds without its `console`
  feature too, so it shows no Console button, and this build does **not** install
  `io.unom.Punktfunk.Console.desktop`.

- **Commit `flake.lock`.** It pins nixpkgs, crane, rust-overlay and bun2nix.

CI runs three tiers (`nix.yml`): `nix flake check --no-build` evaluates every output including the
module check; the two bun packages are built for real on every PR; and a `v*` tag, the nightly or a
dispatch on `main` builds the Rust packages and `punktfunk-gamescope` and publishes them to the
cache. A `flake.lock` bump that breaks the gamescope patches therefore goes red within a day rather
than in an operator's rebuild.

## Cache infrastructure (maintainers)

`https://nix.unom.io` is a `caddy:2-alpine` container on unom-1 serving a static directory —
[`server/`](server/) — exactly like the flatpak repo and the winget source. A Nix binary cache *is*
just `nix-cache-info` + `<hash>.narinfo` + `nar/<hash>.nar.xz` behind a web server; there is no
cache daemon.

**Why not Gitea, and why not `storage.unom.io`.** Gitea has 23 package registry types and none is
Nix — not a missing label, but a path problem: the protocol needs fixed anonymous paths at a URL
*root*, and `/api/packages/{owner}/generic/{name}/{version}/{file}` cannot express them. The RustFS
S3 would work mechanically, but it is a local box on the home uplink with no CDN, so every user
download would compete with CI — and it answers **403** for a missing key unless the bucket policy
grants anonymous `ListBucket`, while nix treats anything other than **404** as a hard error rather
than a cache miss.

### One-time setup, in this order

The publish step ends by fetching `nix.unom.io` to prove the cache answers — and answers 404, not
403, for a path it does not hold — so stand the service up *before* setting the secrets that switch
publishing on. Until those exist the publish no-ops with a warning and `main` stays green.

1. **Ingress — both halves live in `unom/infra` and must move together.**
   `terraform/cloudflare/records.tf` owns the zone and `caddy/Caddyfile` owns the vhosts; that file
   says so itself: *"a name here with no vhost 404s, a vhost with no name here never cuts over."*
   Add `"nix"` to `local.hostnames` — it inherits `proxied = false`, which this service needs,
   because a proxy masking the origin's 404s would fail users' builds for every package the cache
   does not hold — and add a `reverse_proxy localhost:3250` block beside `docs.punktfunk.unom.io`.

   Apply with the **`dns-cutover.yml`** workflow (`action=plan` first — expect a single added
   `cloudflare_record.a["nix"]`, stop if it shows anything else) and `deploy-all` for the Caddyfile.
   **Neither by hand:** a dashboard record risks the duplicate-record round-robin `records.tf`
   documents, and `~/caddy/Caddyfile` on unom-1 is a copy `deploy-all.sh` rsyncs from the repo with
   no `.git` to warn you — a vhost added only on the box survives until the next deploy. This bit
   the winget source on 2026-07-26.

   Until both land the hostname fails the TLS handshake, because Caddy has no certificate for a name
   it does not serve. Diagnose by SNI, not port 80 — Caddy 308s every Host to https, including names
   it has never heard of, so a redirect proves nothing:

   ```sh
   openssl s_client -connect nix.unom.io:443 -servername nix.unom.io </dev/null 2>&1 \
     | grep -E '^subject=|alert'
   ```

2. Dispatch `deploy-services.yml` to bring the container up. It serves an empty cache — every path
   404s, which is what a healthy empty cache does.

3. **Signing key — already installed.** `NIX_CACHE_SIGNING_KEY` is a repo Actions secret. Regenerate
   only deliberately: a new key invalidates every signature already published, and every user
   pinning the old one starts failing.

4. **Deploy host key.** `DEPLOY_KNOWN_HOSTS` holds unom-1's SSH host key and the publish `ssh`es
   with `StrictHostKeyChecking=yes`. Every run starts with an empty `known_hosts`, so `accept-new`
   would make *every* run a first contact, handing `DEPLOY_SSH_KEY` and the signed publish to
   whatever won the race for the address. Key it **port-agnostically** — `ssh` looks a key up by the
   exact string it dialled, and `DEPLOY_PORT` is a secret nobody re-reads when re-keying:

   ```
   <DEPLOY_HOST>,[<DEPLOY_HOST>]:* ssh-ed25519 AAAA…
   ```

   Pin **ed25519 only**. Pinning every type `ssh-keyscan` prints lets a host offering just RSA
   satisfy the check on an RSA line, so the weakest pinned key decides.

5. Dispatch `nix.yml` on `main`. The publish also writes the public key to
   `https://nix.unom.io/punktfunk-cache.pub` and creates `nix-canary`. The next `v*` tag creates
   `nix-stable`; before that, create it by hand at a tag the cache holds. `REGISTRY_TOKEN` moves the
   channel branches, so no branch protection rule may cover `nix-*`.

`scripts/setup-nix-cache.sh` walks this interactively and each stage detects work already done.

### Operational notes

- Only punktfunk's own store paths are published. The publish asserts every built output matches
  that filter, so a future `pname` change fails the build instead of silently dropping a package.
- `rsync` runs **without** `--delete` so a client mid-download is never pulled out from under, and
  NARs upload *before* narinfos — a narinfo whose NAR has not landed is a hard failure for whoever
  fetches it in that window, while an unreferenced NAR is merely invisible.
- Growth is bounded by [`server/prune.sh`](server/prune.sh): it evicts narinfos untouched for 30
  days except the latest stable release's (`stable.narinfos`, written by the tag publish), then
  sweeps unreferenced NARs older than a day. Publishes can overlap, so a younger orphan may belong
  to one in flight. Self-check with `--self-test`.
- A user on a pinned rev older than the eviction window falls back to building from source, which is
  the pre-cache status quo.

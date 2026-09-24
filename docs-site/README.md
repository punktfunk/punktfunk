# punktfunk-docs

The Punktfunk documentation site: [Fumadocs](https://fumadocs.dev) on
[TanStack Start](https://tanstack.com/start) (Vite + Nitro/bun preset).

Content lives in [`content/docs/`](content/docs) as `.md`/`.mdx`. This site is the source of truth
for user-facing guides and the developer guide; design rationale lives in the internal
punktfunk-planning repo, and READMEs and the marketing site link here instead of restating
anything — see "Where facts live" in [CONTRIBUTING.md](../CONTRIBUTING.md). How to write a page:
[docs/writing.md §4c](../docs/writing.md#4c-docs-pages).

The sidebar has three tabs, one folder each with `"root": true` in its `meta.json`: `(guide)`,
`(reference)` and `developers`. A folder in parentheses is a **group**: it shapes the sidebar but
adds no URL segment, so `(guide)/(install)/ubuntu.mdx` serves `/docs/ubuntu`. Move a page between
groups freely; move it out of a group and its URL changes, so add the old slug to `moved` in
`src/routes/docs/$.tsx`.

## API reference

`/api` renders the host's **management REST API** as an interactive
[Scalar](https://github.com/scalar/scalar) reference (linked from the top nav, the docs
sidebar, and the landing page). It reads [`public/openapi.json`](public/openapi.json) — a
**snapshot** of the repo's generated spec. Refresh it after a management-API change:

```sh
# from the repo root — regenerate the spec, then copy the snapshot in:
cargo run -p punktfunk-host -- openapi > api/openapi.json
cp api/openapi.json docs-site/public/openapi.json
```

CI keeps the pair honest: the `docs-drift` job fails unless the snapshot is a byte-for-byte copy
of `api/openapi.json`, and the `rust` job regenerates the spec and diffs it against the committed
one — so a management-API change can't publish stale API docs any more, it fails CI until you run
the two commands above.

## Install commands and ports

`src/data/platforms.json` is a byte-identical snapshot of the repo-root
[`data/platforms.json`](../data/platforms.json) — the single source for install commands, repo
URLs, port facts and the Sunshine/Apollo/Vibeshine conflict facts. The `<Install platform="…" />`
and `<Ports />` MDX components (`src/components/platforms.tsx`) render from it, so no page restates
a command or a port. It's a snapshot for the same reason as `openapi.json` (the Docker build context
is this directory alone), and the same `docs-drift` job fails unless it matches:

```sh
cp data/platforms.json docs-site/src/data/platforms.json   # from the repo root, after editing the canonical file
```

## Develop

```sh
bun install
bun run dev        # http://localhost:3001  (docs at /docs)
```

CI gates every change on `bun run build` followed by `bun run lint` (the TypeScript typecheck), in
that order — the build emits the `.source` typegen the typecheck imports. Run both before you push.

## Build & serve

```sh
bun run build
bun run start      # serves .output/ via Bun
```

## Layout

```
source.config.ts          Fumadocs MDX collection (content/docs)
content/docs/             the docs content (.md/.mdx) + meta.json nav
src/
  routes/
    __root.tsx            RootProvider + html shell
    index.tsx            landing page
    docs/$.tsx           catch-all docs renderer (Fumadocs DocsLayout)
    api/index.tsx        Scalar API reference (reads public/openapi.json)
    api/search.ts        Orama search endpoint
  lib/source.ts          Fumadocs loader over the generated collection
  lib/layout.shared.tsx  shared nav chrome
  components/mdx.tsx      MDX component map
  styles/app.css          Tailwind 4 + Fumadocs preset
```

# punktfunk management console — TanStack Start built with Bun, served by the Nitro `bun`
# preset bundle. Build context is the REPO ROOT: orval reads ../api/openapi.json and Paraglide
# reads ../l10n/messages, both relative to web/:
#
#   docker build -f web/Dockerfile -t punktfunk-web .
#
# Runtime: PORT (default 47992) and PUNKTFUNK_MGMT_URL (upstream management API the Nitro
# server proxies /api to; see web/server/routes).
#
# TWO ports, not one. The console also listens on PUNKTFUNK_UI_PLUGIN_PORT (default PORT + 1 =
# 47993) and serves plugin UIs from there — a different origin, so a plugin's own code cannot act
# as the logged-in operator on the console's origin. Publish BOTH (`-p 47992:47992 -p
# 47993:47993`): the browser loads the frame from the second port directly, so a container that
# only publishes 47992 serves a console whose every plugin interface is an empty panel.
FROM oven/bun:1.4.4 AS build
WORKDIR /repo/web

# Dependency layer: lockfile only, so source edits don't re-install.
# --ignore-scripts: the root `prepare` script runs codegen, which needs sources that
# aren't copied yet — `bun run build` regenerates everything below.
COPY web/package.json web/bun.lock ./
# Retried, because a single failed tarball kills the whole install and takes the image build with
# it. Seen in CI as `error: Fail extracting tarball for "@rolldown/binding-linux-x64-musl"` — a
# 7.7 MB optional binding that bun fetches on any linux-x64 host (the lockfile records `os`/`cpu`
# but no libc, so the musl and glibc bindings are equally eligible) and that had arrived truncated.
# The lockfile is not at fault: `bun install --frozen-lockfile` accepts it, regenerating it with the
# same bun is byte-identical, and this exact layer builds clean for --platform linux/amd64.
# Two attempts with a pause, then fail for real — this recovers a truncated download and does NOT
# paper over a runner that is out of disk, which fails identically on every attempt.
RUN bun install --frozen-lockfile --ignore-scripts \
    || { echo "bun install failed — retrying once"; sleep 5; \
         bun install --frozen-lockfile --ignore-scripts; }

COPY api/openapi.json /repo/api/openapi.json
COPY l10n/messages /repo/l10n/messages
COPY web/ ./
# prebuild runs orval (openapi → src/api/gen); the paraglide vite plugin compiles i18n.
RUN bun run build

FROM oven/bun:1.4.4-slim
WORKDIR /app
COPY --from=build /repo/web/.output ./.output
USER bun
ENV PORT=47992
EXPOSE 47992 47993
CMD ["bun", "run", ".output/server/index.mjs"]

#!/bin/sh
# Internal-link checker (docs-and-onboarding-overhaul WP1). Two classes, both cheap and exact:
#
#   1. Docs-site pages linking other docs-site pages: every `](/docs/<slug>…)` markdown link and
#      `href="/docs/<slug>…"` attribute in docs-site/content must resolve to a content page.
#      Fumadocs 404s these at runtime only — a renamed page leaves silent dead links behind.
#   2. Relative file links in the repo's markdown (READMEs, CONTRIBUTING, docs/): the target file
#      must exist in the tree. Same failure mode: a moved file, a dead link, no CI signal.
#   3. `/docs/<slug>#<anchor>` links in the docs and in code must name a heading on that page
#      (check-docs-anchors.py).
#
# Deliberately NOT checked: external URLs (flaky third-party servers must not gate pushes) and
# site-absolute non-/docs paths like /api (routes in docs-site/src). Historical records —
# docs/releases/, CHANGELOG.md — are exempt from class 2: they describe the tree as it was.

set -u
LC_ALL=C
export LC_ALL
cd "$(dirname "$0")/../.." || exit 2

fail=0
tmp="${TMPDIR:-/tmp}/docs-links.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# ---------------------------------------------------------------- class 1: /docs/* page links
# A `(group)` folder adds no URL segment, so drop it from the path before matching.
git ls-files 'docs-site/content/docs' | sed -E 's#/\([^/]+\)##g' > "$tmp/pages"
git grep -ohE '\]\(/docs[^)]*\)|href="/docs[^"]*"' -- docs-site/content \
    | sed -e 's/^](\(.*\))$/\1/' -e 's/^href="\(.*\)"$/\1/' \
    | sed -e 's/[#?].*$//' | sort -u > "$tmp/links"
while IFS= read -r link; do
    slug=${link#/docs}
    slug=${slug#/}
    slug=${slug%/}
    if [ -z "$slug" ]; then
        target="docs-site/content/docs/index"
    else
        target="docs-site/content/docs/$slug"
    fi
    if ! grep -qxE "$target\.(md|mdx)|$target/index\.(md|mdx)" "$tmp/pages"; then
        echo "::error::dead docs link: $link (no $target.md/.mdx) — referenced from:"
        git grep -lF "$link" -- docs-site/content | sed 's/^/  /'
        fail=1
    fi
done < "$tmp/links"

# ---------------------------------------------------------------- class 2: relative file links
# Vendored trees are third-party docs describing their upstream repo, not ours (same exclusion
# as check-unsafe-hygiene.sh).
git ls-files '*.md' ':!docs-site/content' ':!docs/releases' ':!CHANGELOG.md' \
    ':!clients' ':!*/vendor/*' > "$tmp/mdfiles"
while IFS= read -r f; do
    dir=$(dirname "$f")
    # Markdown links whose target is a plain relative path: skip URLs (://), mailto:, pure
    # anchors, site-absolute paths, and anything with spaces or template syntax.
    grep -oE '\]\([^)]+\)' "$f" 2>/dev/null | sed 's/^](\(.*\))$/\1/' | sed 's/#.*$//' \
        | grep -vE '^$|://|^mailto:|^/|[ $]' | sort -u > "$tmp/rels" || true
    while IFS= read -r rel; do
        if [ ! -e "$dir/$rel" ]; then
            echo "::error::$f links $rel — no such file"
            fail=1
        fi
    done < "$tmp/rels"
done < "$tmp/mdfiles"

# ---------------------------------------------------------------- class 3: #anchors
# The console, the setup wizard and host log lines deep-link headings; a reworded heading
# breaks them silently.
python3 scripts/ci/check-docs-anchors.py || fail=1

exit "$fail"

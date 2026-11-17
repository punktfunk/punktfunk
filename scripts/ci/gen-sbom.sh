#!/bin/sh
# Generate the per-release CycloneDX SBOM (CRA Annex I Part II §1 — machine-readable component
# inventory, attached to every stable release by .gitea/workflows/sbom.yml).
#
# syft walks the checkout and catalogs every lockfile-pinned dependency (every Cargo.lock, the
# Bun/pnpm/npm trees, the Swift Package.resolved);
# compliance/sbom/manual-components.cdx.json contributes the components no lockfile records —
# vendored source trees, dynamically-linked or bundled libraries, and the patched gamescope.
# Keep that file current when vendoring changes (scripts/vendor-pyrowave.sh etc.).
#
# Usage: scripts/ci/gen-sbom.sh VERSION [OUTPUT]
# Requires: syft (pinned install in the workflow), python3 (a proven runner dependency).
set -eu
VERSION="${1:?usage: gen-sbom.sh VERSION [OUTPUT]}"
OUT="${2:-punktfunk-${VERSION}.cdx.json}"
TMP="${OUT}.syft.tmp"

syft scan "dir:." --source-name punktfunk --source-version "$VERSION" \
  -o "cyclonedx-json=$TMP" -q

OUT="$OUT" TMP="$TMP" python3 - <<'PY'
import json, os
gen = json.load(open(os.environ["TMP"]))
manual = json.load(open("compliance/sbom/manual-components.cdx.json"))
gen.setdefault("components", []).extend(manual["components"])
with open(os.environ["OUT"], "w") as f:
    json.dump(gen, f, indent=2)
print("SBOM: %d components -> %s" % (len(gen["components"]), os.environ["OUT"]))
PY
rm -f "$TMP"

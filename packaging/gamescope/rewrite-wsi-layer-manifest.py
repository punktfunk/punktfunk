#!/usr/bin/env python3
"""Rewrite gamescope's generated Vulkan layer manifest so OUR copy of the layer can be installed
beside the distro's instead of colliding with it.

A game nested under gamescope gets its HDR10 swapchain from the FROG WSI layer and from nothing
else, and that layer speaks `gamescope_swapchain` to the compositor: a layer built for a DIFFERENT
gamescope makes the compositor reject the client's swapchain_feedback, and every Vulkan client dies
on a black screen with sound and input and no error. So a compositor we ship needs the layer we
built beside it — which means two gamescope WSI layers on one box.

Three edits make that safe, and the loader is why:

* `name` — the Vulkan loader deduplicates implicit layers by name, and with both called
  VK_LAYER_FROG_gamescope_wsi which one wins is unspecified. A distinct name is what lets both sit
  installed at once.
* `library_path` — made absolute, so resolution never depends on where the loader found the
  manifest.
* `enable_environment` / `disable_environment` — our own gates, so the host can switch ours ON and
  the distro's OFF in the same session. Sharing ENABLE_GAMESCOPE_WSI would make that impossible.

Everything else is passed through untouched, `functions` above all: it names the layer's entry
points, and a manifest with the wrong ones is a layer that silently never loads.

Used by build-punktfunk-gamescope.sh (FHS packaging) and packaging/nix/gamescope.nix (the Nix store),
which is the point of it being a file rather than a heredoc — the two must not drift.

Usage: rewrite-wsi-layer-manifest.py <src.json> <dst.json> <installed-library-path>
"""

import json
import sys

LAYER_NAME = "VK_LAYER_PUNKTFUNK_gamescope_wsi"
ENABLE_VAR = "PUNKTFUNK_GAMESCOPE_WSI"
DISABLE_VAR = "PUNKTFUNK_GAMESCOPE_WSI_DISABLE"


def main(argv):
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    src, dst, lib = argv[1:4]

    with open(src) as f:
        manifest = json.load(f)

    layer = manifest.get("layer")
    if not isinstance(layer, dict):
        print(f"{src}: no 'layer' object — not a Vulkan layer manifest", file=sys.stderr)
        return 1
    # A manifest that never named the entry points would produce a layer that loads and does
    # nothing, which is indistinguishable on a running box from "this GPU has no HDR".
    if not layer.get("functions") and not layer.get("library_path"):
        print(f"{src}: neither 'functions' nor 'library_path' — refusing to rewrite", file=sys.stderr)
        return 1

    layer["name"] = LAYER_NAME
    layer["library_path"] = lib
    layer["enable_environment"] = {ENABLE_VAR: "1"}
    layer["disable_environment"] = {DISABLE_VAR: "1"}

    with open(dst, "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

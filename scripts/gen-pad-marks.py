#!/usr/bin/env python3
"""Console pad outlines -> the Apple clients' template vector imagesets.

The outlines live as 24-unit path data in crates/client/pf-console-ui/src/icons.rs (`PAD_*`, Kenney
Input Prompts 1.5, CC0). The Skia console strokes them and Android reads them over JNI; the
Apple clients need a vector PDF, stroked at the console's 1.5 weight, in PadMarks.xcassets.

Idempotent. Usage: python3 scripts/gen-pad-marks.py   (needs rsvg-convert)
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
ICONS = ROOT / "crates/client/pf-console-ui/src/icons.rs"
OUT = ROOT / "clients/apple/Sources/PunktfunkKit/Resources/PadMarks.xcassets"

# The types the Controller type quick action steps through: asset name -> `icons.rs` const.
MARKS = {
    "pad-xbox360": "PAD_XBOX_360",
    "pad-xboxone": "PAD_XBOX_ONE",
    "pad-dualsense": "PAD_DUALSENSE",
    "pad-dualshock4": "PAD_DUALSHOCK_4",
    "pad-steamdeck": "PAD_STEAM_DECK",
}

SVG = (
    '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24">'
    '<path d="{d}" fill="none" stroke="#000000" stroke-width="1.5" '
    'stroke-linecap="round" stroke-linejoin="round"/></svg>\n'
)
INFO = {"author": "xcode", "version": 1}


def path_data(src: str, const: str) -> str:
    m = re.search(rf'pub const {const}: Icon = Icon\(\s*"([^"]+)",?\s*\);', src)
    if not m:
        sys.exit(f"{const}: not found in {ICONS}")
    return m.group(1)


def main() -> None:
    src = ICONS.read_text()
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "Contents.json").write_text(json.dumps({"info": INFO}, indent=2) + "\n")
    with tempfile.TemporaryDirectory() as tmp:
        for name, const in MARKS.items():
            svg = pathlib.Path(tmp) / f"{name}.svg"
            svg.write_text(SVG.format(d=path_data(src, const)))
            imageset = OUT / f"{name}.imageset"
            imageset.mkdir(exist_ok=True)
            subprocess.run(
                ["rsvg-convert", "-f", "pdf", "-o", str(imageset / f"{name}.pdf"), str(svg)],
                check=True,
            )
            contents = {
                "images": [{"filename": f"{name}.pdf", "idiom": "universal"}],
                "info": INFO,
                "properties": {
                    "preserves-vector-representation": True,
                    "template-rendering-intent": "template",
                },
            }
            (imageset / "Contents.json").write_text(json.dumps(contents, indent=2) + "\n")
            print(name)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Check every App Store copy block in this directory against its field limit.

App Store Connect silently truncates or hard-rejects over-long fields, and the German copy is the
easy one to get wrong because umlauts read as one character but two bytes. Apple counts characters,
so `len()` on a `str` is the right measure — do not switch this to a byte count.

Each fenced code block in the .md files here is one field. Which limit applies is inferred from the
nearest heading above it, else from the block's length (kind_of). Exit status is non-zero if a block
is over its limit or a heading's count has drifted, so CI can gate on it.
"""

from __future__ import annotations

import pathlib
import re
import sys

LIMITS = {"PROMO": 170, "DESC": 4000, "KW": 100, "NOTES": 4000}


def blocks(text: str):
    """Yield (heading, body) for every fenced block, tagged with the heading above it."""
    heading = None
    buf: list[str] | None = None
    for line in text.split("\n"):
        if line.startswith("#") and buf is None:
            heading = line.lstrip("#").strip()
        if line.strip() == "```":
            if buf is None:
                buf = []
            else:
                yield heading or "", "\n".join(buf)
                buf = None
            continue
        if buf is not None:
            buf.append(line)


def kind_of(heading: str, body: str) -> str:
    low = heading.lower()
    if "keyword" in low or re.fullmatch(r"(de|en) \(\d+\)", low):
        return "KW"
    if "template" in low:
        return "NOTES"
    return "DESC" if len(body) > 400 else "PROMO"


def main() -> int:
    here = pathlib.Path(__file__).parent
    failures = 0
    stale = 0
    for path in sorted(here.glob("*.md")):
        found = list(blocks(path.read_text(encoding="utf-8")))
        if not found:
            continue
        print(f"\n=== {path.name} ===")
        for heading, body in found:
            kind = kind_of(heading, body)
            limit = LIMITS[kind]
            n = len(body)
            over = n > limit
            failures += over
            # Headings carry the count in parentheses; flag any that drifted from the real length.
            claimed = re.search(r"\((\d+)\)\s*$", heading)
            drift = ""
            if claimed and int(claimed.group(1)) != n:
                drift = f"   [heading claims {claimed.group(1)}]"
                stale += 1
            status = "OVER" if over else "ok"
            print(f"  [{kind:5}] {status:>4} {n:>4}/{limit}  {heading[:48]}{drift}")

    if failures:
        print(f"\n{failures} block(s) OVER the limit")
    elif stale:
        print(f"\nAll within limits, but {stale} heading count(s) are stale")
    else:
        print("\nAll blocks within limits, all heading counts accurate")
    return 1 if failures or stale else 0


if __name__ == "__main__":
    sys.exit(main())

# Launcher icon masters

The brand marks a **launcher tile** draws — the entries a library plugin publishes with
`role: "launcher"` (design D4), which open Steam Big Picture or Heroic or Playnite rather
than a game. One file per **icon token**, the value a plugin puts in an entry's `icon`
field and every client resolves against the set it ships.

| token | mark | emitted by | source |
|---|---|---|---|
| `steam` | Steam | punktfunk-plugin-steam (Big Picture + desktop) | Font Awesome Free brands (CC BY 4.0) |
| `lutris` | Lutris | punktfunk-plugin-lutris | Simple Icons (CC0 1.0) |
| `heroic` | Heroic Games Launcher | punktfunk-plugin-heroic | Simple Icons (CC0 1.0, slug `heroicgameslauncher`) |
| `playnite` | Playnite | punktfunk-plugin-playnite | JosefNemec/Playnite (MIT) |
| `epic` | Epic Games | punktfunk-plugin-epic | Simple Icons (CC0 1.0, slug `epicgames`) |
| `gog` | GOG.com | punktfunk-plugin-gog | Simple Icons (CC0 1.0, slug `gogdotcom`) |
| `xbox` | Xbox | punktfunk-plugin-xbox | Font Awesome Free brands (CC BY 4.0) |
| `hydra` | Hydra Launcher | punktfunk-plugin-hydra | hydralauncher/hydra (MIT) |

The Epic, GOG and Xbox tiles need a Windows host that knows their `launcher_ui` values; an
older host drops the tile and still syncs the games.

`steam` is the same mark as `assets/os-icons/steam.svg`, generated from that file rather than
re-sourced, so the SteamOS host badge and the Steam launcher tile can never drift apart.

## Why a token and not the icon itself

A plugin sends the **name** of a mark, never its bytes, and never a URL.

The obvious alternative — a plugin ships its own `icon.svg` and the host's art proxy serves it —
is closed by construction, and deliberately: `local_art_bytes` serves what the bytes *are*
(`sniff_image_type`, `crates/punktfunk-host/src/library/art.rs`), and SVG is not on that list
because it is script-capable XML and the web console renders library art in a browser. Widening
that sniff to admit SVG would trade a rendering nicety for a stored-XSS surface.

Sending a token instead keeps that refusal intact and buys three things a proxied image could
not have given us anyway: the glyph stays vector at every tile size a client picks, it takes the
tile's own ink instead of arriving pre-coloured, and it costs no fetch, no cache and no bytes on
a reconcile that is already body-limited.

The cost is that a **third-party** plugin cannot ship a mark no client bundles. Its tile falls
back to the launcher's name on an accent face — exactly what every launcher tile looked like
before this existed — and the fix is a pull request adding the master here.

All files are monochrome (`fill="currentColor"`), original per-icon viewBoxes preserved. Those
viewBoxes are not all square (`0 0 24 24`, `0 0 496 512`, `0 0 1024 1024`), so **a client must
letterbox rather than stretch** — a mark drawn to a square box is a squashed mark.

## Regenerating the per-client derivatives

`bash scripts/gen-launcher-icons.sh [token ...]` turns a master into the three baked forms (GTK
symbolic SVG, Windows PNG, Apple template PDF) and prints the path data for the three clients
that inline it (web console, Android, the in-session console UI). Adding a **new** token also
means adding it to each client's shipped-token list — the script prints that checklist too.

## Licensing

Attribution notices live in `LICENSES/` and are folded into `THIRD-PARTY-NOTICES.txt` by
`scripts/gen-third-party-notices.py`. The marks are trademarks of their respective owners; they
are used here nominatively — to *identify* the launcher a tile opens, the standard practice in
this ecosystem — and imply no affiliation or endorsement.

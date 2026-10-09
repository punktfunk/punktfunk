# Lucide icon masters

The canonical UI marks every client draws its own icons from — the quick-action ring's slots,
and the ordinary shell chrome (back, refresh, save, delete…) on the desktop clients.

[Lucide](https://lucide.dev) **v0.462.0**, ISC licensed (see `THIRD-PARTY-NOTICES.txt`), fetched
unmodified from `lucide-icons/lucide` at that tag. One file per icon, its own name. Every master
is a 24×24 `viewBox`, `fill="none"`, `stroke="currentColor"`, `stroke-width="2"`, round caps and
joins — Lucide's own drawing contract, and what every derivative below reproduces.

## Which client consumes what

`scripts/gen-lucide-assets.sh` derives all of it. Nothing here is hand-edited.

| client | form | where |
|---|---|---|
| Skia console (gamepad UI) | folded path string, stroked by Skia | `crates/client/pf-client-core/src/lucide.rs` → `crates/client/pf-console-ui/src/icons.rs` |
| GTK shell | the same path string, stroked by `gsk::Path` | `crates/client/pf-client-core/src/lucide.rs` |
| WinUI shell | the same table's font codepoint, drawn from `font/lucide.ttf` | `clients/windows/packaging/assets/lucide.ttf` |
| webOS pointer UI | the console's `icons` module, stroked by Skia | `crates/client/pf-console-ui/src/icons.rs` |

Every consumer reads **one** table, so a mark cannot differ between shells. The WinUI shell draws
the icon font because windows-reactor has no vector element; a `FontIcon` is sized by the control
and tinted by the theme, which a baked bitmap could not be.

## Adding an icon

1. Drop the master here: `curl -o assets/lucide/<name>.svg
   https://raw.githubusercontent.com/lucide-icons/lucide/0.462.0/icons/<name>.svg`
2. `bash scripts/gen-lucide-assets.sh`
3. Use the name. Every shell reads the regenerated table; the WinUI shell's `every_name_ships`
   test lists the names it asks for, so add a new one there when that shell uses it.

The console's `icons.rs` and the GTK shell need no list of their own: both read
`pf_client_core::lucide`, which the script regenerates whole.

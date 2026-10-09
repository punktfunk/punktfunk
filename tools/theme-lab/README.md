# Palette lab

Edit the console's backdrop palettes on the field shader the console draws, and read the
legibility checks while the field moves. The page exports each palette as a `PALETTES` entry for
`crates/client/pf-console-ui/src/palette.rs`.

```sh
cargo test -p pf-console-ui --lib dump_theme_lab -- --ignored   # refresh lab-data.json
python3 -m http.server -d tools/theme-lab                       # then open http://localhost:8000
```

Colours are picked in OKLCH, the space the field blends in: a lightness × chroma plane at the
chosen hue, with the area outside sRGB hatched. Values can also be typed as hex or as the 0–1
tuple `palette.rs` uses.

`lab-data.json` holds the palette table, the inks `theme::Ink::of` picks from, and the field
shader around its gradient. Refresh it after changing any of them. The page says so when the shader
it rebuilds no longer matches the Rust one.

Two sets of checks:

- **On the field** samples a rendered frame in the launcher and the settings form: text on glass
  (4.5:1), titles on the bare field (3:1), brightness against the ink, and the hue range in view.
- **What CI checks today** runs the palette tests in `palette.rs`, which still sample the retired
  colour mesh.

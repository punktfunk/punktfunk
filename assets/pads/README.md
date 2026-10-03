# Controller drawings

One SVG master per controller, named by its `GamepadPref` name (`xboxone.svg`). The console's
input test and the web console's Controllers page both draw from these.
`python3 scripts/gen_pad_art.py` writes their tables; never edit those by hand.

## Format

- Front view, in millimetres, `viewBox="0 0 W H"`. Triggers peek above the top edge.
- `<title>` is the controller's name.
- Paint order is document order: triggers and rear buttons first, then the body over them.
- No transforms, no `<style>`, no colours. The renderers own the look; a master is geometry.
- Every shape carries a `class`:

| class | element | meaning |
| --- | --- | --- |
| `body` | any shape | The shell. Filled, with a rim. |
| `panel` | any shape | A recess or a second material, darker than the shell. |
| `line` | any shape | A seam or outline, stroked as a hairline. |
| `button` | any shape, `id` | Lights while `id` is held. No `id`: drawn, never lit. |
| `trigger` | any shape, `id` | `LT` / `RT`. Fills from its top edge as it is pulled. |
| `stick` | `<circle>`, `id` | `LS` / `RS` well. The renderer draws the cap and its travel. |
| `pad` | `<circle>`, `id` | A trackpad that reports as a stick: a dot follows the axes. |
| `glyph` | any shape, `data-on`, `stroke-width` | Stroked ink on control `data-on`. |
| `mark` | any shape, `data-on` | Filled ink on control `data-on`. |
| `label` | `<text>`, `data-on`, `font-size` | Text centred on `x`, `y`. |

## Ids

Face buttons by position: `A` south, `B` east, `X` west, `Y` north. A label carries the
engraving, so a Switch's south button is `id="A"` with the label `B`.

`Up` `Down` `Left` `Right` `LB` `RB` `LT` `RT` `LS` `RS` `Back` `Start` `Guide` `Misc`
`Touchpad`, and the rear buttons by the Deck's names: `L4` `R4` upper, `L5` `R5` lower.

## References

Drawn here. Proportions were measured from these; no image or model data ships.

| master | measured from |
| --- | --- |
| `xboxone` | Commons "Xbox Series Controller Carbon Black.jpg"; controls from Microsoft's Elite Series 2 render |
| `xboxelite` | Microsoft's xbox.com front render |
| `xbox360` | Commons "Xbox 360 controller top (3455250267).jpg", dimensions.com drawing |
| `dualsense` | Commons "Playstation 5 DualSense controller in Midnight Black, 2026-02-07 (front).jpg" |
| `dualsenseedge` | Commons "DualSense Edge Controller.jpg", Sony's part-name diagrams |
| `dualshock4` | Commons "Dualshock 4 Layout.svg" |
| `switchpro`, `switch2pro`, `switch2gamecube` | Nintendo's product renders |
| `joyconpair` | Commons "Nintendo Switch Joy-Con Controllers.png" |
| `8bitdoultimate2`, `8bitdopro2`, `8bitdopro3` | 8BitDo's product renders |
| `horipadsteam` | HORI's product renders and manual (HPC-055) |
| `steamdeck`, `steamcontroller`, `steamcontroller2` | Valve's published CAD drawings (CC BY-NC-SA 4.0) |

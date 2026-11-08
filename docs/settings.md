# Client settings

The settings screen stays small. A setting is permanent surface: every client renders it, presets
and sync carry it, the docs explain it, and every later support thread asks whether it was set.
Add one only when it clearly makes streaming better for many players. This applies to the player
settings of every client — Android, Apple, desktop, the console shell, webOS and the browser.

## Before adding a setting

Answer these in order. The first "yes" ends the search for a setting.

1. **Can it just work?** Detect, measure, or pick the right default. A setting is not a way to skip
   a decision.
2. **Does the platform already say it?** Follow the OS: display scale, text size, theme, reduced
   motion. Do not add an app copy of a system setting.
3. **Is it a fix for one device, one link, or one report?** Fix the detection, or put it in logs
   or an operator env var. A per-device workaround is not a player setting.
4. **Can an existing control absorb it?** Reshape a control so it covers more players with no
   more choices, rather than adding a second one beside it.

A new setting also has to pass this: a player who does not know how streaming works reads the
label and its options and knows what to pick. If it needs a paragraph, it is not a player setting.

## Rules

- **Automatic means automatic.** No "Automatic, but…" modes and no tuning under an adaptive
  control. Bitrate is Automatic or a fixed rate.
- **Never change what the player picked.** Resolution and refresh stay as chosen; adapting moves
  only what Automatic owns.
- **No hidden state across sessions.** Behaviour that silently depends on an earlier session is
  a setting the player cannot see.
- **Operator and diagnostic knobs are not player settings.** They are `PUNKTFUNK_*` env vars or
  host settings.
- **One setting, every client that has the feature.** Shared key and wording (`docs/writing.md`
  §4b). A setting that exists on one client only needs a reason that platform alone has.
- **Ask first.** A PR that adds a player setting names the problem, who hits it, and what was
  tried to make it automatic — and has the maintainer's yes before it is opened.

## Advanced settings

Every client hides its advanced rows until **Show advanced** is on (on General). A
row is advanced when most players never change it and its default is right for them: decoder,
V-Sync, audio quality, controller forwarding, stats position and size. The video codec is not:
players pick PyroWave there. The tier is not an escape from the questions above; an advanced
setting is still a setting.

- **The catalogue is the list.** `clients/shared/settings-catalog.json` holds each shared
  setting's structure (below), never its words. Change it first; every client's test reads it.
- **A hidden value still applies.** A category with advanced values off their default shows
  "N advanced settings changed", which turns Show advanced on.
- **A preset shows what it overrides.** An advanced row a preset overrides stays visible in that
  preset's scope, whatever the switch says.
- **Show advanced is this device's.** It never rides a preset.

## The catalogue

One entry per shared setting. It keeps structure and loses words:

```json
{ "key": "stats_scale_pct", "category": "general", "advanced": true,
  "options": ["75", "100", "125", "150", "175", "200"], "default": "100",
  "preset": true, "on": ["desktop", "android", "apple", "webos"] }
```

- `key`, `category`, `advanced`: the stored key, its page, and whether it waits behind Show
  advanced.
- `options`: the stored value of each choice, in the order screens list them. Left out for a
  switch and for a control that is not a list: bitrate, resolution, PyroWave quality, the palette.
- `default`: what a fresh install stores, a switch as `true` or `false`, the rest as a string.
  Left out where platforms disagree; those are open rows below.
- `preset`: whether a preset may hold the setting.
- `on`: where a player finds a row. `desktop` is the GTK and WinUI dialogs and the desktop
  session's console; the others are `android`, `apple`, `webos`, `web`, `tizen`, `ps5`.

The words are messages in `l10n/messages/client`: `player_<key>`, `player_<key>_hint`,
`player_<key>_<option>`. V-Sync, a brand, has no label message.

Each client's test reads the file and pins its screen to it: a row for every entry that names
the platform, in its category and tier, worded by the `player_<key>` message, with the options in
order, the default and the preset tier; and no row for a setting that leaves the platform out.
The console kit, the Linux dialog and the Windows pages test it in Rust, Android in Kotlin,
Apple in Swift. Android and Apple read their screens' source, since those are not tables.

A difference a test names is listed under Open rows. When a screen stops differing, its test
fails until the exception goes. The browser client is another repository (`client-web`); its
console rows are the kit's `web` rows, pinned here, and a settings screen of its own reads this
file in its own test.

### Per-controller rows

Some settings belong to one physical controller, not to the client. Their rows sit on the pad's
card in the Controllers screen, and the value is stored under the pad's key in `Settings::pads`.
The catalogue lists them in a second array, `per_controller`, beside `settings`; each entry has
a `key`, `category`, `default` and `on`, and its words are `player_<key>` and `player_<key>_hint`.
The first is Speaker volume (`pad_speaker_volume`), shown while the client plays the pad's audio.
The console kit pins it in `pad_menu.rs`. The Linux and Windows dialogs do not draw it yet.

## Open rows

Where screens differ today. Each is the maintainer's call, one row at a time; the tests hold
today's state and change with the screen.

| Setting | What differs |
|---|---|
| `gamepad` | Linux, Windows and Android list `dualsense` before `xboxone`; Android and Apple have no Steam Controller 2; webOS and PS5 offer four of seven |
| `codec` | Apple lists `av1` before `h264`; webOS has no PyroWave, Tizen only `auto`, `hevc`, `h264`, PS5 no AV1 |
| `audio_format` | console, Linux, Windows: Opus and two lossless rates; Android, Apple: Opus and five |
| `decoder` | Linux offers VAAPI, Windows Direct3D 11 |
| `refresh_hz` | Apple lists only the rates the display reaches |
| `bitrate_kbps` | a number box (Linux, Windows), a list with Automatic and Custom (console, Android, Apple TV), a switch and a slider (Mac, iPhone, iPad); the rungs differ too |
| `hud_placement` | default top left on desktop, Android and PS5, top right elsewhere |
| `mouse_mode` | default `desktop` on Android, `capture` elsewhere |
| `vsync` | default off on Apple, on elsewhere |
| `pad_speaker` | default off on Android, on elsewhere |
| `reduce_ui_resolution` | default on for TVs, off elsewhere |
| `library_view` | unset reads as grid in the console, as shelf on Apple's own screen |
| `follow_os_theme` | only the desktop session publishes a theme; Windows has no row, the console offers it everywhere |
| `gamepad_ui_enabled`, `gamepad_ui_mode` | General on Android, Controllers on Apple, absent from the desktop console |
| `forward_pad` | Apple has a row, and keeps its own pad id, a different grammar |
| `ui_palette` | Apple TV edits it under Controllers |
| `ui_palette`, `reduce_motion`, `library_view`, `library_sections`, `host_sort`, `host_grouping`, `reduce_ui_resolution` | the console draws them; the Linux and Windows dialogs have none of the first six, Android's screen none of the seven, Apple's none of `library_sections`, `host_sort`, `host_grouping` |
| `fullscreen_on_stream` | a three-way picker on the Mac and in the Linux dialog's defaults, a switch elsewhere |
| `overlay_actions` | Windows has a page of its own; the console's preset editor has no row |
| `second_screen`, `low_latency` | Android only; the console's preset editor has no row |
| `pad_speaker_volume` | only the console draws the pad card; the desktop dialogs have no per-controller rows |

## Examples

Good — **Aspect ratio + Resolution** (#1088). One extra field turned a single oversized list into
two short ones and covers every panel shape.

Bad — **Automatic with a limit** (#1175) added a third bitrate mode to understand; **per-host ABR
memory** (#1176) started good links at 2 Mbps from a quiet last session. Both reverted in #1207.
**Stepping the mode down** under Automatic (#1177) overrode the player's choice; closed.

Bad — **an overlay size picker** (#530) duplicated the OS display scale on four settings screens.
The picker was taken out before merge; the overlay follows the OS scale. **Statistics size** is
the agreed exception: an advanced multiplier on top of the OS scale, for the overlay only.

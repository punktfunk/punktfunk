#!/usr/bin/env bash
# App Store screenshot driver for the Punktfunk Apple client.
#
# Launches the app in "shot mode" (PUNKTFUNK_SHOT_SCENE=<name> → one mock-populated screen,
# full-bleed; see Sources/PunktfunkClient/Screenshots/) once per scene per device, and lets the OS
# capture the REAL rendered UI:
#   • macOS  → the app captures its own windows through the window server, then exits.
#   • iOS/iPadOS/tvOS → a booted Simulator + `xcrun simctl io booted screenshot` (native pixels =
#                       the exact App Store size for that device).
#
# The captured pixels are exactly App Store Connect's required sizes:
#   mac        2880×1800   (a 2× display with room for the 1440×900 window below its menu bar;
#                           a 1× monitor needs a HiDPI virtual display)
#   iphone-6.9 1320×2868   (portrait)  /  2868×1320 (the landscape hero)
#   ipad-13    2064×2752   (portrait)
#   appletv    1920×1080
#
# The iPad set is landscape throughout, and simctl cannot rotate a device. The iPad simulator runs
# in Full Screen Apps mode and the capture build is marked full-screen-only (UIRequiresFullScreen,
# in the built bundle only), so the app may turn itself landscape. The portrait framebuffer then
# shows it scaled to fit, and the screenshot is cropped to it: 2064×1548.
#
# Requirements:
#   • macOS target: full Xcode. No Screen Recording grant: the app only reads its own windows.
#   • iOS/iPadOS/tvOS targets: full Xcode (xcodebuild + Simulators), not just Command Line Tools.
#
# Usage:
#   tools/screenshots.sh all            # every platform this machine can build
#   tools/screenshots.sh macos          # just macOS
#   tools/screenshots.sh ios ipad tvos  # specific platforms
#   OUT=~/Desktop/shots tools/screenshots.sh all
#   PUNKTFUNK_SHOT_HERO=~/frame.png tools/screenshots.sh ios   # real captured frame for the hero
#   PUNKTFUNK_SHOT_FPS=120 tools/screenshots.sh ios ipad       # HUD refresh; Simulators report 60
#
# Keep SCENES in sync with ShotScenes.all.

set -euo pipefail

APPLE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$APPLE_DIR"

OUT="${OUT:-$APPLE_DIR/screenshots}"
BUNDLE_ID="io.unom.punktfunk"

# The App Store set, in listing order — the first three are what most people ever see, so they are
# the stream itself, the machines it found, and their games. Everything else in
# ShotScenes.all is a dev scene; capture those with `SCENES="15-library-touch 16-host-page" ...`.
SCENES=(${SCENES:-01-stream 02-hosts 15-library-touch 15f-library-filter 06-console 12-controllers 09e-waking-modal 05-settings 03-pair})
SETTLE="${SETTLE:-4}" # seconds to let a scene lay out before capturing

mkdir -p "$OUT"

log()  { printf '\033[1;36m[shots]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[shots]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[shots]\033[0m %s\n' "$*" >&2; exit 1; }

require_xcode() {
  xcrun --find simctl >/dev/null 2>&1 \
    || die "Full Xcode required for simulator capture (have Command Line Tools only).
       Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app"
}

# ---------------------------------------------------------------------------- macOS

shoot_macos() {
  # DEBUG build, deliberately: the whole shot harness lives behind `#if DEBUG`
  # (ScreenshotHost/ScreenshotScenes), so a release binary launches as the NORMAL app, never
  # prints PF_SHOT_WINDOW, and every scene "never reported a window". Debug renders the same
  # pixels — SwiftUI has no release-only visuals. xcodebuild, not `swift build`: SwiftPM copies
  # asset catalogs uncompiled, which blanks every OS mark and launcher icon.
  require_xcode
  log "macOS — building (xcodebuild PunktfunkClient)…"
  local dd="${PF_SHOT_DERIVED_DATA:-$APPLE_DIR/.build/shots-macos}"
  xcodebuild -project Punktfunk.xcodeproj -scheme PunktfunkClient -configuration Debug \
    -destination platform=macOS -derivedDataPath "$dd" CODE_SIGNING_ALLOWED=NO build >/dev/null \
    || die "macOS: xcodebuild failed"
  local bin="$dd/Build/Products/Debug/PunktfunkClient"
  [ -x "$bin" ] || die "build produced no $bin"

  for scene in "${SCENES[@]}"; do
    local logf; logf="$(mktemp)"
    # The app captures its own windows once the scene has settled, then exits (MacSelfCapture).
    # English on every target: the app otherwise follows the machine's locale ("vor 2 Std.").
    PUNKTFUNK_SHOT_SCENE="$scene" PUNKTFUNK_SHOT_SELFCAPTURE="$OUT" \
      PUNKTFUNK_SHOT_DELAY="${PUNKTFUNK_SHOT_DELAY:-$((SETTLE * 1000))}" \
      "$bin" -AppleLanguages '(en)' -AppleLocale en_US >"$logf" 2>&1 &
    local pid=$! dest="$OUT/mac-$scene.png"
    for _ in $(seq 1 150); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    kill -9 "$pid" 2>/dev/null || true
    if grep -q PF_SHOT_SAVED "$logf"; then
      log "macOS/$scene → $dest ($(pixels "$dest"))"
    else
      warn "macOS/$scene: the app saved no capture — skipping"; cat "$logf" >&2
    fi
    rm -f "$logf"
  done
}

# ------------------------------------------------------------------ iOS / iPadOS / tvOS

# $1 png  $2 $3 the app's window in pixels. A window whose orientation differs from the
# framebuffer's is drawn scaled to fit and centred; crop the png to that rect.
crop_to_window() {
  local png="$1" ww="$2" wh="$3" sw sh
  sw="$(sips -g pixelWidth "$png" | awk '/pixelWidth/ {print $2}')"
  sh="$(sips -g pixelHeight "$png" | awk '/pixelHeight/ {print $2}')"
  [ $(( (ww > wh) != (sw > sh) )) -eq 1 ] || return 0
  local cw ch
  if [ $(( ww * sh )) -gt $(( wh * sw )) ]; then
    cw=$sw; ch=$(( wh * sw / ww ))
  else
    ch=$sh; cw=$(( ww * sh / wh ))
  fi
  sips --cropOffset $(( (sh - ch) / 2 )) $(( (sw - cw) / 2 )) -c "$ch" "$cw" "$png" >/dev/null
}

# $1 device-type regex (matches both existing device names and the device-type catalog)
# $2 scheme  $3 sdk  $4 file prefix  $5 runtime platform (iOS|tvOS — for the create fallback)
# $6 name for a device we have to create — MUST satisfy $1 (see below)
shoot_sim() {
  require_xcode
  local match="$1" scheme="$2" sdk="$3" prefix="$4" platform="$5" createname="$6"

  # Reuse an existing device of this type; else create one against the newest available runtime
  # for the platform. CI runners commonly ship a runtime but not every device (the iPhone 16 Pro
  # Max is absent on ours), so create-on-demand is what makes it reproducible.
  #
  # The created device is named after the DEVICE, not after this script, for two reasons. It used
  # to be "pf-shot-<prefix>", which `$match` never matches — so every run created another
  # simulator and none was ever reused (they piled up on the runner). And the name is user-visible:
  # `UIDevice.current.name` is what the pairing sheet prefills as this device's name, so
  # "pf-shot-iphone-6.9" was rendered into an App Store screenshot.
  local udid
  udid="$(xcrun simctl list devices available | grep -E "$match" | grep -oE '[0-9A-F-]{36}' | head -1 || true)"
  if [ -z "$udid" ]; then
    local devtype rt
    devtype="$(xcrun simctl list devicetypes | grep -E "$match" \
      | grep -oE 'com\.apple\.CoreSimulator\.SimDeviceType\.[A-Za-z0-9.-]+' | head -1 || true)"
    rt="$(xcrun simctl list runtimes available | grep -E "^$platform " \
      | grep -oE 'com\.apple\.CoreSimulator\.SimRuntime\.[A-Za-z0-9.-]+' | tail -1 || true)"
    if [ -n "$devtype" ] && [ -n "$rt" ]; then
      udid="$(xcrun simctl create "$createname" "$devtype" "$rt" 2>/dev/null || true)"
      [ -n "$udid" ] && log "$prefix — created Simulator \"$createname\" $udid ($devtype)"
    fi
  fi
  [ -n "$udid" ] || die "$prefix: no Simulator matching /$match/, and none could be created
       (needs a $platform runtime + a matching device type — check 'xcrun simctl list')."
  log "$prefix — Simulator $udid"
  if [ "$prefix" = ipad-13 ]; then
    # Full Screen Apps, not Windowed Apps: a window cannot turn itself landscape. SpringBoard reads
    # this at boot, so set it on a shut-down device.
    xcrun simctl shutdown "$udid" 2>/dev/null || true
    local sbprefs="$HOME/Library/Developer/CoreSimulator/Devices/$udid/data/Library/Preferences/com.apple.springboard.plist"
    [ -f "$sbprefs" ] || plutil -create xml1 "$sbprefs"
    plutil -replace SBEnhancedWindowingModeEnabled -bool NO "$sbprefs"
  fi
  xcrun simctl boot "$udid" 2>/dev/null || true
  xcrun simctl bootstatus "$udid" -b >/dev/null 2>&1 || true
  # Every scene is a dark-mode scene. The in-app `.environment(\.colorScheme, .dark)` override
  # does NOT cross a presentation boundary — a `.sheet` gets its own environment and follows the
  # DEVICE appearance — so the pairing sheet came out light grey over the dark app. Set the
  # simulator itself to dark and the whole hierarchy, presentations included, agrees.
  xcrun simctl ui "$udid" appearance dark >/dev/null 2>&1 || true

  log "$prefix — building ($scheme)…"
  # PF_SHOT_DERIVED_DATA (optional): a STABLE DerivedData root, so repeat runs reuse the
  # incremental build instead of cold-building into a throwaway tmpdir — CI pins this
  # (apple.yml); local runs keep the self-cleaning mktemp default.
  local dd owned=0
  if [ -n "${PF_SHOT_DERIVED_DATA:-}" ]; then dd="$PF_SHOT_DERIVED_DATA"; else dd="$(mktemp -d)"; owned=1; fi
  mkdir -p "$dd"
  # tvOS-SIMULATOR trap (Xcode 26.6 and the 27 beta, local only so far): the build planner
  # schedules the SwiftPM MACRO plugin targets that swiftui-navigation-transitions pulls in
  # (OnceMacro/SwizzlingMacro/AssociationMacro) for the *tvOS* triple and never plans their
  # swift-syntax dependencies at all — "unable to resolve module dependency: 'SwiftSyntax'".
  # Device archives and iOS builds don't hit it (only the tvOS target links that package), and
  # prebuilt-vs-source swift-syntax makes no difference. Until Xcode fixes the planner, the
  # workaround is temporarily unlinking SwiftUINavigationTransitions from the tvOS target
  # (HomeView's use is canImport-guarded — the push transition degrades to the crossfade).
  xcodebuild -project Punktfunk.xcodeproj -scheme "$scheme" -configuration Debug \
    -sdk "$sdk" -destination "id=$udid" -derivedDataPath "$dd" \
    CODE_SIGNING_ALLOWED=NO build >/dev/null \
    || die "$prefix: xcodebuild failed"
  local app; app="$(find "$dd/Build/Products" -maxdepth 2 -name '*.app' -type d | head -1)"
  [ -n "$app" ] || die "$prefix: no .app built"
  if [ "$prefix" = ipad-13 ]; then
    /usr/libexec/PlistBuddy -c "Add :UIRequiresFullScreen bool true" "$app/Info.plist" 2>/dev/null \
      || /usr/libexec/PlistBuddy -c "Set :UIRequiresFullScreen true" "$app/Info.plist"
  fi
  xcrun simctl install "$udid" "$app"

  for scene in "${SCENES[@]}"; do
    xcrun simctl terminate "$udid" "$BUNDLE_ID" 2>/dev/null || true
    # `env` with an array: bash decides what is an assignment BEFORE expanding, so a
    # ${VAR:+NAME=...} word would be run as the command name instead.
    local envs=("SIMCTL_CHILD_PUNKTFUNK_SHOT_SCENE=$scene")
    [ "$prefix" = ipad-13 ] && envs+=("SIMCTL_CHILD_PUNKTFUNK_SHOT_ORIENTATION=landscape")
    [ -n "${PUNKTFUNK_SHOT_HERO:-}" ] \
      && envs+=("SIMCTL_CHILD_PUNKTFUNK_SHOT_HERO=$PUNKTFUNK_SHOT_HERO")
    [ -n "${PUNKTFUNK_SHOT_FPS:-}" ] \
      && envs+=("SIMCTL_CHILD_PUNKTFUNK_SHOT_FPS=$PUNKTFUNK_SHOT_FPS")
    # In OUT, not mktemp: the simulator writes nothing to a file under /var/folders.
    local applog="$OUT/.$prefix-$scene.log"
    rm -f "$applog"
    env "${envs[@]}" xcrun simctl launch --stdout="$applog" "$udid" "$BUNDLE_ID" \
      -AppleLanguages '(en)' -AppleLocale en_US >/dev/null
    sleep "$SETTLE"
    local dest="$OUT/$prefix-$scene.png"
    xcrun simctl io "$udid" screenshot "$dest" >/dev/null
    if [ "$prefix" = ipad-13 ]; then
      local win; win="$(sed -n 's/^PF_SHOT_WINDOW_PX //p' "$applog" | tail -1)"
      if [ -n "$win" ]; then
        crop_to_window "$dest" $win
      else
        warn "$prefix/$scene: the app reported no window size — left uncropped"
      fi
    fi
    rm -f "$applog"
    log "$prefix/$scene → $dest ($(pixels "$dest"))"
  done
  xcrun simctl terminate "$udid" "$BUNDLE_ID" 2>/dev/null || true
  # Only the mktemp default is ours to delete. A caller's pinned root is the incremental
  # build CI reuses across the three invocations.
  if [ "$owned" = 1 ]; then rm -rf "$dd"; fi
}

pixels() { sips -g pixelWidth -g pixelHeight "$1" 2>/dev/null | awk '/pixel/{print $2}' | paste -sd× -; }

# ---------------------------------------------------------------------------- dispatch

[ $# -gt 0 ] || set -- all
for target in "$@"; do
  case "$target" in
    macos) shoot_macos ;;
    ios)   shoot_sim 'iPhone 16 Pro Max'   Punktfunk-iOS  iphonesimulator  iphone-6.9 iOS 'iPhone 16 Pro Max' ;;
    ipad)  shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS 'iPad Pro 13-inch (M4)' ;;
    tvos)  shoot_sim 'Apple TV'            Punktfunk-tvOS appletvsimulator appletv    tvOS 'Apple TV 4K' ;;
    all)
      shoot_macos
      if xcrun --find simctl >/dev/null 2>&1; then
        shoot_sim 'iPhone 16 Pro Max' Punktfunk-iOS iphonesimulator iphone-6.9 iOS 'iPhone 16 Pro Max'
        shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS 'iPad Pro 13-inch (M4)'
        shoot_sim 'Apple TV' Punktfunk-tvOS appletvsimulator appletv tvOS 'Apple TV 4K'
      else
        warn "Skipping iOS/iPadOS/tvOS — full Xcode not found (Command Line Tools only)."
      fi
      ;;
    *) die "unknown target '$target' (use: all macos ios ipad tvos)" ;;
  esac
done

log "Done. Screenshots in $OUT"
ls -1 "$OUT" 2>/dev/null || true

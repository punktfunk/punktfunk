# App Store copy

Source of truth for what goes into App Store Connect. Every character-limited field in here has
been counted with `check-limits.py`; run it after any edit.

```sh
python3 clients/apple/store/check-limits.py
```

| File | Covers |
|------|--------|
| [`ios.md`](ios.md) | iOS/iPadOS Promotional Text (DE + EN), with alternates |
| [`macos.md`](macos.md) | macOS Promotional Text, Description, Keywords (DE + EN) |
| [`tvos.md`](tvos.md) | tvOS Promotional Text, Description, Keywords (DE + EN) |
| [`visionos.md`](visionos.md) | visionOS Promotional Text, Description, Keywords (DE + EN), review notes |
| [`review-notes.md`](review-notes.md) | App Review notes template + pre-submission checklist |
| [`privacy-app-addendum.md`](privacy-app-addendum.md) | App-specific privacy text to add to the existing policy page |
| [`privacy-tvos.md`](privacy-tvos.md) | Privacy Policy text for the Apple TV field (DE + EN), standalone |

German is primary throughout and uses the same informal "du" voice as the website
(`punktfunk-website/messages/de.json`). English is a localisation, not a translation exercise — a
few lines diverge where the German idiom does not carry.

## Three things that contradicted the original brief

1. **A Mac cannot be a host.** The brief suggested Mac copy could cover "running as a host/server
   or client on Mac". There is no macOS host — `punktfunk-host` has no macOS capture, virtual
   display, or encode backend. The macOS copy is client-only and says so explicitly.
2. **The existing privacy policy is website-only.** It covers server logs, Plausible, and a
   language cookie, and never mentions the apps. Linking it unchanged from App Store Connect is
   the kind of thing that draws a reviewer's attention to analytics that have nothing to do with
   the app. See `privacy-app-addendum.md` for the text to append.
3. **App Review notes cap at 4000 characters**, not the unlimited field the brief implied. The
   template is 3919 and fits.

## Claims used, and where they come from

Everything asserted in the copy was checked against the source rather than the marketing site:

- Hardware decode, HDR/4:4:4, controller and input support — the
  [support matrix](https://docs.punktfunk.unom.io/docs/support-matrix) and
  [client settings](https://docs.punktfunk.unom.io/docs/client-settings), which own those claims
- Entitlements and their justifications — `Config/Punktfunk.entitlements`,
  `Config/Punktfunk-macOS.entitlements` (both carry detailed rationale comments)
- Background audio mode and its 2.5.4 constraints — `Config/Info.plist`
- "Collects no data" — verified by absence: no analytics SDK in `Package.swift`, no telemetry
  symbols in `Sources/`, `URLSession` used only against the paired host
- Host platforms and protocol details — root `README.md`, `docs/releases/v0.24.0.md`
- Feature ship dates — `git tag --contains` on the relevant commits

## Not done here

`clients/apple` has no `PrivacyInfo.xcprivacy`. The app uses `UserDefaults`, which is a
required-reason API, so a manifest is expected. Flagged at the end of `review-notes.md`; left
alone because it is a code change, not copy.

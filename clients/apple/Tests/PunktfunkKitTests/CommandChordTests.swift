#if os(macOS)
import AppKit
import XCTest

@testable import PunktfunkKit

/// Pins the macOS ⌘-chord passthrough — the rule deciding which keyDowns `InputCapture`'s local
/// monitor takes off AppKit and forwards to the host instead of letting a menu key equivalent
/// claim them. Two things are worth a test rather than a comment:
///
///  * ⌘Q reaching the host at all. That is the whole point — it is the compositor chord on
///    Hyprland/KDE/GNOME, and left to AppKit it quits the client.
///  * ⌘⎋ NOT reaching it, under every combination. It is the way out of a captured stream;
///    forward it and the user is locked in.
///
/// The mouse model is deliberately absent from the rule (and from its signature): the chords are
/// forwarded under the desktop model too, as they are on the SDL clients. Gating it there would
/// read as a broken client — ⌘Q would quit Punktfunk instead of reaching Hyprland.
final class CommandChordTests: XCTestCase {
    // kVK_ANSI_* — physical positions, layout-independent (the same constants the monitor uses).
    private let q: UInt16 = 12, w: UInt16 = 13, h: UInt16 = 4, m: UInt16 = 46
    private let f: UInt16 = 3, esc: UInt16 = 53, leftArrow: UInt16 = 123

    /// Captured with the setting on — the shipping default.
    private func forwards(
        _ keyCode: UInt16, _ flags: NSEvent.ModifierFlags,
        forwarding: Bool = true, inhibit: Bool = true
    ) -> Bool {
        InputCapture.forwardsCommandChord(
            keyCode: keyCode, flags: flags, forwarding: forwarding,
            inhibitShortcuts: inhibit)
    }

    func testCommandChordsGoToTheHostWhileCaptured() {
        XCTAssertTrue(forwards(q, .command)) // ⌘Q — the compositor chord
        XCTAssertTrue(forwards(w, .command))
        XCTAssertTrue(forwards(h, .command))
        XCTAssertTrue(forwards(m, .command))
        XCTAssertTrue(forwards(q, [.command, .shift])) // ⇧⌘Q
        XCTAssertTrue(forwards(m, [.command, .control, .option, .shift]))
    }

    func testTheEscapeHatchIsNeverForwarded() {
        XCTAssertFalse(forwards(esc, .command))
    }

    /// Capturing system shortcuts includes the Mac's own fullscreen chord; the monitor keeps ⌃⌘F
    /// only with the setting off.
    func testFullscreenChordGoesToTheHostWithTheSetting() {
        XCTAssertTrue(forwards(f, [.control, .command]))
        XCTAssertFalse(forwards(f, [.control, .command], inhibit: false))
    }

    /// The reservation is exact: ⌘⎋ specifically, not "anything with Esc in it".
    func testNeighbouringChordsAreNotReserved() {
        XCTAssertTrue(forwards(esc, [.command, .shift]))
        XCTAssertTrue(forwards(f, .command))
    }

    func testNothingWithoutCommandIsClaimedHere() {
        // The ⌃⌥⇧ family and bare keys reach the monitor's earlier blocks / the responder chain.
        XCTAssertFalse(forwards(q, [.control, .option, .shift]))
        XCTAssertFalse(forwards(q, []))
        XCTAssertFalse(forwards(esc, []))
    }

    func testReleasedCaptureLeavesTheMenuAlone() {
        // Not forwarding = the user is in the local UI: ⌘Q must quit the app, ⌘W close the window.
        XCTAssertFalse(forwards(q, .command, forwarding: false))
        XCTAssertFalse(forwards(w, .command, forwarding: false))
    }

    func testTheCrossClientSettingTurnsItOff() {
        XCTAssertFalse(forwards(q, .command, inhibit: false))
    }

    /// `deviceIndependentFlagsMask` also carries Caps Lock and the `.function`/`.numericPad` bits
    /// every arrow key sets, so comparing it for equality would stop chords being recognized in
    /// exactly the states a user does not connect to their keyboard: Caps Lock on, or the chord
    /// spelled with an arrow. `chordFlags` isolates the four real modifiers.
    func testCapsLockAndArrowBitsDoNotChangeAChord() throws {
        let capsQ = try XCTUnwrap(keyEvent(q, [.command, .capsLock]))
        XCTAssertEqual(InputCapture.chordFlags(capsQ), .command)
        XCTAssertTrue(forwards(q, InputCapture.chordFlags(capsQ)))

        // ⌘⎋ with Caps Lock on is still the escape hatch, not a chord for the host.
        let capsEsc = try XCTUnwrap(keyEvent(esc, [.command, .capsLock]))
        XCTAssertEqual(InputCapture.chordFlags(capsEsc), .command)
        XCTAssertFalse(forwards(esc, InputCapture.chordFlags(capsEsc)))

        // ⌘← — arrows set .function|.numericPad, which say nothing about the chord.
        let cmdLeft = try XCTUnwrap(keyEvent(leftArrow, [.command, .function, .numericPad]))
        XCTAssertEqual(InputCapture.chordFlags(cmdLeft), .command)
        XCTAssertTrue(forwards(leftArrow, InputCapture.chordFlags(cmdLeft)))
    }

    /// A forwarded chord is only useful if the key has a host VK — the monitor swallows either
    /// way, so an unmapped one would silently do nothing. Spot-check the common ⌘ letters.
    func testTheCommonChordKeysMapToHostVKs() {
        XCTAssertEqual(InputCapture.keyCodeToVK[q], 0x51) // VK 'Q'
        XCTAssertEqual(InputCapture.keyCodeToVK[w], 0x57) // VK 'W'
        XCTAssertEqual(InputCapture.keyCodeToVK[h], 0x48) // VK 'H'
        XCTAssertEqual(InputCapture.keyCodeToVK[m], 0x4D) // VK 'M'
        XCTAssertEqual(InputCapture.keyCodeToVK[leftArrow], 0x25) // VK_LEFT
    }

    /// The Mac's global shortcuts are off only while a capture holds them AND the main thread
    /// answers — a hung client must not keep ⌥⌘⎋ from the user.
    func testSystemShortcutsAreOffOnlyWhileHeldByALiveApp() {
        XCTAssertTrue(SystemHotKeys.shouldBeOff(holders: 1, mainSilentFor: 0.5))
        XCTAssertFalse(SystemHotKeys.shouldBeOff(holders: 0, mainSilentFor: 0))
        XCTAssertFalse(SystemHotKeys.shouldBeOff(holders: 1, mainSilentFor: 2.5))
    }

    /// The watchdog end to end, against WindowServer: hang main, the shortcuts come back; main
    /// answers again, they go off again; release, they are back for good.
    func testAHungMainThreadGivesTheSystemShortcutsBack() throws {
        try XCTSkipIf(ProcessInfo.processInfo.environment["CI"] != nil, "toggles this Mac's shortcuts")
        SystemHotKeys.hold()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.3))
        XCTAssertTrue(SystemHotKeys.isOff)
        Thread.sleep(forTimeInterval: 3) // main hangs
        XCTAssertFalse(SystemHotKeys.isOff)
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 1))
        XCTAssertTrue(SystemHotKeys.isOff)
        SystemHotKeys.release()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.3))
        XCTAssertFalse(SystemHotKeys.isOff)
    }

    /// Captured, ⌘Space, ⌘Tab and Mission Control arrive as ordinary key events and need host
    /// VKs: the key path ignores an unmapped keyCode.
    func testTheSystemShortcutKeysMapToHostVKs() {
        XCTAssertEqual(InputCapture.keyCodeToVK[49], 0x20) // Space (⌘Space)
        XCTAssertEqual(InputCapture.keyCodeToVK[48], 0x09) // Tab (⌘Tab)
        XCTAssertEqual(InputCapture.keyCodeToVK[126], 0x26) // Up arrow (⌃↑ Mission Control)
    }

    /// A Mac JIS board sends the VKs an iPad sends for the same keys: kVK_JIS_* → HID usage.
    func testTheJISKeysMapLikeTheHIDPath() {
        let kvkToHID: [UInt16: Int] = [0x5D: 0x89, 0x5E: 0x87, 0x5F: 0x85, 0x68: 0x90, 0x66: 0x91]
        for (kvk, hid) in kvkToHID {
            XCTAssertNotNil(InputCapture.hidToVK[hid])
            XCTAssertEqual(InputCapture.keyCodeToVK[kvk], InputCapture.hidToVK[hid], "kVK \(kvk)")
        }
    }

    // Release ownership follows the forwarded physical key, not the current modifier chord
    func testTrackedReleasesIgnoreModifierChanges() throws {
        for flags: NSEvent.ModifierFlags in [.command, [], .control, [.control, .command]] {
            var tracked: Set<UInt32> = [0x46]
            let event = try XCTUnwrap(keyEvent(f, flags, type: .keyUp))
            XCTAssertEqual(InputCapture.takeRelease(
                event, forwarding: true, pressedVKs: [0x46], chordVKs: &tracked), 0x46)
            XCTAssertTrue(tracked.isEmpty)
        }
    }

    // One key's release cannot clear another held chord key or generate a duplicate release
    func testRepeatedTapsClaimEachReleaseOnce() throws {
        var tracked: Set<UInt32> = [0x51, 0x57]
        let event = try XCTUnwrap(keyEvent(w, .command, type: .keyUp))
        for _ in 0..<3 {
            tracked.insert(0x57)
            XCTAssertEqual(InputCapture.takeRelease(
                event, forwarding: true, pressedVKs: [0x51, 0x57], chordVKs: &tracked), 0x57)
            XCTAssertEqual(tracked, [0x51])
            XCTAssertNil(InputCapture.takeRelease(
                event, forwarding: true, pressedVKs: [0x51], chordVKs: &tracked))
        }
    }

    // Held-key repeats keep their release outstanding for the eventual physical key-up
    func testHeldKeyRepeatDoesNotTakeReleaseOwnership() throws {
        var tracked: Set<UInt32> = [0x57]
        let repeatedDown = try XCTUnwrap(keyEvent(w, .command, isRepeat: true))
        XCTAssertNil(InputCapture.takeRelease(
            repeatedDown, forwarding: true, pressedVKs: [0x57], chordVKs: &tracked))
        XCTAssertEqual(tracked, [0x57])
        let release = try XCTUnwrap(keyEvent(w, .command, type: .keyUp))
        XCTAssertEqual(InputCapture.takeRelease(
            release, forwarding: true, pressedVKs: [0x57], chordVKs: &tracked), 0x57)
    }

    // A key held before ⌘ went down still reaches the host when it is released under ⌘
    func testAHeldKeyReleasedUnderCommandIsTaken() throws {
        var tracked: Set<UInt32> = []
        let underCommand = try XCTUnwrap(keyEvent(leftArrow, .command, type: .keyUp))
        XCTAssertEqual(InputCapture.takeRelease(
            underCommand, forwarding: true, pressedVKs: [0x25], chordVKs: &tracked), 0x25)
        let plain = try XCTUnwrap(keyEvent(leftArrow, [], type: .keyUp))
        XCTAssertNil(InputCapture.takeRelease(
            plain, forwarding: true, pressedVKs: [0x25], chordVKs: &tracked))
    }

    // Unowned releases and local input retain the responder-chain path
    func testUntrackedAndReleasedCaptureKeysPassThrough() throws {
        var tracked: Set<UInt32> = [0x57]
        let untracked = try XCTUnwrap(keyEvent(q, .command, type: .keyUp))
        XCTAssertNil(InputCapture.takeRelease(
            untracked, forwarding: true, pressedVKs: [0x57], chordVKs: &tracked))
        let released = try XCTUnwrap(keyEvent(w, .command, type: .keyUp))
        XCTAssertNil(InputCapture.takeRelease(
            released, forwarding: false, pressedVKs: [0x57], chordVKs: &tracked))
        XCTAssertEqual(tracked, [0x57])
    }

    // Construct physical key events without keyboard layout or window dependencies
    private func keyEvent(
        _ keyCode: UInt16, _ flags: NSEvent.ModifierFlags,
        type: NSEvent.EventType = .keyDown, isRepeat: Bool = false
    ) -> NSEvent? {
        NSEvent.keyEvent(
            with: type, location: .zero, modifierFlags: flags, timestamp: 0,
            windowNumber: 0, context: nil, characters: "", charactersIgnoringModifiers: "",
            isARepeat: isRepeat, keyCode: keyCode)
    }
}
#endif

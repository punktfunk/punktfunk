//
//  LaunchHoldView.swift
//  Punktfunk
//
//  The launch hold: the picked title's cover leaves its shelf tile and holds the screen until
//  the game is actually up.
//

import PunktfunkKit
import SwiftUI

/// One screen from the tap to the game.
///
/// Mounts over the shelf at the dial, flies the cover out of the tile the player just pressed, and
/// stays opaque across the switch to the stream — so the launcher booting behind it is never seen.
/// `SessionModel.launchHold` decides when it comes down; a tap (the button, on tvOS) shows the
/// stream regardless.
///
/// Laid out as a title card rather than a centred stack: the cover carries the screen from the
/// left at most of its height, and everything the host knows about the title reads down the
/// right-hand side.
struct LaunchHoldView: View {
    let entry: GameEntry
    let host: StoredHost?
    /// The dial is still in flight — the status line says so until frames are coming.
    var connecting = false
    /// The game is up and the host is waiting for its window.
    var windowWait = false
    /// The title's files, while the host fetches them before it opens the stream.
    var download: HostDownload?
    /// The shelf tile's rect in global coordinates, when there is a tile to fly out of.
    var sourceRect: CGRect?
    /// Screenshot harness: canned art in place of the paired host's loader.
    var artOverride: (any LibraryArtSource)?
    let onShow: () -> Void

    @State private var loader: (any LibraryArtSource)?
    /// The cover has left its tile (spring), and the backdrop has closed over the shelf (fade).
    @State private var landed = false

    init(
        entry: GameEntry, host: StoredHost?, connecting: Bool = false, windowWait: Bool = false,
        download: HostDownload? = nil,
        sourceRect: CGRect? = nil, artOverride: (any LibraryArtSource)? = nil,
        onShow: @escaping () -> Void
    ) {
        self.entry = entry
        self.host = host
        self.connecting = connecting
        self.windowWait = windowWait
        self.download = download
        self.sourceRect = sourceRect
        self.artOverride = artOverride
        self.onShow = onShow
        // Seeded rather than assigned on appear when the caller already has one. The cover's
        // subtree changes identity the moment a loader arrives, and a replaced subtree has no
        // previous geometry to animate from — so a loader landing on the same frame as the
        // flight is armed silently eats the flight. With the art in hand from the first
        // render there is no swap to collide with.
        _loader = State(initialValue: artOverride)
    }

    /// The cover's flight: a spring, so it arrives with a little weight rather than easing to
    /// a stop, and loose enough that the turn is readable on the way.
    private let flight = Animation.spring(response: 0.75, dampingFraction: 0.72)

    var body: some View {
        GeometryReader { geo in
            let layout = Layout(size: geo.size)
            let from = startRect(layout.cover, geo)
            // The pair is laid out, not positioned. Every attempt to place the card by offset
            // or `position` was undone by the turn: a geometry effect re-anchors what it
            // transforms to its parent's origin, so anything carrying one lands in the corner.
            // A real stack cannot be argued with, and the flight rides on top of it as a delta
            // that is zero once the card is home.
            ZStack {
                backdrop(geo.size)
                    .opacity(landed ? 1 : 0)
                    .animation(.easeOut(duration: 0.3), value: landed)
                HStack(alignment: .center, spacing: layout.gap) {
                    cover
                        .frame(width: layout.cover.width, height: layout.cover.height)
                        // Flattened first: the poster inside is a flexible view, and a
                        // projection applied over one resolves its anchor against the whole
                        // window instead of the card, which is what kept parking it in the
                        // corner. A compositing group gives the turn a definite thing to turn.
                        .compositingGroup()
                        // One full turn on the way over, around the card's own vertical axis.
                        .rotation3DEffect(
                            .degrees(landed ? 360 : 0), axis: (x: 0, y: 1, z: 0),
                            perspective: 0.45)
                        .scaleEffect(landed ? 1 : from.width / max(layout.cover.width, 1))
                        .offset(
                            x: landed ? 0 : from.midX - layout.cover.midX,
                            y: landed ? 0 : from.midY - layout.cover.midY)
                        .animation(flight, value: landed)
                    details
                        .frame(width: layout.details.width, alignment: .leading)
                        .opacity(landed ? 1 : 0)
                        // Behind the cover's own flight, so the card arrives and the words
                        // settle beside it rather than the two racing.
                        .animation(.easeOut(duration: 0.3).delay(0.18), value: landed)
                }
                .frame(width: geo.size.width, height: geo.size.height)
            }
            .frame(width: geo.size.width, height: geo.size.height)
        }
        .environment(\.colorScheme, .dark)
        .ignoresSafeArea()
        #if !os(tvOS)
        .contentShape(Rectangle())
        .onTapGesture(perform: onShow)
        #endif
        .task {
            if loader == nil {
                loader = Self.hostLoader(host)
            }
            // Armed a frame later, deliberately. This view is INSERTED — by the session's
            // overlay, and by the preview harness — and a state change made while that
            // insertion is still being committed rides the same transaction, which SwiftUI
            // is free to apply without animating. Opacity survived that; the geometry did
            // not, so the flight came out as a plain fade. One frame's wait separates them.
            try? await Task.sleep(nanoseconds: 40_000_000)
            landed = true
        }
    }

    /// The shelf's own loader: host-origin art over the paired identity, CDNs plain; the demo
    /// host's posters are drawn in-app, and an unpaired host has none. Resolved once on appear
    /// rather than per render — the session republishes its stats every second, and rebuilding
    /// this on each of those would re-read the identity every second with it.
    private static func hostLoader(_ host: StoredHost?) -> (any LibraryArtSource)? {
        guard let host else { return nil }
        if DemoMode.isDemo(host) { return DemoMode.art }
        guard case .success(let target) = MgmtTarget.make(host: host) else { return nil }
        return LibraryArtLoader(target)
    }

    /// Cover and text as one centred pair: a 2:3 card on the left, and a column beside it wide
    /// enough for a long title to break twice rather than fifteen times.
    private struct Layout {
        let cover: CGRect
        let details: CGRect
        /// The stack's own spacing, so the measured rects and the drawn pair cannot drift.
        let gap: CGFloat

        init(size: CGSize) {
            gap = min(size.width * 0.04, 56)
            let detailsW = min(max(size.width * 0.34, 220), 460)
            // The cover wants most of the height, which on a phone held upright is wider than
            // the screen. The column and the margins come out of the width first; the card
            // takes what is left.
            let widest = size.width - gap - detailsW - 32
            var coverH = min(size.height * 0.62, 460)
            var coverW = coverH * 2 / 3
            if coverW > widest {
                coverW = max(widest, 1)
                coverH = coverW * 1.5
            }
            let originX = (size.width - (coverW + gap + detailsW)) / 2
            cover = CGRect(
                x: originX, y: (size.height - coverH) / 2, width: coverW, height: coverH)
            // Centred on the cover, so the pair reads as one object however tall the text runs.
            details = CGRect(
                x: originX + coverW + gap, y: cover.midY - coverH / 2,
                width: detailsW, height: coverH)
        }
    }

    /// Where the flight starts: the tile the player pressed, in this view's own space.
    ///
    /// Falls back to the settled rect, slightly small, whenever there is no usable tile — a
    /// keyboard launch off a scrolled-away row, a deep link, a stale rect from a recycled cell.
    /// The cover then simply arrives rather than flying from somewhere the player never looked.
    private func startRect(_ target: CGRect, _ geo: GeometryProxy) -> CGRect {
        let container = geo.frame(in: .global)
        guard let source = sourceRect, source.width > 1, source.height > 1 else {
            return target.insetBy(dx: target.width * 0.14, dy: target.height * 0.14)
        }
        let local = source.offsetBy(dx: -container.minX, dy: -container.minY)
        guard local.intersects(CGRect(origin: .zero, size: geo.size).insetBy(dx: -80, dy: -80))
        else {
            return target.insetBy(dx: target.width * 0.14, dy: target.height * 0.14)
        }
        return local
    }

    /// The title's own art, thrown out of focus behind it — the game colours the room it is
    /// starting in. Decoded small on purpose: it is blurred to nothing but a wash, so a
    /// full-resolution copy of a cover already on screen would be paid for twice for no pixels.
    /// Opaque underneath, because hiding the launcher behind it is this screen's whole job.
    private func backdrop(_ size: CGSize) -> some View {
        ZStack {
            Color.black
            // Held back until there is a loader, for the same reason as the cover: a poster
            // still waiting on one draws its grey library placeholder under the wash.
            if loader != nil {
                PosterImage(
                    candidates: entry.art.posterCandidates, title: "", loader: loader,
                    drawnSize: CGSize(width: 120, height: 180))
                    .frame(width: size.width, height: size.height)
                    // Overscanned before the blur: a blur samples transparent pixels past the
                    // edge, which would ring the wash in a dark frame the size of the screen.
                    .scaleEffect(1.25)
                    .blur(radius: 120)
                    .saturation(1.3)
                    .opacity(0.55)
            }
            LinearGradient(
                colors: [.black.opacity(0.3), .black.opacity(0.55), .black.opacity(0.9)],
                startPoint: .top, endPoint: .bottom)
        }
        .ignoresSafeArea()
    }

    private var cover: some View {
        // Held back until there is a loader: the hold's own placeholder is the flat dark card,
        // not the poster's grey one — and a subtree swapped in mid-flight has no previous frame
        // to animate from, so the cover would land in place rather than fly.
        Group {
            if loader != nil {
                PosterImage(
                    candidates: entry.art.posterCandidates, title: entry.title,
                    loader: loader, icon: entry.iconToken)
            } else {
                Color.white.opacity(0.06)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(.white.opacity(0.14), lineWidth: 1)
        }
        .shadow(color: .black.opacity(0.65), radius: 34, y: 18)
    }

    /// `PC · 2024 · Steam` — what the host filed the title under, in the order a player scans it.
    private var facts: String {
        [entry.platform, entry.releaseYear.map(String.init), entry.storeLabel]
            .compactMap { $0 }
            .joined(separator: " \u{b7} ")
    }

    private var details: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(entry.title)
                .font(.geist(34, .semibold, relativeTo: .largeTitle))
                .foregroundStyle(.white)
                .lineLimit(3)
                .minimumScaleFactor(0.7)
                .fixedSize(horizontal: false, vertical: true)
            if !facts.isEmpty {
                Text(facts)
                    .font(.geist(15, .medium, relativeTo: .callout))
                    .foregroundStyle(.white.opacity(0.62))
                    .padding(.top, 10)
            }
            if let developer = entry.developer, !developer.isEmpty {
                Text(developer)
                    .font(.geist(14, .regular, relativeTo: .subheadline))
                    .foregroundStyle(.white.opacity(0.45))
                    .padding(.top, 4)
            }
            if let genres = entry.genres, !genres.isEmpty {
                Text(genres.joined(separator: " \u{b7} "))
                    .font(.geist(14, .regular, relativeTo: .subheadline))
                    .foregroundStyle(.white.opacity(0.45))
                    .padding(.top, 4)
            }
            if let download {
                // The wait has a length now: a bar where the spinner was. No Show stream — there
                // is no stream until the files are in.
                Group {
                    if let fraction = download.fraction {
                        ProgressView(value: fraction)
                    } else {
                        ProgressView(value: nil as Double?)
                    }
                }
                .progressViewStyle(.linear)
                .tint(.white)
                .frame(maxWidth: 360)
                .padding(.top, 26)
                Text(download.state == "downloading" ? "Downloading \u{b7} \(download.line)" : download.line)
                    .font(.geist(13, .regular, relativeTo: .footnote))
                    .foregroundStyle(.white.opacity(0.7))
                    .padding(.top, 10)
                Text("Leaving won't stop the download.")
                    .font(.geist(12, .regular, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.4))
                    .padding(.top, 4)
            } else {
                HStack(spacing: 9) {
                    ProgressView().controlSize(.small).tint(.white)
                    Text(
                        connecting
                            ? "Connecting…"
                            : windowWait ? "Waiting for the game's window…" : "Starting the game…")
                        .font(.geist(13, .regular, relativeTo: .footnote))
                        .foregroundStyle(.white.opacity(0.5))
                }
                .padding(.top, 26)
                Button("Show stream", action: onShow)
                    .buttonStyle(.bordered)
                    .padding(.top, 16)
            }
        }
    }
}

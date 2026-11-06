// visionOS: the stream on a curved screen in a volume. The session stays in its window's model;
// the volume shows the same picture texture the window shows flat.

#if os(visionOS)
import GameController
import PunktfunkKit
import RealityKit
import SwiftUI

/// The sessions a curved screen can show, by connection, and the volume each one names.
@MainActor @Observable
final class CurveStage {
    static let shared = CurveStage()

    /// A streaming session and its picture; `curved` while the volume shows it.
    @MainActor @Observable
    final class Session {
        let id = UUID()
        let connection: PunktfunkConnection
        @ObservationIgnored weak var model: SessionModel?
        @ObservationIgnored let ring: RingState
        let target: CurveTarget
        var curved = false

        init(
            connection: PunktfunkConnection, model: SessionModel, ring: RingState,
            target: CurveTarget
        ) {
            self.connection = connection
            self.model = model
            self.ring = ring
            self.target = target
        }
    }

    private(set) var sessions: [ObjectIdentifier: Session] = [:]

    /// `connection` streams in a window into `target`, which a curve can show instead.
    func register(
        _ connection: PunktfunkConnection, model: SessionModel, ring: RingState,
        target: CurveTarget
    ) {
        let key = ObjectIdentifier(connection)
        guard sessions[key] == nil else { return }
        sessions[key] = Session(connection: connection, model: model, ring: ring, target: target)
    }

    func session(_ id: UUID) -> Session? {
        sessions.values.first { $0.id == id }
    }

    func session(for connection: PunktfunkConnection) -> Session? {
        sessions[ObjectIdentifier(connection)]
    }

    /// The curve's volume shows `connection`'s picture.
    func curved(_ connection: PunktfunkConnection) -> Bool {
        session(for: connection)?.curved == true
    }

    func enter(_ connection: PunktfunkConnection) {
        guard let session = session(for: connection), !session.curved else { return }
        session.curved = true
        session.target.curved = true
        session.target.pointer?.curveShown(true)
    }

    func leave(_ connection: PunktfunkConnection) {
        guard let session = session(for: connection), session.curved else { return }
        session.curved = false
        session.target.curved = false
        session.target.pointer?.curveShown(false)
    }

    func ended(_ connection: PunktfunkConnection) {
        sessions[ObjectIdentifier(connection)] = nil
    }
}

/// The volume a curved session shows its picture in. It closes itself once the session leaves
/// the curve; the player closing it brings the picture back to the session's window and sets the
/// stop to Flat. With the session's window gone too, a Home window opens in its place.
struct CurveScene: View {
    static let sceneID = "curve"
    let id: UUID?
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen

    var body: some View {
        if let id, let session = CurveStage.shared.session(id), session.curved,
           let model = session.model {
            CurveScreen(
                target: session.target, model: model, ring: session.ring,
                connection: session.connection)
                // On the top front edge, clear of the system bar and its grab handle below.
                .ornament(attachmentAnchor: .scene(UnitPoint3D.topFront), contentAlignment: .bottom) {
                    StreamOrnament(quickActions: { session.ring.toggleCentred() })
                }
                .onDisappear {
                    // Still the session's curve, so the player closed it rather than the slider
                    // or the session's end: Flat, so the window's slider says what it shows.
                    if CurveStage.shared.curved(session.connection) {
                        screen = ScreenCurve.flat.rawValue
                    }
                    CurveStage.shared.leave(session.connection)
                }
        } else {
            Color.clear.task {
                // Back to Flat: the session's window holds the picture again, so this just goes.
                if let id, CurveStage.shared.session(id) != nil {
                    dismissWindow()
                    return
                }
                // The session ended with its window: the app's last window cannot close itself,
                // so the Home must be up first. A closing window can read as on screen a moment.
                try? await Task.sleep(for: .milliseconds(500))
                if !Self.anotherWindowShows {
                    openWindow(id: PunktfunkClientApp.mainSceneID)
                    for _ in 0..<30 where !Self.anotherWindowShows {
                        try? await Task.sleep(for: .milliseconds(100))
                    }
                }
                dismissWindow()
            }
        }
    }

    /// Another of the app's windows is on screen besides this volume.
    private static var anotherWindowShows: Bool {
        UIApplication.shared.connectedScenes.filter {
            $0 is UIWindowScene
                && ($0.activationState == .foregroundActive
                    || $0.activationState == .foregroundInactive)
        }.count > 1
    }
}

/// What the window draws over the picture, re-homed on the curve as attachments.
fileprivate enum CurveOverlay: String, CaseIterable {
    case hud, badges, ring, resize
}

/// The picture across the volume, bent to the stored Screen stop; a new curved stop rebuilds the
/// mesh live. The content stays flexible: any fixed axis freezes the volume's resize handle. A
/// pinch on it reaches the host through the session's stream view, or closes the ring while it
/// is open; a two-hand twist turns the ring, and the window's overlays lie along it, just in front.
/// A pad press goes to the game wherever the eyes rest, never to the screen as a pinch.
private struct CurveScreen: View {
    let target: CurveTarget
    @ObservedObject var model: SessionModel
    @ObservedObject var ring: RingState
    let connection: PunktfunkConnection
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.statsScalePct) private var statsScalePct = 100
    @Environment(\.physicalMetrics) private var metrics
    /// What the screen's mesh and material were last built for, and the pinch in progress.
    @State private var state = CurveState()
    /// Each overlay's laid-out size in points, for placing it on the curve.
    @State private var sizes: [CurveOverlay: CGSize] = [:]
    /// Counts the builds a resize deferred, so the overlays lie down again on each.
    @State private var rebuilt = 0

    final class CurveState {
        var shape: CurveShape?
        var texture: TextureResource?
        /// The stop's radius the shape was built for; nil is flat.
        var stop: Float?
        /// The shape a resize wants, built once the box holds still.
        var pending: CurveShape?
        /// The box shape last handed to Match window.
        var reported: CurveShape?
        var pinching = false
        /// This pinch closed the ring and goes no further.
        var swallowing = false
    }

    /// The ring's frame on the curve: its scrim and the wheel inside it.
    private static let ringSide: CGFloat = 720
    /// The window picture width, in points, whose overlay proportions the curve keeps.
    private static let referenceWidth: CGFloat = 1280
    /// The twist that commits the ring, as on a touchscreen.
    private static let commitDegrees: Double = 30

    var body: some View {
        GeometryReader3D { proxy in
            let box = metrics.convert(proxy.size, to: .meters)
            let texture = target.texture, aspect = target.aspect
            let radius = ScreenCurve(name: screen).radius
            let placement = HUDPlacement(rawValue: hudPlacement) ?? .topTrailing
            let extents = sizes.mapValues { metrics.convert($0, to: .meters) }
            let pointWidth = Float(metrics.convert(Self.referenceWidth, to: .meters))
            let generation = rebuilt
            RealityView { content, attachments in
                let screen = ModelEntity()
                screen.components.set(InputTargetComponent())
                content.add(screen)
                for overlay in CurveOverlay.allCases {
                    if let entity = attachments.entity(for: overlay) { screen.addChild(entity) }
                }
            } update: { content, attachments in
                guard let screen = content.entities.first as? ModelEntity, let texture, let aspect
                else { return }
                _ = generation
                let space = SIMD3(Float(box.width), Float(box.height), Float(box.depth))
                // Match window fills the box and asks the host for its shape; otherwise the
                // picture keeps its own shape inside the box.
                let follows = connection.settings.matchWindow
                let wanted = follows
                    ? CurveShape(filling: space, radius: radius)
                    : CurveShape(box: space, aspect: Float(aspect), radius: radius)
                if follows, wanted != state.reported {
                    state.reported = wanted
                    target.pointer?.curveSize(
                        width: Double(metrics.convert(CGFloat(wanted.length), from: .meters)),
                        height: Double(metrics.convert(CGFloat(wanted.height), from: .meters)))
                }
                if state.shape == nil || texture !== state.texture || radius != state.stop {
                    build(wanted, on: screen, texture: texture, stop: radius)
                } else if let built = state.shape, wanted != built, wanted != state.pending {
                    // A resize: stretch the built screen to the box now and build the new one once
                    // the box holds still. A mesh and a collision shape each frame stall the drag.
                    screen.scale = SIMD3(repeating: wanted.halfChord / built.halfChord)
                    screen.position = [0, 0, wanted.front]
                    state.pending = wanted
                    Task { @MainActor in
                        try? await Task.sleep(for: .milliseconds(250))
                        guard state.pending == wanted else { return }
                        build(wanted, on: screen, texture: texture, stop: radius)
                        rebuilt &+= 1
                    }
                } else if wanted == state.shape, state.pending != nil {
                    // Back to the built box before the build came due.
                    screen.scale = .one
                    screen.position = [0, 0, wanted.front]
                    state.pending = nil
                }
                // The overlays lie on the built screen; a stretch carries them with it.
                let shape = state.shape ?? wanted
                // The overlays keep the size they have over a window picture this wide.
                let scale = 2 * shape.halfChord / pointWidth
                for overlay in CurveOverlay.allCases {
                    guard let entity = attachments.entity(for: overlay) else { continue }
                    let size = extents[overlay] ?? .zero
                    let pose = shape.pose(
                        of: overlay, size: SIMD2(Float(size.width), Float(size.height)) * scale,
                        placement: placement)
                    entity.scale = SIMD3(repeating: scale)
                    entity.position = pose.position
                    entity.orientation = pose.orientation
                }
            } attachments: {
                Attachment(id: CurveOverlay.hud) {
                    if model.phase == .streaming, model.statsVerbosity != .off {
                        StreamHUDView(
                            model: model, connection: connection, placement: placement,
                            verbosity: model.statsVerbosity,
                            scale: Double(min(max(statsScalePct, 75), 200)) / 100)
                            .measured { sizes[.hud] = $0 }
                    }
                }
                Attachment(id: CurveOverlay.badges) {
                    StreamBadgeStack(
                        model: model, captureEnabled: true, statsVerbosity: model.statsVerbosity)
                        .measured { sizes[.badges] = $0 }
                }
                Attachment(id: CurveOverlay.ring) {
                    if ring.visible {
                        RingOverlay(
                            state: ring,
                            cfg: OverlayConfig.parse(model.settings.overlayActions),
                            actions: model.ringActions(connection, ring: ring))
                            .frame(width: Self.ringSide, height: Self.ringSide)
                            .measured { sizes[.ring] = $0 }
                    }
                }
                Attachment(id: CurveOverlay.resize) {
                    if model.resizing {
                        ResizeIndicatorView(active: true)
                            .measured { sizes[.resize] = $0 }
                    }
                }
            }
            .environment(\.inAttachment, true)
            .gesture(pinch)
            .simultaneousGesture(twist)
            .handlesGameControllerEvents(matching: .gamepad)
        }
        .background(Anchor(target: target))
        .volumeBaseplateVisibility(.hidden)
        // The volume's chrome hides while streaming, as the session's window hides its own.
        .persistentSystemOverlays(.hidden)
        .supportedVolumeViewpoints(.front)
    }

    /// The screen's mesh, material and collision shape for `shape`, at its own size.
    private func build(
        _ shape: CurveShape, on screen: ModelEntity, texture: TextureResource, stop: Float?
    ) {
        guard let mesh = try? shape.mesh() else { return }
        screen.model = ModelComponent(mesh: mesh, materials: [UnlitMaterial.picture(texture)])
        screen.scale = .one
        screen.position = [0, 0, shape.front]
        state.shape = shape
        state.texture = texture
        state.stop = stop
        state.pending = nil
        Task { @MainActor in
            guard let collision = try? await ShapeResource.generateStaticMesh(from: mesh),
                  state.shape == shape
            else { return }
            screen.components.set(CollisionComponent(shapes: [collision]))
        }
    }

    /// One pinch, held or tapped, on the screen: the session's touch model takes it from there.
    private var pinch: some Gesture {
        DragGesture(minimumDistance: 0)
            .targetedToAnyEntity()
            .onChanged { value in
                send(value, phase: state.pinching ? .moved : .began)
                state.pinching = true
            }
            .onEnded { value in
                send(value, phase: .ended)
                state.pinching = false
            }
    }

    /// A pinch on the picture goes to the host, unless the ring is open: then it closes the ring,
    /// as the window's scrim does, and none of it reaches the host.
    private func send(_ value: EntityTargetValue<DragGesture.Value>, phase: CurvePointerPhase) {
        guard let shape = state.shape, value.entity.components.has(ModelComponent.self) else { return }
        if phase == .began, ring.visible {
            if ring.sheet { ring.sheet = false } else { ring.close() }
            state.swallowing = true
        }
        if state.swallowing {
            if phase == .ended { state.swallowing = false }
            return
        }
        let local = value.convert(value.location3D, from: .local, to: value.entity)
        let uv = shape.uv(at: local)
        target.pointer?.curvePointer(u: Double(uv.x), v: Double(uv.y), phase: phase)
    }

    /// Two hands turning the screen: the ring's dial, opened at its own frame's centre.
    private var twist: some Gesture {
        RotateGesture3D(constrainedToAxis: .z)
            .targetedToAnyEntity()
            .onChanged { value in
                let degrees = value.rotation.angle.degrees
                ring.handle(.turn(
                    progress: min(abs(degrees) / Self.commitDegrees, 1),
                    clockwise: degrees < 0,
                    at: CGPoint(x: Self.ringSide / 2, y: Self.ringSide / 2)))
            }
            .onEnded { value in
                let committed = abs(value.rotation.angle.degrees) >= Self.commitDegrees
                ring.handle(committed ? .commit : .cancel)
            }
    }
}

private extension View {
    /// Report this view's laid-out size.
    func measured(_ report: @escaping (CGSize) -> Void) -> some View {
        onGeometryChange(for: CGSize.self, of: { $0.size }, action: report)
    }
}

/// The volume's UIKit view, handed to the session for its window and the soft keyboard.
private struct Anchor: UIViewRepresentable {
    let target: CurveTarget

    func makeUIView(context: Context) -> CurveAnchor {
        let view = CurveAnchor()
        target.anchor = view
        return view
    }

    func updateUIView(_ view: CurveAnchor, context: Context) {}
}

/// The picture flat in the session's window: the texture the curve shows, on a plane that fits
/// the window at the picture's shape. It takes no input; the stream view under it keeps every
/// touch, so a touch reaches the host where it lands.
struct FlatScreen: View {
    /// How far the window's 2D overlays lift, points: past the plane's millimetre (~1.4 pt).
    static let overlayLift: CGFloat = 4
    let target: CurveTarget
    @Environment(\.physicalMetrics) private var metrics

    var body: some View {
        GeometryReader { proxy in
            let bounds = metrics.convert(proxy.size, to: .meters)
            let texture = target.texture, aspect = target.aspect
            RealityView { content in
                content.add(ModelEntity())
            } update: { content in
                guard let plane = content.entities.first as? ModelEntity, let texture, let aspect
                else { return }
                // The picture's shape fitted inside the window, as the layer's aspect fit was.
                let width = Float(min(bounds.width, bounds.height * CGFloat(aspect)))
                plane.model = ModelComponent(
                    mesh: .generatePlane(width: width, height: width / Float(aspect)),
                    materials: [UnlitMaterial.picture(texture)])
                // A millimetre in front of the window's own layers, so the two never share a depth.
                plane.position = [0, 0, 0.001]
            }
        }
        .allowsHitTesting(false)
    }
}

private extension UnlitMaterial {
    /// The stream's picture, unlit. Clamped at the edges: RealityKit's default sampler repeats,
    /// and its linear filter would blend each edge's texels into the opposite edge.
    static func picture(_ texture: TextureResource) -> UnlitMaterial {
        var sampler = MaterialParameters.Texture.Sampler()
        sampler.modify {
            $0.sAddressMode = .clampToEdge
            $0.tAddressMode = .clampToEdge
        }
        var material = UnlitMaterial(applyPostProcessToneMap: false)
        material.color = .init(tint: .white, texture: .init(texture, sampler: sampler))
        return material
    }
}

/// What the stream's window shows while its picture is on the curve. The ornament's slider at
/// Flat brings it back.
struct ScreenPlaceholder: View {
    let endStream: () -> Void

    var body: some View {
        VStack(spacing: 20) {
            Image(systemName: "pano")
                .font(.system(size: 44))
            Text(L10n.appleCurveStreaming)
                .font(.title2)
            Button(L10n.appleCurveEndStream, systemImage: "xmark", role: .destructive, action: endStream)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
    }
}

/// The screen's RealityKit side: its mesh and where the overlays lie on it.
extension CurveShape {
    static let columns = 64

    /// Where an overlay `size` metres across floats, and how it turns: the HUD in its corner, the
    /// rest on the centre line. Each lies tangent to the surface at its centre, out by its own
    /// width's sag plus a lift, so no edge dips behind the picture; the ring floats further out.
    fileprivate func pose(
        of overlay: CurveOverlay, size: SIMD2<Float>, placement: HUDPlacement
    ) -> (position: SIMD3<Float>, orientation: simd_quatf) {
        let inset: Float = 0.02
        let lift: Float = overlay == .ring ? Self.ringLift : 0.005
        var x: Float = 0, y: Float = 0
        switch overlay {
        case .hud:
            let trailing = placement == .topTrailing || placement == .bottomTrailing
            let top = placement == .topLeading || placement == .topTrailing
            x = (trailing ? 1 : -1) * max(halfChord - size.x / 2 - inset, 0)
            y = (top ? 1 : -1) * max(height / 2 - size.y / 2 - inset, 0)
        case .badges:
            y = -max(height / 2 - size.y / 2 - inset, 0)
        case .ring, .resize:
            break
        }
        // The mesh's normal at angle a is (-sin a, 0, cos a); a turn of -a about y faces it.
        let angle = radius.map { asin(min(max(x / $0, -1), 1)) } ?? 0
        let normal = SIMD3<Float>(-sin(angle), 0, cos(angle))
        let out = (radius.map { size.x * size.x / (8 * $0) } ?? 0) + lift
        return (
            SIMD3(x, y, surfaceZ(atX: x)) + normal * out,
            simd_quatf(angle: -angle, axis: [0, 1, 0]))
    }

    /// Two rows of vertices, u linear in angle (in x when flat) and v in height (1 at the top).
    func mesh() throws -> MeshResource {
        let columns = radius == nil ? 1 : Self.columns
        var positions: [SIMD3<Float>] = []
        var normals: [SIMD3<Float>] = []
        var uvs: [SIMD2<Float>] = []
        for row in 0...1 {
            let y = (Float(row) - 0.5) * height
            for column in 0...columns {
                let u = Float(column) / Float(columns)
                let angle = (u * 2 - 1) * halfAngle
                let x = radius.map { $0 * sin(angle) } ?? (u * 2 - 1) * halfChord
                positions.append([x, y, surfaceZ(atX: x)])
                normals.append([-sin(angle), 0, cos(angle)])
                uvs.append([u, Float(row)])
            }
        }
        var indices: [UInt32] = []
        let stride = UInt32(columns + 1)
        for column in 0..<UInt32(columns) {
            let bottomLeft = column, bottomRight = column + 1
            let topLeft = column + stride, topRight = column + 1 + stride
            indices += [bottomLeft, bottomRight, topRight, bottomLeft, topRight, topLeft]
        }
        var descriptor = MeshDescriptor(name: "curve")
        descriptor.positions = MeshBuffers.Positions(positions)
        descriptor.normals = MeshBuffers.Normals(normals)
        descriptor.textureCoordinates = MeshBuffers.TextureCoordinates(uvs)
        descriptor.primitives = .triangles(indices)
        return try MeshResource.generate(from: [descriptor])
    }
}
#endif

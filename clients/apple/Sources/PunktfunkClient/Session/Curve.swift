// visionOS: the stream's picture on a screen inside the main volume, flat or curved. The
// presenter draws into a CurveTarget; the session's stream view stays, hidden, for input.

#if os(visionOS)
import PunktfunkKit
import RealityKit
import SwiftUI

extension StreamView {
    /// The picture on the volume's screen. The stream view stays at 1×1 pt, invisible and out of
    /// hit-testing, for the keyboard, mouse and controllers. `pinched` fires as each pinch on the
    /// picture begins.
    func onCurveScreen(
        _ target: CurveTarget, model: SessionModel, ring: RingState, connection: PunktfunkConnection,
        pinched: @escaping () -> Void
    ) -> some View {
        ZStack {
            CurveScreen(
                target: target, model: model, ring: ring, connection: connection, pinched: pinched)
            curve(target)
                .frame(width: 1, height: 1)
                .opacity(0)
                .allowsHitTesting(false)
        }
    }
}

/// What a window would draw over the picture, laid on the screen as attachments.
fileprivate enum CurveOverlay: String, CaseIterable {
    case hud, badges, ring, resize
}

/// The picture across the volume, bent to the stored Screen stop; a new stop rebuilds the mesh
/// live. Once a resize or a new picture settles, the box snaps to the picture's shape at its
/// width, and the corner handle stays free. A pinch on it reaches the host through the session's
/// stream view, or closes the ring while it is open; a two-hand twist turns the ring, and the
/// session's overlays lie along it, just in front.
private struct CurveScreen: View {
    let target: CurveTarget
    @ObservedObject var model: SessionModel
    @ObservedObject var ring: RingState
    let connection: PunktfunkConnection
    /// A pinch began on the picture: the controls show for a moment.
    let pinched: () -> Void
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.statsScalePct) private var statsScalePct = 100
    @Environment(\.physicalMetrics) private var metrics
    /// What the screen's mesh and material were last built for, and the pinch in progress.
    @State private var state = CurveState()
    /// Each overlay's laid-out size in points, for placing it on the curve.
    @State private var sizes: [CurveOverlay: CGSize] = [:]
    /// The box's width in points.
    @State private var width: CGFloat = 0
    /// The height the box snaps to, set only for the snap's moment.
    @State private var snap: CGFloat?

    final class CurveState {
        var shape: CurveShape?
        var texture: TextureResource?
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
                // The trust prompt has the volume to itself; the picture shows once it streams.
                screen.isEnabled = model.phase == .streaming
                let shape = CurveShape(
                    box: SIMD3(Float(box.width), Float(box.height), Float(box.depth)),
                    aspect: Float(aspect), radius: radius)
                if shape != state.shape || texture !== state.texture, let mesh = try? shape.mesh() {
                    var material = UnlitMaterial(applyPostProcessToneMap: false)
                    material.color = .init(tint: .white, texture: .init(texture))
                    screen.model = ModelComponent(mesh: mesh, materials: [material])
                    screen.position = [0, 0, shape.front]
                    state.shape = shape
                    state.texture = texture
                    Task { @MainActor in
                        guard let collision = try? await ShapeResource.generateStaticMesh(from: mesh),
                              state.shape == shape
                        else { return }
                        screen.components.set(CollisionComponent(shapes: [collision]))
                    }
                }
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
        }
        // Whole points, so the height it sets cannot feed back into a new width.
        .onGeometryChange(for: CGFloat.self, of: { $0.size.width.rounded() }) { width = $0 }
        // A pinned height freezes the corner handle (a volume resizes in both axes), so the snap
        // is a moment: once a drag or a new picture has settled, the height is pinned for one
        // layout and let go again.
        .frame(minHeight: snap, maxHeight: snap)
        .task(id: [width, CGFloat(target.aspect ?? 0)]) {
            guard width > 0, let aspect = target.aspect else { return }
            try? await Task.sleep(for: .milliseconds(400))
            snap = (width / CGFloat(aspect)).rounded()
            try? await Task.sleep(for: .milliseconds(400))
            snap = nil
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

    /// A pinch on the picture shows the controls and goes to the host, unless the ring is open:
    /// then it closes the ring, as the window's scrim does, and none of it reaches the host.
    private func send(_ value: EntityTargetValue<DragGesture.Value>, phase: CurvePointerPhase) {
        guard let shape = state.shape, value.entity.components.has(ModelComponent.self) else { return }
        if phase == .began { pinched() }
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

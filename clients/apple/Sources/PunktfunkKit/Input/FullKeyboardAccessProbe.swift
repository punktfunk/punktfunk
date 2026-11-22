// Notices Full Keyboard Access outside a stream.
//
// Full Keyboard Access walks the view tree as it moves between controls, and the request to
// each view it asks names it (iOS 18). No API reports the setting, so an empty view in the
// tree that reads that name is how the app learns of it before a stream starts.

#if os(iOS) || os(visionOS)
import Accessibility
import GameController
import SwiftUI
import UIKit

/// Put one behind a screen's content. It draws nothing and takes no touches.
public struct FullKeyboardAccessProbe: UIViewRepresentable {
    public init() {}

    public func makeUIView(context: Context) -> UIView { ProbeView() }
    public func updateUIView(_ view: UIView, context: Context) {}

    private final class ProbeView: UIView {
        private var connect: NSObjectProtocol?

        override init(frame: CGRect) {
            super.init(frame: frame)
            isUserInteractionEnabled = false
            // A proof stored by an earlier run is checked against this run's keyboard.
            FullKeyboardAccess.watchGameController()
            connect = NotificationCenter.default.addObserver(
                forName: .GCKeyboardDidConnect, object: nil, queue: .main
            ) { _ in MainActor.assumeIsolated { FullKeyboardAccess.watchGameController() } }
        }

        deinit {
            if let connect { NotificationCenter.default.removeObserver(connect) }
        }

        required init?(coder: NSCoder) { fatalError("not used") }

        override var isAccessibilityElement: Bool {
            get {
                note()
                return false
            }
            set {}
        }

        override var accessibilityElements: [Any]? {
            get {
                note()
                return nil
            }
            set {}
        }

        private func note() {
            guard #available(iOS 18.0, *), !FullKeyboardAccess.proven,
                  AccessibilityRequest.current?.technology == .fullKeyboardAccess
            else { return }
            DispatchQueue.main.async { FullKeyboardAccess.proven = true }
        }
    }
}
#endif

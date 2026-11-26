// The keyboard row on iPhone and iPad: the stream view's input accessory, so the system puts it
// on whatever keyboard it shows, moves it with that keyboard and takes it away with it. The
// buttons are the quick actions' `row`, and `KeyRowEngine` decides what each one sends. visionOS
// has no input accessory views, so it has no row.

#if os(iOS)
import PunktfunkShared
import UIKit

/// What the session lends the row: how a quick action looks right now, and the path that runs one.
public struct KeyRowClient {
    public struct Face {
        /// An SF Symbol; `nil` draws `label` as the cap's words.
        public var symbol: String?
        public var label: String
        public var enabled: Bool

        public init(symbol: String? = nil, label: String, enabled: Bool = true) {
            self.symbol = symbol
            self.label = label
            self.enabled = enabled
        }
    }

    public var face: (SlotId) -> Face
    public var fire: (Shortcuts.Action, Shortcuts.Phase) -> Void

    public init(face: @escaping (SlotId) -> Face,
                fire: @escaping (Shortcuts.Action, Shortcuts.Phase) -> Void) {
        self.face = face
        self.fire = fire
    }
}

/// A scroll view that still scrolls when the drag starts on a button. UIKit otherwise keeps a
/// touch that began on a control with that control.
private final class RowScrollView: UIScrollView {
    override func touchesShouldCancel(in view: UIView) -> Bool { true }
}

/// One cap as drawn: its legend or glyph, lit while its modifier is latched, dim while its
/// action can't run.
private struct CapFace: Equatable {
    var title: String?
    var symbol: String?
    var label: String
    var lit = false
    var enabled = true
}

final class KeyRowView: UIInputView, UIInputViewAudioFeedback {
    /// The row's height: a toolbar's 44 points.
    private static let barHeight: CGFloat = 44

    /// The legend of a lit cap. The cap fills with the label colour, so its legend takes the
    /// opposite.
    private static let litInk = UIColor { $0.userInterfaceStyle == .dark ? .black : .white }

    /// Clicks follow the system's Keyboard Clicks setting.
    var enableInputClicksWhenVisible: Bool { true }

    var client: KeyRowClient? { didSet { refresh() } }
    /// Send one key down or up, or committed text, to the host. The stream view's trust gate
    /// applies.
    var sendKey: ((UInt32, Bool) -> Void)?
    var sendText: ((String) -> Void)?
    /// The player asked for the keyboard to go.
    var hide: (() -> Void)?

    private let buttons: [RowSlot]
    private var caps: [UIButton] = []
    /// What each cap last drew, so a redraw touches only a cap that changed.
    private var drawn: [CapFace?] = []
    private var engine = KeyRowEngine()
    private var resignObserver: NSObjectProtocol?

    init(buttons: [RowSlot]) {
        self.buttons = buttons
        // Before iOS 26 the keyboard's own backdrop runs under the row. From 26 the keyboard
        // floats with rounded corners, and a backdrop above it would be a slab: the row floats too.
        let style: UIInputView.Style = if #available(iOS 26, *) { .default } else { .keyboard }
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: Self.barHeight + 12), inputViewStyle: style)
        allowsSelfSizing = true
        translatesAutoresizingMaskIntoConstraints = false
        build()
        // A backgrounded app shows no keyboard: nothing may stay held on the host meanwhile.
        resignObserver = NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.releaseAll() }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    deinit {
        if let resignObserver { NotificationCenter.default.removeObserver(resignObserver) }
    }

    // MARK: Layout

    private func build() {
        let scroll = RowScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.alwaysBounceHorizontal = true

        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 2
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)

        for i in buttons.indices {
            let cap = makeCap(tag: i)
            let press = UILongPressGestureRecognizer(target: self, action: #selector(longPressed(_:)))
            cap.addGestureRecognizer(press)
            cap.addTarget(self, action: #selector(tapped(_:)), for: .touchUpInside)
            caps.append(cap)
            stack.addArrangedSubview(cap)
        }

        // The buttons scroll in one pane; the hide button is pinned beside it in its own, since
        // an iPhone's keyboard has no key that hides it.
        let strip = pane(holding: scroll, interactive: false)
        let hidePane = pane(holding: makeHideButton(), interactive: true)
        addSubview(strip)
        addSubview(hidePane)

        let safe = safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            strip.leadingAnchor.constraint(equalTo: safe.leadingAnchor, constant: 8),
            strip.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            strip.heightAnchor.constraint(equalToConstant: Self.barHeight),
            // Alone at the screen's bottom, beside a hardware keyboard, the row clears the home bar.
            strip.bottomAnchor.constraint(equalTo: safe.bottomAnchor, constant: -6),
            hidePane.leadingAnchor.constraint(equalTo: strip.trailingAnchor, constant: 8),
            hidePane.trailingAnchor.constraint(equalTo: safe.trailingAnchor, constant: -8),
            hidePane.centerYAnchor.constraint(equalTo: strip.centerYAnchor),
            hidePane.widthAnchor.constraint(equalToConstant: Self.barHeight),
            hidePane.heightAnchor.constraint(equalToConstant: Self.barHeight),

            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
        ])
        drawn = Array(repeating: nil, count: caps.count)
        refresh()
    }

    /// A capsule around `content`. From iOS 26 it is glass in the keyboard's own tone, so a
    /// legend holds over a bright picture as the keyboard's keys do. Before, it is bare: the
    /// keyboard's backdrop runs under the whole row.
    private func pane(holding content: UIView, interactive: Bool) -> UIView {
        let pane: UIView
        if #available(iOS 26, *) {
            let effect = UIGlassEffect(style: .regular)
            effect.tintColor = UIColor.systemBackground.withAlphaComponent(0.45)
            effect.isInteractive = interactive
            let glass = UIVisualEffectView(effect: effect)
            glass.cornerConfiguration = .capsule()
            glass.clipsToBounds = true
            glass.contentView.addSubview(content)
            pane = glass
        } else {
            pane = UIView()
            pane.addSubview(content)
        }
        pane.translatesAutoresizingMaskIntoConstraints = false
        content.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            content.leadingAnchor.constraint(equalTo: pane.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: pane.trailingAnchor),
            content.topAnchor.constraint(equalTo: pane.topAnchor),
            content.bottomAnchor.constraint(equalTo: pane.bottomAnchor),
        ])
        return pane
    }

    /// One button's cap: the system's plain button. A latched modifier inverts, as the system
    /// keyboard's Shift does while it is on, and VoiceOver reads it as selected. An action that
    /// can't run is a disabled button: it takes no tap and VoiceOver reads it as dimmed.
    private func makeCap(tag: Int) -> UIButton {
        var config = UIButton.Configuration.plain()
        config.contentInsets = NSDirectionalEdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 12)
        config.cornerStyle = .capsule
        config.titleLineBreakMode = .byTruncatingTail
        config.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
            var attributes = attributes
            attributes.font = .systemFont(ofSize: 16, weight: .medium)
            return attributes
        }
        config.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(pointSize: 16, weight: .medium)
        let cap = UIButton(configuration: config)
        cap.tag = tag
        cap.configurationUpdateHandler = { button in
            var config = button.configuration ?? .plain()
            let ink: UIColor = button.isSelected ? Self.litInk : .label
            config.baseForegroundColor = button.isEnabled ? ink : .tertiaryLabel
            config.background.backgroundColor = button.isSelected ? .label : .clear
            button.configuration = config
        }
        NSLayoutConstraint.activate([
            cap.widthAnchor.constraint(greaterThanOrEqualToConstant: 44),
            cap.widthAnchor.constraint(lessThanOrEqualToConstant: 180),
        ])
        return cap
    }

    private func makeHideButton() -> UIButton {
        var config = UIButton.Configuration.plain()
        config.image = UIImage(systemName: "keyboard.chevron.compact.down")
        config.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(pointSize: 16, weight: .medium)
        config.baseForegroundColor = .label
        config.cornerStyle = .capsule
        let button = UIButton(configuration: config, primaryAction: UIAction { [weak self] _ in
            UIDevice.current.playInputClick()
            self?.hide?()
        })
        button.accessibilityLabel = String(localized: L10n.controlsHideKeyboard)
        return button
    }

    // MARK: Faces

    /// Redraw each cap whose face changed: a latched modifier lit, each action as the session
    /// says it looks now. At once: UIKit otherwise cross-fades a cap's legend, which reads white
    /// on the lit cap's white for the length of the fade.
    func refresh() {
        UIView.performWithoutAnimation {
            for (i, cap) in caps.enumerated() {
                let face = face(buttons[i])
                guard drawn[i] != face else { continue }
                drawn[i] = face
                var config = cap.configuration ?? .plain()
                config.title = face.title
                config.image = face.symbol.flatMap { UIImage(systemName: $0) }
                cap.configuration = config
                cap.accessibilityLabel = face.label
                cap.isSelected = face.lit
                cap.isEnabled = face.enabled
                cap.layoutIfNeeded()
            }
        }
    }

    private func face(_ button: RowSlot) -> CapFace {
        switch button {
        case .key(let name):
            let legend = keyLegend(name)
            return CapFace(title: legend, label: legend, lit: engine.isLatched(button))
        case .action(.keyboard):
            // On the row the Keyboard action hides the keyboard the row sits on.
            return CapFace(symbol: "keyboard.chevron.compact.down",
                           label: String(localized: L10n.controlsHideKeyboard))
        case .action(let action):
            let face = client?.face(action) ?? KeyRowClient.Face(label: action.id)
            // A glyph the system lacks falls back to the action's name.
            let symbol = face.symbol.flatMap { UIImage(systemName: $0) == nil ? nil : $0 }
            return CapFace(title: symbol == nil ? face.label : nil, symbol: symbol,
                           label: face.label, enabled: face.enabled)
        }
    }

    // MARK: Presses

    @objc private func tapped(_ cap: UIButton) {
        UIDevice.current.playInputClick()
        run(engine.tap(buttons[cap.tag]))
    }

    @objc private func longPressed(_ press: UILongPressGestureRecognizer) {
        guard let cap = press.view as? UIButton else { return }
        switch press.state {
        case .began:
            UIDevice.current.playInputClick()
            run(engine.hold(buttons[cap.tag]))
        case .ended, .cancelled, .failed:
            run(engine.lift(buttons[cap.tag]))
        default:
            break
        }
    }

    /// Text went through the system keyboard. True when a latched modifier took it: the row
    /// sent it, as a key press under the modifier or as text once the modifier was up.
    func typed(_ text: String) -> Bool {
        guard let out = engine.type(text) else { return false }
        run(out)
        return true
    }

    /// A key the row did not send lifted on the host. It was the next key, so the latched
    /// modifiers lift.
    func keyLifted() {
        run(engine.release())
    }

    /// The keyboard hid, the app left the foreground or the stream ended: nothing stays down.
    func releaseAll() {
        run(engine.end())
    }

    private func run(_ out: [KeyRowOut]) {
        for step in out {
            switch step {
            case .key(let vk, let down): sendKey?(vk, down)
            case .text(let text): sendText?(text)
            case .fire(.slot(.keyboard), _): hide?()
            case .fire(let action, let phase): client?.fire(action, phase)
            }
        }
        refresh()
    }
}
#endif

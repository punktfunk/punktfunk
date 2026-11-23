// The host UI without the host.
//   HostUIPreview --render <dir>     every screen and state as PNG, light and dark, English and
//                                    German, into <dir>/<language>/; nothing shows on screen
//   HostUIPreview --canned <state>   the menu bar app on one canned state (Preview.scenarios)
//   HostUIPreview                    the menu bar app against the management port in
//                                    $PUNKTFUNK_CONFIG_DIR; $PUNKTFUNK_HOST_EXE names the host
//                                    binary for `ctl`
// The last two put a menu bar item on screen: run them in a VM. PUNKTFUNK_HOST_UI_OPEN=panel
// or onboarding opens one at launch.
import Foundation
import PunktfunkHostUI

let args = CommandLine.arguments
let env = ProcessInfo.processInfo.environment

func value(after flag: String) -> String? {
    args.firstIndex(of: flag).flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil }
}

if let dir = value(after: "--render") {
    // One child per language: the process's language is fixed at its start.
    for lang in ["en", "de"] {
        let child = Process()
        child.executableURL = URL(fileURLWithPath: args[0]).standardizedFileURL
        child.arguments = ["--render-one", dir + "/" + lang, "-AppleLanguages", "(\(lang))"]
        try child.run()
        child.waitUntilExit()
        guard child.terminationStatus == 0 else { exit(child.terminationStatus) }
    }
} else if let dir = value(after: "--render-one") {
    try MainActor.assumeIsolated { try Preview.render(to: URL(fileURLWithPath: dir)) }
} else if let name = value(after: "--canned") {
    // As the host does: SIGUSR1 shows the panel instead of ending the process.
    signal(SIGUSR1) { _ in }
    MainActor.assumeIsolated { Preview.runCanned(name) }
} else {
    signal(SIGUSR1) { _ in }
    MainActor.assumeIsolated {
        Preview.runLive(configDir: HostContext.defaultConfigDir(),
                        hostExecutable: env["PUNKTFUNK_HOST_EXE"].map { URL(fileURLWithPath: $0) })
    }
}

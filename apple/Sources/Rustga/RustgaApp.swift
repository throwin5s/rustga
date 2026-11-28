#if os(macOS)
import AppKit
#endif
import SwiftUI

@main
struct RustgaApp: App {
    #if os(macOS)
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    #endif

    var body: some Scene {
        WindowGroup("rustga") {
            ContentView()
        }
        #if os(macOS)
        .windowStyle(.hiddenTitleBar)
        #endif
    }
}

#if os(macOS)
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // A bare executable has no app bundle, so AppKit defaults it to
        // `.prohibited` and its windows cannot be focused or brought forward.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate()
    }
}
#endif

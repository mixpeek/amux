// overlay <x> <y> <w> <h> <level>: a borderless window from ANOTHER process at
// a window level, until killed. Stands in for the system's overlays above
// app windows (the Dock's full-screen window at layer 20, a tab drag image at
// the dragging level) in desktop/test/pane-grid-e2e.py.
import Cocoa
let a = CommandLine.arguments.dropFirst().compactMap { Double($0) }
let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let w = NSWindow(contentRect: NSRect(x: a[0], y: a[1], width: a[2], height: a[3]), styleMask: .borderless, backing: .buffered, defer: false)
w.level = NSWindow.Level(rawValue: Int(a[4]))
w.backgroundColor = NSColor.gray.withAlphaComponent(0.3)
w.isOpaque = false
w.ignoresMouseEvents = true
w.orderFrontRegardless()
print("overlay up"); fflush(stdout)
app.run()

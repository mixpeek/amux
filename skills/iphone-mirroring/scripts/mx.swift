// Real mouse events for iPhone Mirroring (skills/iphone-mirroring.md).
//   mx click X Y           left click at screen point X,Y
//   mx press X Y [SECS]    long press (default 1 s)
//   mx scroll X Y DY       trackpad-style scroll at X,Y; DY < 0 moves content up (scrolls down)
// System Events "click at" reaches the window's accessibility group but never the phone, and plain
// wheel or drag events are ignored: the phone only scrolls on continuous events with phases.
import Foundation
import CoreGraphics

let a = CommandLine.arguments
guard a.count >= 4, let x = Double(a[2]), let y = Double(a[3]) else {
  FileHandle.standardError.write("usage: mx click X Y | mx press X Y [SECS] | mx scroll X Y DY\n".data(using: .utf8)!); exit(2)
}
let p = CGPoint(x: x, y: y)
func post(_ e: CGEvent?) { e?.post(tap: .cghidEventTap) }
post(CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: p, mouseButton: .left))
usleep(100_000)
switch a[1] {
case "click":
  post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: p, mouseButton: .left))
  usleep(80_000)
  post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: p, mouseButton: .left))
case "press":                       // long press: mx press X Y [seconds]
  let secs = Double(a.count > 4 ? a[4] : "1.0") ?? 1.0
  post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: p, mouseButton: .left))
  usleep(useconds_t(secs * 1_000_000))
  post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: p, mouseButton: .left))
case "scroll":
  let dy = Int32(a.count > 4 ? a[4] : "-40") ?? -40
  for i in 0..<25 {
    guard let e = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1, wheel1: dy, wheel2: 0, wheel3: 0) else { continue }
    e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
    e.setIntegerValueField(CGEventField(rawValue: 99)!, value: i == 0 ? 1 : (i == 24 ? 4 : 2))   // scroll phase: began, changed, ended
    e.location = p
    e.post(tap: .cghidEventTap)
    usleep(16_000)
  }
default:
  FileHandle.standardError.write("unknown verb \(a[1])\n".data(using: .utf8)!); exit(2)
}

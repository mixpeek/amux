// Unit check of desktop/amux-snapzones.swift: which zone a drag lands in for
// each spot on a display, and the name a saved frame gets.
//   xcrun swiftc -o /tmp/snapzones desktop/amux-snapzones.swift desktop/test/snapzones-test.swift && /tmp/snapzones
import Cocoa

var fails = 0
func check(_ label: String, _ want: String?, _ got: String?) {
    if want == got { print("  ok   \(label)") } else { print("  FAIL \(label): expected \(want ?? "nil"), got \(got ?? "nil")"); fails += 1 }
}

@main struct SnapZonesTest {
    static func main() {
        // A 1512x982 display: menu bar 37 at the top, Dock 60 at the bottom.
        let sf = NSRect(x: 0, y: 0, width: 1512, height: 982)
        let vf = NSRect(x: 0, y: 60, width: 1512, height: 885)
        func z(_ x: CGFloat, _ y: CGFloat) -> String? { snapZone(cursor: NSPoint(x: x, y: y), screen: sf, visible: vf)?.name }
        check("middle of the display is no zone", nil, z(756, 500))
        check("left edge is the left half", "left half", z(0, 500))
        check("right edge is the right half", "right half", z(1511, 500))
        check("top edge, over the menu bar, fills", "full screen", z(756, 970))
        check("top left corner is a quarter", "top left", z(2, 975))
        check("top right corner from the top edge", "top right", z(1450, 975))
        check("bottom left corner from the left edge", "bottom left", z(0, 100))
        check("bottom right corner", "bottom right", z(1511, 70))
        check("bottom edge, left part, is the left third", "left third", z(300, 62))
        check("bottom edge, middle, is the center third", "center third", z(756, 30))
        check("bottom edge, right part, is the right third", "right third", z(1200, 62))
        check("just inside the edge is no zone", nil, z(20, 500))
        check("a cursor on another display is no zone", nil, z(2000, 500))

        let half = SnapZone.leftHalf.frame(in: vf)
        check("the left half frame", NSStringFromRect(NSRect(x: 0, y: 60, width: 756, height: 885)), NSStringFromRect(half))
        let third = SnapZone.rightThird.frame(in: vf)
        check("the right third ends at the display edge", "1512.0", String(describing: Double(third.maxX)))

        // A second display to the right with an offset origin.
        let sf2 = NSRect(x: 1512, y: -200, width: 1920, height: 1080), vf2 = NSRect(x: 1512, y: -200, width: 1920, height: 1055)
        check("zones follow the display the cursor is on", "right half",
              snapZone(cursor: NSPoint(x: 3431, y: 300), screen: sf2, visible: vf2)?.name)

        check("a saved left half is named", "left half", zoneName(rel: "0.0000,0.0000,0.5000,1.0000"))
        check("a saved top right quarter is named", "top right", zoneName(rel: "0.5000,0.5000,0.5000,0.5000"))
        check("a free-form frame has no name", nil, zoneName(rel: "0.1200,0.3000,0.4000,0.5000"))
        check("no saved frame has no name", nil, zoneName(rel: nil))

        print(fails == 0 ? "PASS: snapzones, all checks passed" : "snapzones: \(fails) check(s) FAILED")
        exit(fails == 0 ? 0 : 1)
    }
}

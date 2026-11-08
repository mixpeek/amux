// Unit check of desktop/amux-tabcolors.swift: every server gets its own tab color, and keeps it.
//   xcrun swiftc -parse-as-library -o /tmp/tabcolors desktop/amux-tabcolors.swift desktop/test/tabcolors-test.swift && /tmp/tabcolors
import Foundation

var fails = 0
func check(_ label: String, _ want: String, _ got: String) {
    if want == got { print("  ok   \(label)") } else { print("  FAIL \(label): expected \(want), got \(got)"); fails += 1 }
}

@main struct TabColorsTest {
    static func main() {
        var a: [String: Int] = [:]
        func color(_ s: String) -> String { let r = tabColorIndex(for: s, assigned: a); a = r.assigned; return tabColorNames[r.index] }

        // The 2026-10-31 report: sandbox and mixpeek-dev were both blue.
        check("the first server keeps blue", "blue", color("sandbox.amux.io:0"))
        check("a second amux.io server gets its own color", "orange", color("mixpeek-dev.amux.io:0"))
        check("a server seen again keeps its color", "blue", color("sandbox.amux.io:0"))
        check("a third server takes the next free one", "purple", color("tubescience.amux.io:0"))

        // Saved and loaded again (a relaunch): nothing moves, and the next new server is still distinct.
        let saved = a
        a = [:]; a = saved
        check("after a relaunch mixpeek-dev is still orange", "orange", color("mixpeek-dev.amux.io:0"))
        check("a new server after a relaunch takes a free color", "pink", color("box.example.com:8822"))

        // A color freed by a forgotten server is reused before any is shared.
        a.removeValue(forKey: "tubescience.amux.io:0")
        check("a freed color is taken again", "purple", color("other.example.com:0"))

        // Every server distinct until the palette runs out.
        var b: [String: Int] = [:]
        for n in 0..<tabColorNames.count { b = tabColorIndex(for: "s\(n)", assigned: b).assigned }
        check("one color per server across the whole palette", "\(tabColorNames.count)", "\(Set(b.values).count)")
        let over = tabColorIndex(for: "one-too-many", assigned: b)
        check("past the palette the pick is stable", "\(over.index)", "\(tabColorIndex(for: "one-too-many", assigned: b).index)")

        // A saved index from a longer palette is replaced, not used out of range.
        let bad = tabColorIndex(for: "x", assigned: ["x": 99])
        check("an out-of-range saved color is replaced", "blue", tabColorNames[bad.index])

        print(fails == 0 ? "tabcolors: all passed" : "tabcolors: \(fails) failed")
        exit(fails == 0 ? 0 : 1)
    }
}

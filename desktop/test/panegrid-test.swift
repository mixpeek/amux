// Unit check of desktop/amux-panegrid.swift: where a tab dropped on a pane
// goes, how the grid grows and shrinks, and how it is saved.
//   xcrun swiftc -parse-as-library -o /tmp/panegrid desktop/amux-panegrid.swift desktop/test/panegrid-test.swift && /tmp/panegrid
import Foundation
import CoreGraphics

var fails = 0
func check(_ label: String, _ want: String?, _ got: String?) {
    if want == got { print("  ok   \(label)") } else { print("  FAIL \(label): expected \(want ?? "nil"), got \(got ?? "nil")"); fails += 1 }
}

@main struct PaneGridTest {
    static func main() {
        // Drop zones on a wide pane (a 1400x860 window's content).
        func e(_ u: CGFloat, _ v: CGFloat, _ w: CGFloat = 1400, _ h: CGFloat = 860) -> String {
            let d = paneDropEdge(u: u, v: v, width: w, height: h)
            return "\(d.edge.rawValue) \(d.name)"
        }
        check("left part is the left half", "left left", e(0.1, 0.5))
        check("right part is the right half", "right right", e(0.9, 0.5))
        check("top part is the top half", "top top", e(0.5, 0.9))
        check("bottom part is the bottom half", "bottom bottom", e(0.5, 0.1))
        check("the middle joins the pane's tabs", "center center", e(0.6, 0.5))
        check("just outside the middle goes to the nearest edge", "right right", e(0.75, 0.5))
        let full = paneDropEdge(u: 0.95, v: 0.5, width: 1400, height: 860, full: true)
        check("a full window only joins, even at an edge", "center center", "\(full.edge.rawValue) \(full.name)")
        check("a corner of a wide pane splits it side by side", "right top right", e(0.9, 0.9))
        check("a corner of a tall pane splits it top and bottom", "top top right", e(0.9, 0.9, 700, 860))
        check("bottom left corner of a tall pane", "bottom bottom left", e(0.1, 0.1, 700, 860))

        let pane = CGRect(x: 100, y: 50, width: 800, height: 600)
        check("the right half preview", "(500.0, 50.0, 400.0, 600.0)", fmt(paneDropRect(pane, edge: .right)))
        check("the top half preview sits on top (y up)", "(100.0, 350.0, 800.0, 300.0)", fmt(paneDropRect(pane, edge: .top)))
        check("a join previews the whole pane, inset", "(124.0, 74.0, 752.0, 552.0)", fmt(paneDropRect(pane, edge: .center)))
        check("a join leaves the layout alone", "0", PaneTree.leaf(0).inserting(1, at: 0, edge: .center).encoded)

        // Growing: one page, a tab dropped on its right, then on the right pane's top.
        var t = PaneTree.leaf(0)
        t = t.inserting(1, at: 0, edge: .right)
        check("two panes side by side", "r0.500(0,1)", t.encoded)
        t = t.inserting(2, at: 1, edge: .top)
        check("the right pane split, the new one on top", "r0.500(0,c0.500(2,1))", t.encoded)
        t = t.inserting(3, at: 0, edge: .bottom)
        check("four panes", "r0.500(c0.500(0,3),c0.500(2,1))", t.encoded)
        check("leaves in reading order", "[0, 3, 2, 1]", "\(t.leaves)")
        check("inserting at a pane that is not there changes nothing", t.encoded, t.inserting(9, at: 7, edge: .left).encoded)

        let r = t.rects(in: CGRect(x: 0, y: 0, width: 1000, height: 800))
        check("pane 2 is the top right quarter", "(500.0, 400.0, 500.0, 400.0)", fmt(r[2]!))
        check("pane 3 is the bottom left quarter", "(0.0, 0.0, 500.0, 400.0)", fmt(r[3]!))

        // Shrinking: a pane's sibling takes its space; the last pane leaves no grid.
        check("closing pane 2 gives its space to pane 1", "r0.500(c0.500(0,3),1)", t.removing(2)!.encoded)
        check("closing both on the left leaves the right column", "c0.500(2,1)", t.removing(0)!.removing(3)!.encoded)
        check("the last pane removed is no grid", nil, PaneTree.leaf(4).removing(4)?.encoded)

        // Saving.
        let saved = PaneTree.split(row: true, fraction: 0.3, .leaf(0), .split(row: false, fraction: 0.62, .leaf(1), .leaf(2)))
        check("encoded with fractions", "r0.300(0,c0.620(1,2))", saved.encoded)
        check("decodes to the same tree", saved.encoded, PaneTree.decode(saved.encoded)?.encoded)
        check("a bare page decodes", "0", PaneTree.decode("0")?.encoded)
        check("a truncated grid does not decode", nil, PaneTree.decode("r0.5(0,")?.encoded)
        check("a pane named twice does not decode", nil, PaneTree.decode("r0.5(1,1)")?.encoded)
        check("trailing text does not decode", nil, PaneTree.decode("r0.5(0,1)x")?.encoded)
        check("renaming ids for saving", "r0.300(5,c0.620(6,7))", saved.mapped { $0 + 5 }.encoded)

        print(fails == 0 ? "PASS: panegrid, all checks passed" : "panegrid: \(fails) check(s) FAILED")
        exit(fails == 0 ? 0 : 1)
    }

    static func fmt(_ r: CGRect) -> String { "(\(Double(r.minX)), \(Double(r.minY)), \(Double(r.width)), \(Double(r.height)))" }
}

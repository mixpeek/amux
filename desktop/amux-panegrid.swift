import Foundation
import CoreGraphics

// The layout of a window split into panes (Ethan, 2026-10-23: "me dragging a
// tab into a section in the amux mac os app window should create the grid").
// A binary tree: each split puts two parts side by side (row) or one above the
// other (column); each leaf is one pane, named by an id the window assigns.
//
// Pure: no window, no view. amux-panes.swift draws it with split views;
// desktop/test/panegrid-test.swift checks it.

let maxPanes = 4

// center: the dropped tab joins the pane's own tabs instead of splitting it
// (Ethan, 2026-10-24: "my thing can have its own tabs and the others can have
// their own tabs").
enum PaneEdge: String { case left, right, top, bottom, center }

indirect enum PaneTree: Equatable {
    case leaf(Int)
    // row: first is left, second right. column: first is top, second bottom.
    case split(row: Bool, fraction: Double, PaneTree, PaneTree)

    var leaves: [Int] {
        switch self {
        case .leaf(let id): return [id]
        case .split(_, _, let a, let b): return a.leaves + b.leaves
        }
    }

    /// `new` beside `target` on its `edge`, the two sharing target's space evenly.
    func inserting(_ new: Int, at target: Int, edge: PaneEdge) -> PaneTree {
        switch self {
        case .leaf(let id) where id == target:
            switch edge {
            case .left: return .split(row: true, fraction: 0.5, .leaf(new), self)
            case .right: return .split(row: true, fraction: 0.5, self, .leaf(new))
            case .top: return .split(row: false, fraction: 0.5, .leaf(new), self)
            case .bottom: return .split(row: false, fraction: 0.5, self, .leaf(new))
            case .center: return self
            }
        case .leaf: return self
        case .split(let row, let f, let a, let b):
            return .split(row: row, fraction: f, a.inserting(new, at: target, edge: edge), b.inserting(new, at: target, edge: edge))
        }
    }

    /// The tree without `id`; its sibling takes the space. nil when `id` was the only pane.
    func removing(_ id: Int) -> PaneTree? {
        switch self {
        case .leaf(let l): return l == id ? nil : self
        case .split(let row, let f, let a, let b):
            guard let a2 = a.removing(id) else { return b }
            guard let b2 = b.removing(id) else { return a }
            return .split(row: row, fraction: f, a2, b2)
        }
    }

    /// Each pane's rectangle inside `r` (y up, as Cocoa draws a window).
    func rects(in r: CGRect) -> [Int: CGRect] {
        switch self {
        case .leaf(let id): return [id: r]
        case .split(let row, let f, let a, let b):
            let f = CGFloat(min(max(f, 0.05), 0.95))
            if row {
                let wa = r.width * f
                return a.rects(in: CGRect(x: r.minX, y: r.minY, width: wa, height: r.height))
                    .merging(b.rects(in: CGRect(x: r.minX + wa, y: r.minY, width: r.width - wa, height: r.height))) { x, _ in x }
            }
            let hb = r.height * (1 - f)   // the first child is on top
            return a.rects(in: CGRect(x: r.minX, y: r.minY + hb, width: r.width, height: r.height - hb))
                .merging(b.rects(in: CGRect(x: r.minX, y: r.minY, width: r.width, height: hb))) { x, _ in x }
        }
    }

    /// The same tree with ids renamed (for saving panes by their order).
    func mapped(_ f: (Int) -> Int) -> PaneTree {
        switch self {
        case .leaf(let id): return .leaf(f(id))
        case .split(let row, let fr, let a, let b): return .split(row: row, fraction: fr, a.mapped(f), b.mapped(f))
        }
    }

    // Saved as text: "r0.5(0,c0.5(1,2))", r = row, c = column.
    var encoded: String {
        switch self {
        case .leaf(let id): return String(id)
        case .split(let row, let f, let a, let b):
            return (row ? "r" : "c") + String(format: "%.3f", f) + "(" + a.encoded + "," + b.encoded + ")"
        }
    }

    static func decode(_ s: String) -> PaneTree? {
        var i = s.startIndex
        func node() -> PaneTree? {
            guard i < s.endIndex else { return nil }
            let c = s[i]
            if c == "r" || c == "c" {
                i = s.index(after: i)
                let start = i
                while i < s.endIndex, s[i] != "(" { i = s.index(after: i) }
                guard i < s.endIndex, let f = Double(s[start..<i]) else { return nil }
                i = s.index(after: i)
                guard let a = node(), i < s.endIndex, s[i] == "," else { return nil }
                i = s.index(after: i)
                guard let b = node(), i < s.endIndex, s[i] == ")" else { return nil }
                i = s.index(after: i)
                return .split(row: c == "r", fraction: f, a, b)
            }
            let start = i
            while i < s.endIndex, s[i].isNumber { i = s.index(after: i) }
            return Int(s[start..<i]).map { .leaf($0) }
        }
        guard let t = node(), i == s.endIndex, Set(t.leaves).count == t.leaves.count else { return nil }
        return t
    }
}

/// Where a tab dropped on a pane goes. The middle of the pane (the central
/// `middle` of each axis) joins its tabs. Elsewhere the cursor's spot in the
/// pane (u right, v up, each 0...1) picks the nearest edge, and the new pane
/// takes that half. A window that already has `maxPanes` only takes joins
/// (`full`), so a fifth tab lands in the pane under the pointer.
/// A corner splits the pane across its longer side, toward that corner: on a
/// wide pane the top-right corner is the right half, and on a pane that is
/// already a half (taller than wide) it is the top half, so two corner drops
/// make quarters.
func paneDropEdge(u: CGFloat, v: CGFloat, width: CGFloat, height: CGFloat, corner: CGFloat = 0.25,
                  middle: CGFloat = 0.4, full: Bool = false) -> (edge: PaneEdge, name: String) {
    let m = (1 - middle) / 2
    if full || (u > m && u < 1 - m && v > m && v < 1 - m) { return (.center, "center") }
    let left = u < 0.5, bottom = v < 0.5
    let inCorner = (u < corner || u > 1 - corner) && (v < corner || v > 1 - corner)
    if inCorner {
        let name = (bottom ? "bottom " : "top ") + (left ? "left" : "right")
        if width >= height { return (left ? .left : .right, name) }
        return (bottom ? .bottom : .top, name)
    }
    let d: [(CGFloat, PaneEdge)] = [(u, .left), (1 - u, .right), (v, .bottom), (1 - v, .top)]
    let e = d.min { $0.0 < $1.0 }!.1
    return (e, e.rawValue)
}

/// The half of `pane` a new pane on `edge` takes: what the drop preview shows.
func paneDropRect(_ pane: CGRect, edge: PaneEdge) -> CGRect {
    switch edge {
    case .left: return CGRect(x: pane.minX, y: pane.minY, width: pane.width / 2, height: pane.height)
    case .right: return CGRect(x: pane.midX, y: pane.minY, width: pane.width / 2, height: pane.height)
    case .top: return CGRect(x: pane.minX, y: pane.midY, width: pane.width, height: pane.height / 2)
    case .bottom: return CGRect(x: pane.minX, y: pane.minY, width: pane.width, height: pane.height / 2)
    // Joining: the whole pane, inset so it reads differently from a split.
    case .center: return pane.insetBy(dx: min(24, pane.width * 0.06), dy: min(24, pane.height * 0.06))
    }
}

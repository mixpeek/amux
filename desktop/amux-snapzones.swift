import Cocoa

// Snap zones: where a dragged window or tab lands when the cursor reaches an
// edge of a display (Ethan, 2026-10-21: "drag a tab into a snap to grid window
// with different sizes/positions"). Away from the edges a drag keeps the grid
// snap in amux-workspace.swift; these give the sizes.
//
//   left / right edge          that half
//   a corner                   that quarter
//   top edge                   fill
//   bottom edge                the third under the cursor
//
// Pure: no window, no event. desktop/test/snapzones-test.swift checks it.

struct SnapZone: Equatable {
    let name: String
    // x, y, width, height as fractions of the display's usable area (y up, as Cocoa).
    let fx: CGFloat, fy: CGFloat, fw: CGFloat, fh: CGFloat

    func frame(in vf: NSRect) -> NSRect {
        NSRect(x: vf.minX + vf.width * fx, y: vf.minY + vf.height * fy,
               width: vf.width * fw, height: vf.height * fh).integral
    }

    static let leftHalf = SnapZone(name: "left half", fx: 0, fy: 0, fw: 0.5, fh: 1)
    static let rightHalf = SnapZone(name: "right half", fx: 0.5, fy: 0, fw: 0.5, fh: 1)
    static let topHalf = SnapZone(name: "top half", fx: 0, fy: 0.5, fw: 1, fh: 0.5)
    static let bottomHalf = SnapZone(name: "bottom half", fx: 0, fy: 0, fw: 1, fh: 0.5)
    static let topLeft = SnapZone(name: "top left", fx: 0, fy: 0.5, fw: 0.5, fh: 0.5)
    static let topRight = SnapZone(name: "top right", fx: 0.5, fy: 0.5, fw: 0.5, fh: 0.5)
    static let bottomLeft = SnapZone(name: "bottom left", fx: 0, fy: 0, fw: 0.5, fh: 0.5)
    static let bottomRight = SnapZone(name: "bottom right", fx: 0.5, fy: 0, fw: 0.5, fh: 0.5)
    static let leftThird = SnapZone(name: "left third", fx: 0, fy: 0, fw: 1.0 / 3, fh: 1)
    static let centerThird = SnapZone(name: "center third", fx: 1.0 / 3, fy: 0, fw: 1.0 / 3, fh: 1)
    static let rightThird = SnapZone(name: "right third", fx: 2.0 / 3, fy: 0, fw: 1.0 / 3, fh: 1)
    static let fill = SnapZone(name: "full screen", fx: 0, fy: 0, fw: 1, fh: 1)

    static let all: [SnapZone] = [leftHalf, rightHalf, topHalf, bottomHalf, topLeft, topRight,
                                  bottomLeft, bottomRight, leftThird, centerThird, rightThird, fill]
}

/// The zone under the cursor, or nil when it is not at an edge of the display.
/// `screen` is the display's whole frame (the cursor reaches its edges, over the
/// menu bar and the Dock too); `visible` is its usable area, which zones fill.
func snapZone(cursor p: NSPoint, screen sf: NSRect, visible vf: NSRect,
              edge: CGFloat = 6, corner: CGFloat = 90) -> SnapZone? {
    guard p.x >= sf.minX, p.x <= sf.maxX, p.y >= sf.minY, p.y <= sf.maxY, vf.width > 0, vf.height > 0 else { return nil }
    let atLeft = p.x <= max(sf.minX, vf.minX) + edge
    let atRight = p.x >= min(sf.maxX, vf.maxX) - edge - 1
    let atTop = p.y >= vf.maxY - edge
    let atBottom = p.y <= vf.minY + edge
    let nearTop = p.y >= vf.maxY - corner
    let nearBottom = p.y <= vf.minY + corner
    let nearLeft = p.x <= vf.minX + corner
    let nearRight = p.x >= vf.maxX - corner
    if atLeft { return nearTop ? .topLeft : nearBottom ? .bottomLeft : .leftHalf }
    if atRight { return nearTop ? .topRight : nearBottom ? .bottomRight : .rightHalf }
    if atTop { return nearLeft ? .topLeft : nearRight ? .topRight : .fill }
    if atBottom {
        if nearLeft { return .bottomLeft }
        if nearRight { return .bottomRight }
        let t = (p.x - vf.minX) / vf.width
        return t < 1.0 / 3 ? .leftThird : t < 2.0 / 3 ? .centerThird : .rightThird
    }
    return nil
}

/// The zone a saved frame fills ("x,y,w,h" fractions, as relRect writes them),
/// for naming a workspace's windows; nil for a free-form spot.
func zoneName(rel: String?) -> String? {
    guard let p = rel?.split(separator: ",").compactMap({ Double($0) }), p.count == 4 else { return nil }
    let tol = 0.02
    return SnapZone.all.first { z in
        abs(Double(z.fx) - p[0]) < tol && abs(Double(z.fy) - p[1]) < tol
            && abs(Double(z.fw) - p[2]) < tol && abs(Double(z.fh) - p[3]) < tol
    }?.name
}

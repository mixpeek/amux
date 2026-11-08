// Which color each server's tabs get (Ethan, 2026-10-31: "each different amux server should be
// a different color in the tabs"). Before, every *.amux.io server was blue, so sandbox and
// mixpeek-dev tabs looked the same. This Mac stays green; every other server takes the first
// color no other server holds, the first time it is seen, and keeps it: the assignment is saved,
// so a color never moves between launches. Only past the palette do two servers share one.
//
// Pure: no window, no defaults. desktop/test/tabcolors-test.swift checks it.
import Foundation

/// The palette's names, in the order servers take them. Blue first: the first cloud server
/// keeps the blue it had before.
let tabColorNames = ["blue", "orange", "purple", "pink", "teal", "yellow", "indigo", "red", "brown", "mint", "cyan"]

/// The palette index for `server` (host, or host:port), given the indices already held. Returns
/// the index and the assignments to save.
func tabColorIndex(for server: String, assigned: [String: Int]) -> (index: Int, assigned: [String: Int]) {
    if let i = assigned[server], i >= 0, i < tabColorNames.count { return (i, assigned) }
    let taken = Set(assigned.values)
    var i = (0..<tabColorNames.count).first { !taken.contains($0) } ?? -1
    if i < 0 {
        // Every color is held: a stable pick from the name, so it is still the same each launch.
        var h: UInt32 = 5381
        for b in server.utf8 { h = (h &* 33) ^ UInt32(b) }
        i = Int(h % UInt32(tabColorNames.count))
    }
    var next = assigned
    next[server] = i
    return (i, next)
}

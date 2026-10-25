// wsctl: reads what the window server knows about an app's windows, and
// drives a running amux app by pid. Used by workspace-e2e.py.
//   wsctl windows <pid>     one JSON line per window: number, title, frame
//                           (Cocoa coordinates), Desktop number, display UUID
//   wsctl send <pid> <url>  hands the app an amux:// link
//   wsctl displays          one JSON line per display: uuid, frame
//   wsctl spaces            one JSON line per Desktop: id, uuid, display, number, current
import Cocoa
@_silgen_name("CGSMainConnectionID") func CGSMainConnectionID() -> Int32
@_silgen_name("CGSCopySpacesForWindows") func CGSCopySpacesForWindows(_ c: Int32, _ mask: Int32, _ wids: CFArray) -> CFArray
@_silgen_name("CGSCopyManagedDisplaySpaces") func CGSCopyManagedDisplaySpaces(_ c: Int32) -> CFArray

func json(_ o: [String: Any]) { print(String(data: try! JSONSerialization.data(withJSONObject: o, options: [.sortedKeys]), encoding: .utf8)!) }
let args = CommandLine.arguments
switch args.count > 1 ? args[1] : "" {
case "send":
    DistributedNotificationCenter.default().postNotificationName(Notification.Name("io.amux.desktop.url.\(args[2])"), object: args[3], userInfo: nil, deliverImmediately: true)
    RunLoop.main.run(until: Date().addingTimeInterval(0.2))
case "displays":
    for s in NSScreen.screens {
        let id = (s.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0
        let u = CGDisplayCreateUUIDFromDisplayID(id).map { CFUUIDCreateString(nil, $0.takeRetainedValue()) as String } ?? ""
        json(["uuid": u, "name": s.localizedName, "frame": NSStringFromRect(s.frame), "visible": NSStringFromRect(s.visibleFrame)])
    }
case "spaces":
    let cid = CGSMainConnectionID()
    for d in CGSCopyManagedDisplaySpaces(cid) as! [[String: Any]] {
        let disp = d["Display Identifier"] as? String ?? "?"
        let cur = (d["Current Space"] as? [String: Any])?["ManagedSpaceID"] as? Int ?? -1
        var n = 0
        for s in d["Spaces"] as? [[String: Any]] ?? [] where (s["type"] as? Int ?? 0) != 4 {
            n += 1
            let id = s["ManagedSpaceID"] as! Int
            json(["id": id, "uuid": s["uuid"] as? String ?? "", "display": disp, "number": n, "current": id == cur])
        }
    }
case "windows":
    let pid = Int32(args[2])!
    let cid = CGSMainConnectionID()
    var desk: [Int: (String, Int)] = [:]
    for d in CGSCopyManagedDisplaySpaces(cid) as! [[String: Any]] {
        let disp = d["Display Identifier"] as? String ?? "?"
        var n = 0
        for s in d["Spaces"] as? [[String: Any]] ?? [] {
            let id = s["ManagedSpaceID"] as! Int
            let fs = (s["type"] as? Int ?? 0) == 4
            if !fs { n += 1 }
            desk[id] = (disp, fs ? 0 : n)
        }
    }
    let primaryH = CGDisplayBounds(CGMainDisplayID()).height
    let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]]
    for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0 {
        let num = w[kCGWindowNumber as String] as! Int
        let b = w[kCGWindowBounds as String] as! [String: CGFloat]
        let title = w[kCGWindowName as String] as? String ?? ""
        if b["Width"]! < 300 || title.isEmpty { continue }
        let spaces = CGSCopySpacesForWindows(cid, 7, [num] as CFArray) as! [Int]
        let info = spaces.first.flatMap { desk[$0] }
        json(["n": num, "title": title, "onscreen": w[kCGWindowIsOnscreen as String] as? Bool ?? false,
              "x": Int(b["X"]!), "y": Int(primaryH - b["Y"]! - b["Height"]!), "w": Int(b["Width"]!), "h": Int(b["Height"]!),
              "desktop": info?.1 ?? -1, "display": info?.0 ?? ""])
    }
default:
    FileHandle.standardError.write("usage: wsctl windows <pid> | send <pid> <url> | displays | spaces\n".data(using: .utf8)!)
    exit(2)
}

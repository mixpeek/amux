// Unit check of UnauthorizedBackoff (scripts/amux-menubar.swift), run in desktop.yml:
//   xcrun swiftc -parse-as-library -D AMUX_APP -o /tmp/backoff scripts/amux-menubar.swift desktop/test/unauthorized-backoff-test.swift -framework Cocoa -framework WebKit && /tmp/backoff
// It is the wait the desktop app and the menu bar apply after a 401.
import Foundation

@main
struct UnauthorizedBackoffTest {
    static var failures = 0
    static func check(_ ok: Bool, _ what: String) {
        if ok { print("PASS \(what)") } else { print("FAIL \(what)"); failures += 1 }
    }

    static func main() {
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        var b = UnauthorizedBackoff()
        check(!b.holds(nil, now: t0), "nothing refused yet: a call goes through")
        check(b.refused(nil, now: t0) == 30, "first 401 waits 30s")
        check(b.holds(nil, now: t0.addingTimeInterval(29)), "the same anonymous call is held inside the wait")
        check(!b.holds("fresh-token", now: t0.addingTimeInterval(29)), "a new token goes through at once")
        check(!b.holds(nil, now: t0.addingTimeInterval(31)), "after the wait the call is tried again")
        var waits: [Int] = []
        for i in 1...8 { waits.append(b.refused(nil, now: t0.addingTimeInterval(Double(i) * 1000))) }
        check(waits == [60, 120, 240, 480, 600, 600, 600, 600], "doubles to a 10 minute cap: \(waits)")
        b.reset()
        check(!b.holds(nil, now: t0.addingTimeInterval(8001)), "a success ends the wait")
        check(b.refused("stale", now: t0) == 30, "after a success the wait starts over at 30s")
        check(b.holds("stale", now: t0.addingTimeInterval(10)), "a refused token is held like an anonymous call")
        print(failures == 0 ? "unauthorized-backoff: all passed" : "unauthorized-backoff: \(failures) failed")
        exit(failures == 0 ? 0 : 1)
    }
}

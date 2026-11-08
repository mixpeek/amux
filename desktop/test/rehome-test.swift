// Unit check of desktop/amux-rehome.swift: which saved links move to this
// Mac's address for the server when a workspace saved on another Mac opens.
//   xcrun swiftc -parse-as-library -o /tmp/rehome desktop/amux-rehome.swift desktop/test/rehome-test.swift && /tmp/rehome
import Foundation

var fails = 0
func check(_ label: String, _ want: String, _ got: String) {
    if want == got { print("  ok   \(label)") } else { print("  FAIL \(label): expected \(want), got \(got)"); fails += 1 }
}

@main struct RehomeTest {
    static func main() {
        let here = URL(string: "https://studio.tail1234.ts.net:8824")!
        let savedOn = originKey(URL(string: "https://localhost:8824")!)
        let peek = "https://localhost:8824/#peek=amux-app&tab=chat"
        check("a tab on the saving Mac's server moves to this Mac's address, keeping its view",
              "https://studio.tail1234.ts.net:8824/#peek=amux-app&tab=chat", rehomedURL(peek, savedOn: savedOn, here: here))
        check("a path and query are kept", "https://studio.tail1234.ts.net:8824/f/Users/x/a.md?raw=1",
              rehomedURL("https://localhost:8824/f/Users/x/a.md?raw=1", savedOn: savedOn, here: here))
        check("a tab on another server keeps its link", "https://sb-1.amux.io/#view=board",
              rehomedURL("https://sb-1.amux.io/#view=board", savedOn: savedOn, here: here))
        check("a tab already on this Mac's address is left alone", "https://studio.tail1234.ts.net:8824/#view=map",
              rehomedURL("https://studio.tail1234.ts.net:8824/#view=map", savedOn: savedOn, here: here))
        check("the same server on another port is another server", "https://localhost:9000/#view=map",
              rehomedURL("https://localhost:9000/#view=map", savedOn: savedOn, here: here))
        check("saved against a LAN name, opened on the server's own Mac", "https://localhost:8824/#view=board",
              rehomedURL("https://studio.local:8824/#view=board", savedOn: "https://studio.local:8824", here: URL(string: "https://localhost:8824")!))
        // Workspaces saved before the address was kept.
        check("an old workspace's loopback tab moves when this Mac reaches the server elsewhere",
              "https://studio.tail1234.ts.net:8824/#peek=amux-app&tab=chat", rehomedURL(peek, savedOn: nil, here: here))
        check("an old workspace's loopback tab stays on a Mac that is the server", peek,
              rehomedURL(peek, savedOn: nil, here: URL(string: "https://127.0.0.1:8824")!))
        check("an old workspace's non-loopback tab keeps its link", "https://studio.local:8824/#view=board",
              rehomedURL("https://studio.local:8824/#view=board", savedOn: nil, here: here))
        check("not connected: nothing moves", peek, rehomedURL(peek, savedOn: savedOn, here: nil))
        check("a link that is not a web page is left alone", "amux://workspace/x", rehomedURL("amux://workspace/x", savedOn: savedOn, here: here))
        if fails > 0 { print("rehome: \(fails) check(s) FAILED"); exit(1) }
        print("PASS: rehome, all checks passed")
    }
}

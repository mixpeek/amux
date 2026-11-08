import Foundation

// A saved workspace names each tab by its whole link, and the link carries the
// address the saving Mac used for the server ("https://localhost:8824/#peek=x").
// Another Mac reaches that same server at an address of its own (a LAN name, a
// tunnel), so opening the workspace there pointed every tab at a server that
// is not this one (owner, 2026-10-29: "when i load a workspace that was saved
// from another client, all the workers and tabs are not connected to the same
// server. workspace should maintain the connections"). A workspace now keeps
// the address it was saved against, and opening it moves the tabs on that
// server to this Mac's address for it. Tabs on any other server (a cloud
// sandbox) keep their link, which every Mac reaches the same way.

/// scheme://host:port, lowercased, with the default port filled in.
func originKey(_ u: URL) -> String? {
    guard let scheme = u.scheme?.lowercased(), scheme == "https" || scheme == "http", let host = u.host?.lowercased() else { return nil }
    return "\(scheme)://\(host):\(u.port ?? (scheme == "https" ? 443 : 80))"
}

func isLoopbackHost(_ host: String?) -> Bool {
    ["localhost", "127.0.0.1", "::1"].contains(host?.lowercased() ?? "")
}

/// The saved link `saved` as this Mac reaches it. A page on the server the
/// workspace was saved against (`savedOn`, an originKey) moves to `here`, this
/// Mac's address for that server, keeping its path, query and view. With no
/// `savedOn` (a workspace saved before it was kept) a loopback link stands for
/// the saving Mac's own server, the one the workspaces sync through, so it
/// moves only when this Mac reaches that server somewhere else.
func rehomedURL(_ saved: String, savedOn: String?, here: URL?) -> String {
    guard let here = here, let want = originKey(here), let u = URL(string: saved), let have = originKey(u), have != want,
          savedOn.map({ $0 == have }) ?? (isLoopbackHost(u.host) && !isLoopbackHost(here.host)),
          var c = URLComponents(url: u, resolvingAgainstBaseURL: false) else { return saved }
    c.scheme = here.scheme
    c.host = here.host
    c.port = here.port
    return c.string ?? saved
}

import Cocoa
import WebKit

// Find, Copy and the keys around them (owner, 2026-11-13: "Make sure in the
// Mac OS app I can use ctrl F and cmd c and cmd f etc. currently it just makes
// the disabled beep sound").
//
// The app is WKWebView windows, and WKWebView has no Find of its own: ⌘F went
// to a menu with no Find item and beeped. Edit > Find opens a bar over the
// page you are in; WKWebView.find selects and scrolls to each match. ⌃F
// outside a text box opens it too (app.js _hkFindKey sends amuxFind).
//
// ⌘C went to WebKit's copy:, which WebKit disables, with a beep, whenever its
// own record of the selection says nothing is selected. Edit > Copy now uses
// WebKit's copy when WebKit allows it and otherwise copies the page's
// selection itself.

/// The shared find text (⌘E, ⌘G across apps) and the clipboard. A test copy
/// gets private ones, so a test never touches the owner's.
let findPasteboard = desktopTestMode ? NSPasteboard(name: .init("com.amux.test.find.\(getpid())")) : NSPasteboard(name: .find)
let copyPasteboard = desktopTestMode ? NSPasteboard(name: .init("com.amux.test.copy.\(getpid())")) : NSPasteboard.general

final class FindBar: NSView, NSSearchFieldDelegate {
    let field = NSSearchField()
    let status = NSTextField(labelWithString: "")
    weak var webView: WKWebView?
    private var typing: DispatchWorkItem?

    init(webView: WKWebView) {
        self.webView = webView
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        wantsLayer = true
        let bg = NSVisualEffectView()
        bg.material = .popover
        bg.state = .active
        bg.wantsLayer = true
        bg.layer?.cornerRadius = 8
        bg.translatesAutoresizingMaskIntoConstraints = false
        addSubview(bg)
        field.placeholderString = "Find in page"
        field.delegate = self
        field.target = self
        field.action = #selector(enter(_:))
        field.sendsWholeSearchString = true
        status.textColor = .secondaryLabelColor
        status.font = .systemFont(ofSize: 11)
        let prev = FindBar.button("chevron.up", "Previous match (⇧⌘G)", #selector(previous(_:)), self)
        let next = FindBar.button("chevron.down", "Next match (⌘G)", #selector(next(_:)), self)
        let done = NSButton(title: "Done", target: self, action: #selector(done(_:)))
        done.bezelStyle = .rounded
        done.controlSize = .small
        let row = NSStackView(views: [field, status, prev, next, done])
        row.spacing = 6
        row.translatesAutoresizingMaskIntoConstraints = false
        addSubview(row)
        NSLayoutConstraint.activate([
            bg.leadingAnchor.constraint(equalTo: leadingAnchor), bg.trailingAnchor.constraint(equalTo: trailingAnchor),
            bg.topAnchor.constraint(equalTo: topAnchor), bg.bottomAnchor.constraint(equalTo: bottomAnchor),
            row.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8), row.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            row.topAnchor.constraint(equalTo: topAnchor, constant: 6), row.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
            field.widthAnchor.constraint(equalToConstant: 200),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    static func button(_ symbol: String, _ tip: String, _ action: Selector, _ target: AnyObject) -> NSButton {
        let b = NSButton(title: "", target: target, action: action)
        b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tip)
        b.bezelStyle = .rounded
        b.controlSize = .small
        b.toolTip = tip
        return b
    }

    /// Selects the next (or previous) match of the field's text, wrapping.
    /// `fromTop` starts over, so typing narrows from the top of the page.
    func find(backwards: Bool, fromTop: Bool = false) {
        guard let wv = webView else { return }
        let text = field.stringValue
        guard !text.isEmpty else { status.stringValue = ""; return }
        findPasteboard.clearContents()
        findPasteboard.setString(text, forType: .string)
        let go = {
            let cfg = WKFindConfiguration()
            cfg.backwards = backwards
            cfg.wraps = true
            cfg.caseSensitive = false
            wv.find(text, configuration: cfg) { [weak self] r in
                self?.status.stringValue = r.matchFound ? "" : "Not found"
                dlog("verdict=find_in_page found=\(r.matchFound ? 1 : 0) chars=\(text.count) backwards=\(backwards ? 1 : 0)")
            }
        }
        if fromTop {
            wv.evaluateJavaScript("getSelection().removeAllRanges(); 0") { _, _ in go() }
        } else {
            go()
        }
    }

    @objc func enter(_ sender: Any?) {
        find(backwards: NSApp.currentEvent?.modifierFlags.contains(.shift) ?? false)
    }
    @objc func next(_ sender: Any?) { find(backwards: false) }
    @objc func previous(_ sender: Any?) { find(backwards: true) }
    @objc func done(_ sender: Any?) { close() }

    func controlTextDidChange(_ obj: Notification) {
        typing?.cancel()
        let w = DispatchWorkItem { [weak self] in self?.find(backwards: false, fromTop: true) }
        typing = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: w)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy sel: Selector) -> Bool {
        if sel == #selector(NSResponder.cancelOperation(_:)) { close(); return true }
        return false
    }

    /// Hides the bar and gives the keyboard back to the page; the match stays selected.
    func close() {
        typing?.cancel()
        let wv = webView
        removeFromSuperview()
        if let wv = wv { wv.window?.makeFirstResponder(wv) }
        dlog("verdict=find_bar_closed")
    }
}

extension DashboardWindowController {
    var findBar: FindBar? { webView.subviews.compactMap { $0 as? FindBar }.first }

    /// Shows this page's find bar (or focuses it) with `text`, else the last search.
    func showFind(_ text: String? = nil) {
        let bar = findBar ?? {
            let b = FindBar(webView: webView)
            webView.addSubview(b)
            NSLayoutConstraint.activate([
                b.topAnchor.constraint(equalTo: webView.topAnchor, constant: 8),
                b.trailingAnchor.constraint(equalTo: webView.trailingAnchor, constant: -16),
            ])
            return b
        }()
        if let t = text { bar.field.stringValue = t }
        else if bar.field.stringValue.isEmpty { bar.field.stringValue = findPasteboard.string(forType: .string) ?? "" }
        window?.makeFirstResponder(bar.field)
        bar.field.currentEditor()?.selectAll(nil)
        dlog("verdict=find_bar_shown view=\(viewHash)")
        if text != nil { bar.find(backwards: false, fromTop: true) }
    }
}

/// The page's selection, as text (a text box's selected range, else the
/// page's), and whether a frame has the keyboard, whose selection the page
/// cannot read.
let pageSelectionJS = """
(() => { const a = document.activeElement;
  const frame = !!a && a.tagName === 'IFRAME';
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && a.selectionStart !== a.selectionEnd)
    return { text: a.value.slice(a.selectionStart, a.selectionEnd), frame };
  return { text: String(getSelection()), frame }; })()
"""

extension AppDelegate {
    /// The page these act on, or nil when the key window is another window
    /// (Settings), whose own responders then take the action as usual.
    var findCopyPage: DashboardWindowController? {
        if let k = NSApp.keyWindow, keyController?.window !== k { return nil }
        return keyPage
    }

    @objc func findInPage(_ sender: Any?) { findCopyPage?.showFind() }

    @objc func findNextInPage(_ sender: Any?) { findStep(backwards: false) }
    @objc func findPreviousInPage(_ sender: Any?) { findStep(backwards: true) }

    private func findStep(backwards: Bool) {
        guard let c = findCopyPage else { return }
        guard let bar = c.findBar else { c.showFind(); return }
        if bar.field.stringValue.isEmpty { c.showFind(); return }
        bar.find(backwards: backwards)
    }

    @objc func useSelectionForFind(_ sender: Any?) {
        guard let c = findCopyPage else { return }
        c.webView.evaluateJavaScript(pageSelectionJS) { r, _ in
            let t = ((r as? [String: Any])?["text"] as? String) ?? ""
            guard !t.isEmpty else { dlog("verdict=find_use_selection_empty"); return }
            findPasteboard.clearContents()
            findPasteboard.setString(t, forType: .string)
            c.findBar?.field.stringValue = t
            dlog("verdict=find_use_selection chars=\(t.count)")
        }
    }

    /// Edit > Copy. A native text field (the find bar) copies its own text.
    /// In a page, WebKit's copy (it keeps rich text) runs only over a
    /// selection: with none it beeps from inside WebKit, and WebKit's own
    /// menu validation answers yes either way, so the page is asked.
    @objc func copyFromPage(_ sender: Any?) {
        guard let c = findCopyPage, let w = c.window else { NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: sender); return }
        if let tv = w.firstResponder as? NSTextView { tv.copy(sender); return }
        c.webView.evaluateJavaScript(pageSelectionJS) { r, err in
            let d = r as? [String: Any]
            let t = (d?["text"] as? String) ?? ""
            let inFrame = (d?["frame"] as? Bool) ?? false
            if t.isEmpty && !inFrame {
                dlog("verdict=copy_nothing_selected view=\(c.viewHash)\(err.map { " error=\($0.localizedDescription)" } ?? "")")
                return
            }
            // A test copy writes a private pasteboard: WebKit's copy writes the owner's clipboard.
            if desktopTestMode {
                copyPasteboard.clearContents()
                copyPasteboard.setString(t, forType: .string)
            } else {
                NSApp.sendAction(#selector(NSText.copy(_:)), to: c.webView, from: sender)
            }
            // A frame's selection is the frame's own; WebKit copies it if there is one.
            dlog("verdict=copy_done source=\(t.isEmpty ? "frame" : "selection") chars=\(t.count) view=\(c.viewHash)")
        }
    }
}

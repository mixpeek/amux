// Messages notifications on iOS (app.js _cvMaybeNotify, owner 2026-10-27:
// notifications should work on iOS and Mac). The page asks for one when a
// post addressed to the owner arrives and the owner is not looking at that
// conversation; this posts it as a local notification, and a tap opens the
// conversation or thread it names.
//
// Limit: a local notification needs the app running (in the foreground, or
// kept alive by a recording or location in the background). A notification
// to an app that iOS has suspended needs APNs push from the server, which
// needs the owner's Apple push key.

import UIKit
import UserNotifications
import WebKit

final class NotifyBridge: NSObject, UNUserNotificationCenterDelegate {
    static let shared = NotifyBridge()
    weak var webView: WKWebView?
    private var asked = false

    func setUp(webView: WKWebView) {
        self.webView = webView
        UNUserNotificationCenter.current().delegate = self
    }

    func handle(_ body: [String: Any]) {
        let center = UNUserNotificationCenter.current()
        let post = {
            let c = UNMutableNotificationContent()
            c.title = (body["title"] as? String) ?? "amux"
            c.body = (body["body"] as? String) ?? ""
            c.sound = .default
            c.threadIdentifier = (body["tag"] as? String) ?? "messages"
            c.userInfo = ["hash": (body["url"] as? String) ?? "#view=messages"]
            center.add(UNNotificationRequest(identifier: UUID().uuidString, content: c, trigger: nil))
        }
        if asked {
            post()
            return
        }
        center.requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
            self.asked = true
            if granted { post() }
        }
    }

    // Shown even while the app is open, unless that conversation is on screen
    // (the page already decided that before asking).
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        if let hash = response.notification.request.content.userInfo["hash"] as? String, hash.hasPrefix("#"),
           hash.allSatisfy({ $0.isLetter || $0.isNumber || "#=&:-_.%|@".contains($0) }) {
            DispatchQueue.main.async { self.webView?.evaluateJavaScript("location.hash = '\(hash)'", completionHandler: nil) }
        }
        completionHandler()
    }
}

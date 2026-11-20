import Foundation
import UserNotifications

private func notificationDiagnostic(_ message: String) {
    if ProcessInfo.processInfo.environment["MICLIP_DIAGNOSTICS"] != nil {
        NSLog("%@", message)
    }
}

private final class ClipNotifications: NSObject, UNUserNotificationCenterDelegate {
    // UNUserNotificationCenter holds its delegate weakly.
    static let shared = ClipNotifications()

    func deliver(saved: Bool) {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert]) { granted, error in
            guard granted, error == nil else {
                notificationDiagnostic("System notification unavailable; menu-bar feedback remains active.")
                return
            }
            let content = UNMutableNotificationContent()
            content.title = "Mi Clip"
            content.body = saved
                ? "文件已接收并保存。"
                : "收到手机的文件分享请求，请查看菜单栏接收状态。"
            let request = UNNotificationRequest(
                identifier: UUID().uuidString, content: content, trigger: nil)
            center.add(request) { error in
                if error != nil {
                    notificationDiagnostic("System notification delivery failed; menu-bar feedback remains active.")
                }
            }
        }
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .list])
    }
}

@_cdecl("clip_notification_deliver")
public func clipNotificationDeliver(_ saved: Bool) {
    // Calling current() from an unbundled development binary can raise an exception.
    guard Bundle.main.bundleIdentifier == "com.cyeam.miclip",
          Bundle.main.bundleURL.pathExtension == "app" else { return }
    DispatchQueue.main.async {
        ClipNotifications.shared.deliver(saved: saved)
    }
}

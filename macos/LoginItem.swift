import AppKit
import ServiceManagement

@_cdecl("clip_login_item_ensure_enabled")
public func clipLoginItemEnsureEnabled() {
    // Development binaries and command-line checks must not register themselves.
    guard Bundle.main.bundleIdentifier == "com.cyeam.miclip",
          Bundle.main.bundleURL.pathExtension == "app" else { return }
    guard #available(macOS 13.0, *) else { return }
    do {
        let service = SMAppService.mainApp
        if service.status == .enabled { return }
        if service.status != .requiresApproval {
            try service.register()
        }
        switch service.status {
        case .enabled:
            return
        case .requiresApproval:
            let alert = NSAlert()
            alert.messageText = "请允许 Clip 开机启动"
            alert.informativeText = "Clip 将在登录 Mac 后自动运行。请在系统设置的登录项中允许 Clip。"
            alert.addButton(withTitle: "打开系统设置")
            alert.addButton(withTitle: "稍后")
            if alert.runModal() == .alertFirstButtonReturn {
                SMAppService.openSystemSettingsLoginItems()
            }
        default:
            showLoginItemError("系统未能启用 Clip 登录项。请将 Clip.app 安装到应用程序文件夹后重试。")
        }
    } catch {
        showLoginItemError(error.localizedDescription)
    }
}

private func showLoginItemError(_ detail: String) {
    let alert = NSAlert()
    alert.messageText = "无法启用开机启动"
    alert.informativeText = detail
    alert.addButton(withTitle: "好")
    alert.runModal()
}

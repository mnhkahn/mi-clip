import Cocoa
import UniformTypeIdentifiers
import QuartzCore

private final class DeviceGridView: NSView {
    override var isFlipped: Bool { true }
}
private final class DeviceCardButton: NSButton {
    override var isFlipped: Bool { true }
    let deviceIcon = NSImageView(frame: .zero)
    let deviceName = NSTextField(labelWithString: "")
    let deviceDetail = NSTextField(labelWithString: "")
    let brandLogo = NSImageView(frame: .zero)
    private let avatar = CALayer()
    private let ring = CAShapeLayer()
    private var animating = false
    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        configureContents()
    }
    required init?(coder: NSCoder) {
        super.init(coder: coder)
        configureContents()
    }
    private func configureContents() {
        title = ""
        isBordered = false
        wantsLayer = true
        avatar.frame = NSRect(x: 21, y: 2, width: 68, height: 68)
        avatar.cornerRadius = 34
        avatar.backgroundColor = NSColor.controlAccentColor.withAlphaComponent(0.10).cgColor
        avatar.isHidden = true
        layer?.addSublayer(avatar)
        ring.frame = NSRect(x: 21, y: 2, width: 68, height: 68)
        ring.path = CGPath(ellipseIn: CGRect(x: 3, y: 3, width: 62, height: 62), transform: nil)
        ring.fillColor = NSColor.clear.cgColor
        ring.strokeColor = NSColor.controlAccentColor.cgColor
        ring.lineWidth = 3
        ring.lineCap = .round
        ring.strokeEnd = 0.75
        ring.isHidden = true
        layer?.addSublayer(ring)
        deviceIcon.frame = NSRect(x: 27, y: 10, width: 56, height: 52)
        deviceIcon.wantsLayer = true
        deviceName.wantsLayer = true
        deviceIcon.imageScaling = .scaleProportionallyDown
        deviceName.frame = NSRect(x: 7, y: 76, width: 96, height: 22)
        deviceName.alignment = .center
        deviceName.font = .systemFont(ofSize: 12, weight: .medium)
        deviceName.lineBreakMode = .byTruncatingTail
        deviceDetail.frame = NSRect(x: 4, y: 99, width: 102, height: 16)
        deviceDetail.font = .systemFont(ofSize: 10)
        deviceDetail.textColor = .secondaryLabelColor
        deviceDetail.alignment = .center
        deviceDetail.lineBreakMode = .byTruncatingTail
        brandLogo.wantsLayer = true
        brandLogo.imageScaling = .scaleProportionallyUpOrDown
        addSubview(deviceIcon)
        addSubview(deviceName)
        addSubview(deviceDetail)
        addSubview(brandLogo)
    }
    func setTransferring(_ active: Bool) {
        guard active != animating else { return }
        animating = active
        ring.isHidden = !active
        avatar.isHidden = !active
        if active && !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            let rotation = CABasicAnimation(keyPath: "transform.rotation.z")
            rotation.fromValue = 0
            rotation.toValue = Double.pi * 2
            rotation.duration = 1.2
            rotation.repeatCount = .infinity
            ring.add(rotation, forKey: "transfer")
        } else { ring.removeAnimation(forKey: "transfer") }
    }
    override func hitTest(_ point: NSPoint) -> NSView? {
        // The whole card, including its icon and label, selects the device.
        super.hitTest(point) == nil ? nil : self
    }
}
private struct ShareDeviceChoice {
    let endpoint: String
    let name: String
    let type: Int
    var brand: ShareDeviceBrand? { ShareDeviceBrand.identify(name) }
    var detail: String { brand.map { "\($0.title) · \(kind)" } ?? kind }
    private var isTablet: Bool {
        type == 3 || (type == 2 && name.components(separatedBy: .alphanumerics.inverted)
            .contains { $0.caseInsensitiveCompare("pad") == .orderedSame })
    }
    var symbol: String {
        if isTablet { return "ipad" }
        switch type { case 1: return "iphone"; case 2: return "laptopcomputer"; default: return "desktopcomputer" }
    }
    var kind: String {
        if isTablet { return "平板电脑" }
        switch type { case 1: return "手机"; case 2: return "电脑"; default: return "设备" }
    }
}

@objc(ShareViewController)
final class ShareViewController: NSViewController {
    private let label = NSTextField(wrappingLabelWithString: "准备分享文件…")
    private let closeButton = NSButton(title: "", target: nil, action: nil)
    private let progress = NSProgressIndicator(frame: .zero)
    private let progressLabel = NSTextField(labelWithString: "")
    private var completed = false
    private var closed = false
    private var lastRefresh = Date.distantPast
    private let devices = DeviceGridView(frame: .zero)
    private let deviceScroll = NSScrollView(frame: .zero)
    private let deviceHeading = NSTextField(labelWithString: "选择接收设备")
    private let emptyDevices = NSTextField(wrappingLabelWithString: "正在搜索可接收设备…")
    private var choices: [ShareDeviceChoice] = []
    private var selectedEndpoint: String?
    private var deviceButtons: [DeviceCardButton] = []
    private var preparedNames: [String] = []
    private var submitted = false
    private var catalogueUpdated = Date.distantPast
    private var catalogueSignature = ""
    private var directory: URL?
    private var timer: Timer?
    private var started = Date()
    private var finished = false
    private var loading = false
    private var total: Int64 = 0
    private let limit: Int64 = 8 * 1024 * 1024 * 1024

    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 400, height: 380))
        let icon = NSImageView(frame: NSRect(x: 24, y: 320, width: 36, height: 36))
        let resources = Bundle(for: ShareViewController.self)
        icon.image = resources.image(forResource: "ClipShare")
            ?? resources.image(forResource: "Clip")
            ?? resources.url(forResource: "Clip", withExtension: "icns").flatMap { NSImage(contentsOf: $0) }
            ?? NSImage(systemSymbolName: "doc.on.clipboard.fill", accessibilityDescription: "Clip")
        let title = NSTextField(labelWithString: "Clip 分享")
        title.font = .boldSystemFont(ofSize: 19)
        title.frame = NSRect(x: 74, y: 325, width: 260, height: 27)
        label.frame = NSRect(x: 24, y: 254, width: 352, height: 56)
        label.font = .systemFont(ofSize: 14)
        deviceHeading.frame = NSRect(x: 24, y: 230, width: 352, height: 20)
        deviceHeading.font = .systemFont(ofSize: 12, weight: .medium)
        deviceScroll.frame = NSRect(x: 24, y: 94, width: 352, height: 128)
        deviceScroll.hasVerticalScroller = true
        deviceScroll.autohidesScrollers = true
        deviceScroll.drawsBackground = false
        devices.frame = NSRect(x: 0, y: 0, width: 352, height: 128)
        deviceScroll.documentView = devices
        emptyDevices.frame = NSRect(x: 8, y: 34, width: 332, height: 60)
        emptyDevices.alignment = .center
        emptyDevices.font = .systemFont(ofSize: 12)
        emptyDevices.textColor = .secondaryLabelColor
        devices.addSubview(emptyDevices)
        progress.frame = NSRect(x: 24, y: 64, width: 352, height: 12)
        progress.style = .bar
        progress.isIndeterminate = false
        progress.minValue = 0
        progress.maxValue = 100
        progress.isHidden = true
        progressLabel.frame = NSRect(x: 24, y: 34, width: 352, height: 20)
        progressLabel.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        progressLabel.textColor = .secondaryLabelColor
        progressLabel.isHidden = true
        closeButton.frame = NSRect(x: 350, y: 330, width: 28, height: 28)
        closeButton.isBordered = false
        closeButton.image = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "关闭")
        closeButton.contentTintColor = .secondaryLabelColor
        closeButton.setAccessibilityLabel("关闭分享")
        closeButton.toolTip = "关闭"
        closeButton.keyEquivalent = "\u{1b}"
        closeButton.target = self
        closeButton.action = #selector(close)
        [icon, title, label, deviceHeading, deviceScroll, progress, progressLabel, closeButton].forEach { view.addSubview($0) }

    }
    override func viewDidAppear() {
        super.viewDidAppear()
        guard !loading else { return }
        loading = true
        let providers = (extensionContext?.inputItems as? [NSExtensionItem] ?? [])
            .flatMap { $0.attachments ?? [] }
        guard !providers.isEmpty && providers.count <= 128 else {
            fail("请选择 1–128 个普通文件。"); return
        }
        do {
            let root = FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/Application Support/MiClip/Outbox", isDirectory: true)
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
            // Recover disk space left by an interrupted extension host.
            let oldRequests = (try? FileManager.default.contentsOfDirectory(at: root,
                includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
            for old in oldRequests where ["ready", "loading"].contains(old.pathExtension) {
                if let date = try? old.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
                   Date().timeIntervalSince(date) > 86400 {
                    try? FileManager.default.removeItem(at: old)
                }
            }
            let staging = root.appendingPathComponent(UUID().uuidString + ".loading", isDirectory: true)
            try FileManager.default.createDirectory(at: staging.appendingPathComponent("files"), withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
            directory = staging
            launchClient()
            refreshDevices()
            timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in self?.poll() }
            load(providers, index: 0, names: [])
        } catch { fail("无法准备文件：\(error.localizedDescription)") }
    }
    private func load(_ providers: [NSItemProvider], index: Int, names: [String]) {
        guard !finished, let directory = directory else { return }
        if index == providers.count {
            preparedNames = names
            label.stringValue = "已准备 \(names.count) 个文件 · 点击设备即可发送。"
            deviceChanged()
            return
        }
        let provider = providers[index]
        let receive: (URL?, Error?) -> Void = { [weak self] url, error in
            guard let self = self else { return }
            do {
                guard let url = url, url.isFileURL else {
                    throw error ?? NSError(domain: "Clip", code: 1,
                        userInfo: [NSLocalizedDescriptionKey: "只支持普通文件，暂不支持文件夹或网页链接。"])
                }
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                let info = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
                guard info.isRegularFile == true, info.isSymbolicLink != true else {
                    throw NSError(domain: "Clip", code: 2, userInfo: [NSLocalizedDescriptionKey: "请选择普通文件。"])
                }
                let size = Int64(info.fileSize ?? 0)
                guard self.total + size <= self.limit else {
                    throw NSError(domain: "Clip", code: 3, userInfo: [NSLocalizedDescriptionKey: "单次分享最多 8 GiB。"])
                }
                var name = url.lastPathComponent
                if names.contains(name) { name = "\(index + 1)-\(name)" }
                let destination = directory.appendingPathComponent("files").appendingPathComponent(name)
                try FileManager.default.copyItem(at: url, to: destination)
                try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
                DispatchQueue.main.async {
                    guard !self.finished else { return }
                    self.total += size
                    self.load(providers, index: index + 1, names: names + [name])
                }
            } catch { DispatchQueue.main.async { self.fail("无法读取文件：\(error.localizedDescription)") } }
        }
        if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, error in
                let url: URL?
                if let value = item as? URL { url = value }
                else if let data = item as? Data { url = URL(dataRepresentation: data, relativeTo: nil) }
                else { url = nil }
                receive(url, error)
            }
        } else if let type = provider.registeredTypeIdentifiers.first(where: {
            UTType($0)?.conforms(to: .data) == true
        }) {
            provider.loadFileRepresentation(forTypeIdentifier: type, completionHandler: receive)
        } else { fail("此内容不是可发送的文件。") }
    }
    private func launchClient() {
        let app = Bundle.main.bundleURL.deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let config = NSWorkspace.OpenConfiguration()
        config.activates = false
        NSWorkspace.shared.openApplication(at: app, configuration: config) { [weak self] _, error in
            if let error = error { DispatchQueue.main.async { self?.fail("无法启动 Clip：\(error.localizedDescription)") } }
        }
    }
    @objc private func refreshDevices() {
        guard !submitted, let directory = directory else { return }
        lastRefresh = Date()
        try? Data().write(to: directory.deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("refresh-devices"), options: .atomic)
        readDevices()
    }
    private func readDevices() {
        guard !submitted, let directory = directory else { return }
        let path = directory.deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("devices.json")
        var entries: [[String: Any]] = []
        if let data = try? Data(contentsOf: path),
           let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
           let updated = (json["updated"] as? NSNumber)?.doubleValue,
           Date().timeIntervalSince1970 - updated < 45 {
            catalogueUpdated = Date(timeIntervalSince1970: updated)
            entries = json["devices"] as? [[String: Any]] ?? []
        }
        let signature = entries.map { "\($0["endpoint"] ?? ""): \($0["name"] ?? ""): \($0["device_type"] ?? 1)" }.joined(separator: "|")
        if signature != catalogueSignature || (choices.isEmpty && !entries.isEmpty) {
            choices = entries.compactMap { entry in
                guard let endpoint = entry["endpoint"] as? String, let name = entry["name"] as? String else { return nil }
                return ShareDeviceChoice(endpoint: endpoint, name: name, type: (entry["device_type"] as? NSNumber)?.intValue ?? 1)
            }
            if !choices.contains(where: { $0.endpoint == selectedEndpoint }) { selectedEndpoint = nil }
            renderDevices()
            catalogueSignature = signature
        }
        if entries.isEmpty { emptyDevices.stringValue = "正在搜索设备…\n请解锁手机并开启互联。" }
        deviceChanged()
    }
    private func renderDevices() {
        deviceButtons.forEach { $0.removeFromSuperview() }
        deviceButtons.removeAll()
        emptyDevices.isHidden = !choices.isEmpty
        let rows = (choices.count + 2) / 3
        devices.frame.size = NSSize(width: 352, height: max(128, rows * 126))
        for (index, choice) in choices.enumerated() {
            let card = DeviceCardButton(frame: NSRect(x: (index % 3) * 118, y: (index / 3) * 126, width: 110, height: 118))
            card.target = self
            card.action = #selector(selectDevice(_:))
            card.tag = index
            card.deviceName.stringValue = choice.name
            card.deviceDetail.stringValue = choice.detail
            card.brandLogo.isHidden = choice.brand == nil
            card.deviceIcon.isHidden = choice.brand != nil
            card.brandLogo.frame = NSRect(x: 25, y: 6, width: 60, height: 60)
            if let brand = choice.brand {
                card.brandLogo.image = brand.image
                card.brandLogo.contentTintColor = brand.color
            }
            card.deviceIcon.image = NSImage(systemSymbolName: choice.symbol, accessibilityDescription: choice.kind)?
                .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 32, weight: .regular))
            card.toolTip = "\(choice.name) · \(choice.detail)"
            card.setAccessibilityLabel("\(choice.name)，\(choice.detail)")
            card.wantsLayer = true
            card.layer?.cornerRadius = 12
            devices.addSubview(card)
            deviceButtons.append(card)
        }
        deviceChanged()
    }
    @objc private func selectDevice(_ sender: NSButton) {
        guard !submitted, !finished, !preparedNames.isEmpty, choices.indices.contains(sender.tag) else { return }
        selectedEndpoint = choices[sender.tag].endpoint
        deviceChanged()
        sendFiles()
    }
    @objc private func deviceChanged() {
        deviceHeading.stringValue = submitted ? "接收设备" : "选择接收设备"
        if submitted {
            devices.frame.size.height = 128
            deviceScroll.contentView.scroll(to: .zero)
            deviceScroll.reflectScrolledClipView(deviceScroll.contentView)
        }
        for (index, card) in deviceButtons.enumerated() {
            let selected = choices[index].endpoint == selectedEndpoint
            card.isHidden = submitted && !selected
            card.isEnabled = !submitted && !finished && !preparedNames.isEmpty
            if submitted && selected { card.frame.origin = NSPoint(x: 121, y: 8) }
            card.brandLogo.frame = submitted
                ? NSRect(x: 31, y: 12, width: 48, height: 48)
                : NSRect(x: 25, y: 6, width: 60, height: 60)
            card.setTransferring(submitted && selected && !finished)
            card.deviceIcon.contentTintColor = selected ? .controlAccentColor : .labelColor
            card.deviceName.textColor = selected ? .controlAccentColor : .labelColor
            card.layer?.backgroundColor = (submitted ? NSColor.clear : (selected ? NSColor.controlAccentColor.withAlphaComponent(0.10) : NSColor.controlBackgroundColor)).cgColor
            card.layer?.borderColor = (selected ? NSColor.controlAccentColor : NSColor.separatorColor.withAlphaComponent(0.35)).cgColor
            card.layer?.borderWidth = submitted ? 0 : (selected ? 2 : 1)
            card.setAccessibilityValue(selected ? "已选择" : "未选择")
        }
    }
    @objc private func sendFiles() {
        guard !submitted, let directory = directory,
              !preparedNames.isEmpty,
              let endpoint = selectedEndpoint else { return }
        guard Date().timeIntervalSince(catalogueUpdated) < 45 else {
            label.stringValue = "正在重新搜索设备，请稍后点击设备发送。"
            refreshDevices()
            return
        }
        do {
            let request: [String: Any] = ["files": preparedNames, "target": endpoint]
            try JSONSerialization.data(withJSONObject: request).write(to: directory.appendingPathComponent("request.json"), options: .atomic)
            let ready = directory.deletingPathExtension().appendingPathExtension("ready")
            try FileManager.default.moveItem(at: directory, to: ready)
            self.directory = ready
            submitted = true
            started = Date()
            progress.isHidden = false
            progressLabel.isHidden = false
            updateProgress(sent: 0)
            deviceChanged()
            label.stringValue = "正在连接所选设备…\n尚未开始发送文件。"
        } catch { fail("无法提交分享：\(error.localizedDescription)") }
    }
    private func poll() {
        guard !finished, let directory = directory else { return }
        if !submitted {
            if Date().timeIntervalSince(lastRefresh) >= 8 { refreshDevices() }
            else { readDevices() }
            return
        }
        if let data = try? Data(contentsOf: directory.appendingPathComponent("status.json")),
           let status = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
           let state = status["state"] as? String {
            switch state {
            case "connecting": label.stringValue = "正在与所选设备建立分享通道…\n尚未收到手机的接收响应。"
            case "waiting": label.stringValue = "分享请求已发出，等待手机接受…\n如手机显示接收提示，请确认。"
            case "sending":
                let sent = (status["sent"] as? NSNumber)?.int64Value ?? 0
                updateProgress(sent: sent)
                label.stringValue = "正在发送到设备…"
            case "completed":
                timer?.invalidate()
                label.stringValue = "手机已确认接收完成。"
                completed = true
                finished = true
                updateProgress(sent: total)
                deviceChanged()
                cleanup()
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in self?.close() }
                return
            case "failed":
                let message = status["message"] as? String ?? "分享通道已断开，未收到接收完成确认。"
                fail("文件未发送完成：\(message)")
                return
            default: break
            }
        }
        if Date().timeIntervalSince(started) > 600 && !label.stringValue.hasPrefix("正在发送") { fail("分享超时，请检查 Clip 和手机的连接后重试。") }
        if Date().timeIntervalSince(started) > 60 && !FileManager.default.fileExists(atPath: directory.appendingPathComponent("status.json").path) {
            fail("未能连接手机。请启动 Clip，确认已登录并连接手机后重试。")
        }
    }
    override func viewDidDisappear() {
        super.viewDidDisappear()
        timer?.invalidate()
        finished = true
        cleanup()
    }
    private func cleanup() {
        if let directory = directory { try? FileManager.default.removeItem(at: directory) }
    }
    private func fail(_ message: String) {
        timer?.invalidate()
        finished = true
        deviceChanged()
        label.stringValue = message
        cleanup()
    }
    private func updateProgress(sent: Int64) {
        let bytes = max(0, min(sent, total))
        let percent = total == 0 ? (completed ? 100.0 : 0.0) : Double(bytes) / Double(total) * 100
        progress.doubleValue = percent
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        formatter.allowsNonnumericFormatting = false
        progressLabel.stringValue = "\(formatter.string(fromByteCount: bytes)) / \(formatter.string(fromByteCount: total)) · \(Int(percent))%"
        progressLabel.toolTip = "已发送 \(bytes) 字节，共 \(total) 字节"
        progress.setAccessibilityValue("\(Int(percent))%")
    }
    #if SHARE_PREVIEW
    static func verifyPreview(output: URL, icon: URL) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let staging = root.appendingPathComponent("Outbox/preview.loading")
        try FileManager.default.createDirectory(at: staging.appendingPathComponent("files"), withIntermediateDirectories: true)
        let controller = ShareViewController()
        _ = controller.view
        let window = NSWindow(contentRect: controller.view.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = controller.view
        window.backgroundColor = .windowBackgroundColor
        controller.loading = true
        controller.directory = staging
        controller.preparedNames = ["example.png"]
        controller.total = 1048576
        controller.label.stringValue = "已准备 1 个文件 · 点击设备即可发送。"
        (controller.view.subviews.first as? NSImageView)?.image = NSImage(contentsOf: icon)
        let catalogue: [String: Any] = ["updated": Date().timeIntervalSince1970, "devices": [
            ["endpoint": "192.0.2.1:1234", "name": "Xiaomi 15 Pro", "device_type": 1],
            ["endpoint": "192.0.2.2:1234", "name": "Redmi Pad", "device_type": 2],
            ["endpoint": "192.0.2.3:1234", "name": "MacBook Pro", "device_type": 2],
        ]]
        try JSONSerialization.data(withJSONObject: catalogue).write(to: root.appendingPathComponent("devices.json"))
        controller.poll()
        precondition(FileManager.default.fileExists(atPath: root.appendingPathComponent("refresh-devices").path), "Opening must automatically request discovery")
        precondition(controller.deviceButtons.count == 3)
        precondition(controller.choices.map { $0.brand } == [.xiaomi, .redmi, .apple])
        precondition(controller.choices[1].detail == "Redmi · 平板电脑")
        precondition(controller.choices[1].symbol == "ipad")
        precondition(ShareDeviceChoice(endpoint: "", name: "Test tablet", type: 3).kind == "平板电脑")
        precondition(ShareDeviceChoice(endpoint: "", name: "ThinkPad", type: 2).kind == "电脑")
        precondition(ShareDeviceBrand.identify("我的手机") == nil)
        precondition(ShareDeviceBrand.identify("2412DPC0AG") == nil)
        precondition(ShareDeviceBrand.identify("Galaxy S24") == .samsung)
        precondition(ShareDeviceBrand.identify("POCO F6") == .poco)
        for brand in ShareDeviceBrand.allCases {
            precondition(brand.image.tiffRepresentation != nil, "Every brand mark must render locally")
        }
        precondition(controller.deviceButtons.allSatisfy { !$0.brandLogo.isHidden })
        precondition(!controller.submitted, "Discovery must not send without a device click")
        func snapshot(_ name: String) throws {
            controller.view.layoutSubtreeIfNeeded()
            let bitmap = controller.view.bitmapImageRepForCachingDisplay(in: controller.view.bounds)!
            controller.view.cacheDisplay(in: controller.view.bounds, to: bitmap)
            try bitmap.representation(using: .png, properties: [:])!.write(to: output.appendingPathComponent(name))
        }
        try snapshot("devices.png")
        controller.selectDevice(controller.deviceButtons[0])
        precondition(controller.submitted, "One device click must submit the transfer")
        let ready = staging.deletingPathExtension().appendingPathExtension("ready")
        let request = try JSONSerialization.jsonObject(with: Data(contentsOf: ready.appendingPathComponent("request.json"))) as! [String: Any]
        precondition(request["target"] as? String == "192.0.2.1:1234")
        controller.selectDevice(controller.deviceButtons[1])
        precondition(controller.selectedEndpoint == "192.0.2.1:1234", "Repeated clicks must not switch an active transfer")
        try JSONSerialization.data(withJSONObject: ["state": "sending", "sent": 524288]).write(to: ready.appendingPathComponent("status.json"))
        controller.poll()
        precondition(controller.progress.doubleValue == 50)
        precondition(controller.deviceButtons[1].isHidden)
        try snapshot("sending.png")
        try JSONSerialization.data(withJSONObject: ["state": "completed"]).write(to: ready.appendingPathComponent("status.json"))
        controller.poll()
        precondition(controller.completed && controller.finished)
        precondition(controller.progress.doubleValue == 100)
        precondition(!FileManager.default.fileExists(atPath: ready.path))
        // Exercise the scheduled automatic dismissal, without a phone or host app.
        RunLoop.current.run(until: Date().addingTimeInterval(0.6))
        precondition(controller.closed, "Receiver completion must automatically dismiss the extension")
        print("PASS: automatic discovery, one-click submission, duplicate-click protection, byte progress and completion cleanup")
    }
    #endif
    @objc private func close() {
        guard !closed else { return }
        closed = true
        timer?.invalidate()
        let success = completed
        finished = true
        deviceChanged()
        cleanup()
        if success { extensionContext?.completeRequest(returningItems: nil, completionHandler: nil) }
        else { extensionContext?.cancelRequest(withError: NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError)) }
    }
}

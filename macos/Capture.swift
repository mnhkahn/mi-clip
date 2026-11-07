import Cocoa
import Carbon
import ScreenCaptureKit
import UniformTypeIdentifiers

private let capture = CaptureController()
@_cdecl("clip_capture_start") public func captureStart() { capture.registerShortcut() }
@_cdecl("clip_capture_open") public func captureOpen() { capture.begin() }
@_cdecl("clip_capture_settings") public func captureSettings() { capture.settings() }
@_cdecl("clip_capture_stop") public func captureStop() { capture.stop() }

private final class CapturePanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}
private func paintControl(_ bounds: CGRect, title: String, pressed: Bool) {
    NSColor(white: pressed ? 0.32 : 0.22, alpha: 1).setFill()
    NSBezierPath(roundedRect: bounds.insetBy(dx: 1, dy: 1), xRadius: 5, yRadius: 5).fill()
    let attributes: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 12), .foregroundColor: NSColor.white]
    let size = (title as NSString).size(withAttributes: attributes)
    (title as NSString).draw(at: CGPoint(x: max(4, (bounds.width-size.width)/2), y: (bounds.height-size.height)/2), withAttributes: attributes)
}
private final class CaptureButton: NSButton {
    override func draw(_ dirtyRect: NSRect) { paintControl(bounds, title: title, pressed: isHighlighted) }
}
private final class CapturePopup: NSPopUpButton {
    override func draw(_ dirtyRect: NSRect) { paintControl(bounds, title: (titleOfSelectedItem ?? "") + " ▾", pressed: isHighlighted) }
}
private func button(_ title: String, _ target: AnyObject, _ action: Selector) -> NSButton {
    let result = CaptureButton(title: title, target: target, action: action)
    result.bezelStyle = .rounded; return result
}
private func notify(_ message: String) {
    let alert = NSAlert(); alert.messageText = "Clip 截图"; alert.informativeText = message
    alert.runModal()
}
private func screenRect(_ screen: NSScreen) -> CGRect {
    let top = NSScreen.screens.first!.frame.maxY
    return CGRect(x: screen.frame.minX, y: top-screen.frame.maxY, width: screen.frame.width, height: screen.frame.height)
}
private func captureImage(screen: NSScreen, region: CGRect?, below: CGWindowID?, completion: @escaping (CGImage?) -> Void) {
    let rect = screenRect(screen)
    let requested = region.map { CGRect(x: rect.minX+$0.minX, y: rect.minY+$0.minY, width: $0.width, height: $0.height) } ?? rect
    if #available(macOS 14.0, *) {
        Task { @MainActor in
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
                let identifier = (screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as! NSNumber).uint32Value
                guard let display = content.displays.first(where: { $0.displayID == identifier }) else { completion(nil); return }
                let own = content.windows.filter { $0.owningApplication?.processID == ProcessInfo.processInfo.processIdentifier }
                let filter = SCContentFilter(display: display, excludingWindows: own)
                let config = SCStreamConfiguration()
                config.sourceRect = region ?? CGRect(origin: .zero, size: rect.size)
                config.width = Int(requested.width * screen.backingScaleFactor)
                config.height = Int(requested.height * screen.backingScaleFactor)
                config.showsCursor = false
                let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
                completion(image)
            } catch { completion(nil) }
        }
    } else {
        let image = CGWindowListCreateImage(requested, below == nil ? .optionOnScreenOnly : .optionOnScreenBelowWindow, below ?? kCGNullWindowID, [.bestResolution, .boundsIgnoreFraming])
        completion(image)
    }
}

private final class CaptureController: NSObject, NSWindowDelegate {
    var hotkey: EventHotKeyRef?
    var handler: EventHandlerRef?
    var overlays: [NSWindow] = []
    var toolbar: NSPanel?
    var editor: EditorController?
    var settingsWindow: NSWindow?
    var recorder: ShortcutRecorder?
    var settingStatus: NSTextField?
    var enabled: NSButton?
    var selectedView: SelectionView?
    var timer: Timer?
    var assembler: ScrollAssembler?
    var scrollMarks: [CaptureMark] = []
    var stableFrame: PixelFrame?
    var hudLabel: NSTextField?
    var preview: NSImageView?
    var sampling = false
    var previousApp: NSRunningApplication?
    var busy = false
    var generation = 0
    var shortcutLabel: String { UserDefaults.standard.string(forKey: "capture.shortcutLabel") ?? "⌃⌘A" }
    func registerShortcut() {
        if let hotkey { UnregisterEventHotKey(hotkey); self.hotkey = nil }
        if handler == nil {
            var event = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
            InstallEventHandler(GetApplicationEventTarget(), { _, event, _ in
                var id = EventHotKeyID()
                guard GetEventParameter(event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID), nil, MemoryLayout<EventHotKeyID>.size, nil, &id) == noErr,
                      id.signature == 0x434C4950, id.id == 1 else { return OSStatus(eventNotHandledErr) }
                DispatchQueue.main.async { capture.begin() }; return noErr
            }, 1, &event, nil, &handler)
        }
        if UserDefaults.standard.object(forKey: "capture.enabled") as? Bool == false { settingStatus?.stringValue = "快捷键已关闭 · 可从菜单栏截图"; return }
        let key = UserDefaults.standard.object(forKey: "capture.key") as? UInt32 ?? UInt32(kVK_ANSI_A)
        let mods = UserDefaults.standard.object(forKey: "capture.mods") as? UInt32 ?? UInt32(controlKey | cmdKey)
        let result = RegisterEventHotKey(key, mods, EventHotKeyID(signature: 0x434C4950, id: 1), GetApplicationEventTarget(), 0, &hotkey)
        settingStatus?.stringValue = result == noErr ? "\(shortcutLabel) 已启用" : "快捷键被占用或注册失败（\(result)），请修改或重试"
        if result != noErr && settingsWindow == nil {
            DispatchQueue.main.async { self.settings(); self.settingStatus?.stringValue = "\(self.shortcutLabel) 被占用或注册失败，请修改或重试" }
        }
    }
    func stop() {
        cancel(); if let hotkey { UnregisterEventHotKey(hotkey); self.hotkey = nil }
        if let handler { RemoveEventHandler(handler); self.handler = nil }
        settingsWindow?.delegate = nil; settingsWindow?.close(); settingsWindow = nil
    }
    func begin() {
        guard !busy else { editor?.window.makeKeyAndOrderFront(nil); return }
        guard CGPreflightScreenCaptureAccess() else {
            CGRequestScreenCaptureAccess()
            notify("请在系统设置 → 隐私与安全性 → 屏幕录制中允许 Clip，然后重新尝试截图。必要时重新启动 Clip。")
            return
        }
        previousApp = NSWorkspace.shared.frontmostApplication
        busy = true; generation += 1
        let token = generation
        settingsWindow?.orderOut(nil)
        let screens = NSScreen.screens
        var images: [(NSScreen, CGImage)] = []
        func next(_ index: Int) {
            guard token == self.generation else { return }
            if index == screens.count {
                guard !images.isEmpty else { self.cancel(); notify("无法获取屏幕画面，请检查屏幕录制权限。"); return }
                for (screen, image) in images {
                    let panel = CapturePanel(contentRect: screen.frame, styleMask: [.borderless], backing: .buffered, defer: false)
                    panel.level = .screenSaver; panel.isOpaque = true; panel.hasShadow = false
                    panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
                    panel.isReleasedWhenClosed = false
                    let view = SelectionView(screen: screen, image: image, owner: self)
                    panel.contentView = view; self.overlays.append(panel)
                    panel.makeKeyAndOrderFront(nil); panel.makeFirstResponder(view)
                }
                NSApp.activate(ignoringOtherApps: true)
                return
            }
            captureImage(screen: screens[index], region: nil, below: nil) { image in
                if let image { images.append((screens[index], image)) }; next(index+1)
            }
        }
        next(0)
    }
    func selected(_ view: SelectionView) {
        selectedView = view
        editSelection()
    }
    @objc func editSelection() {
        guard let view = selectedView, let image = view.cropped() else { return }
        showEditor(image, allowScroll: true)
    }
    func showEditor(_ image: CGImage, allowScroll: Bool = false, marks: [CaptureMark] = []) {
        timer?.invalidate(); timer = nil; generation += 1
        overlays.forEach { $0.close() }; overlays.removeAll(); toolbar?.close(); toolbar = nil
        assembler = nil; stableFrame = nil; scrollMarks = []
        if !allowScroll { selectedView = nil }
        editor = EditorController(image: image, onScroll: allowScroll ? { [weak self] in self?.startScroll() } : nil, onClose: { [weak self] in self?.editor = nil; self?.selectedView = nil; self?.busy = false })
        editor!.canvas.document.marks = marks
        editor!.window.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true)
    }
    @objc func startScroll() {
        guard let view = selectedView, let image = view.cropped() else { return }
        guard image.width * image.height <= ScrollAssembler.maxPixels else { notify("选区过大，请缩小选区。"); return }
        editor?.canvas.finishText()
        scrollMarks = editor?.canvas.document.marks ?? []
        // Transition to scrolling without ending the current capture session.
        editor?.window.delegate = nil; editor?.window.close(); editor = nil
        assembler = ScrollAssembler(image); stableFrame = nil; sampling = false
        overlays.forEach { $0.orderOut(nil) }; toolbar?.close()
        let frame = view.screen.visibleFrame
        let panel = CapturePanel(contentRect: CGRect(x: frame.maxX-300, y: frame.minY+20, width: 280, height: 215), styleMask: [.titled], backing: .buffered, defer: false)
        panel.title = "滚动截图 · 手动向下滚动"; panel.level = .floating; panel.isReleasedWhenClosed = false
        // Keep controls outside the selected area whenever possible; modern
        // capture explicitly excludes every Clip window.
        let label = NSTextField(wrappingLabelWithString: "缓慢向下滚动 · 完成后点击完成")
        label.frame = CGRect(x: 12, y: 164, width: 256, height: 42); panel.contentView?.addSubview(label); hudLabel = label
        let imageView = NSImageView(frame: CGRect(x: 12, y: 48, width: 256, height: 110)); imageView.imageScaling = .scaleProportionallyUpOrDown
        imageView.image = NSImage(cgImage: image, size: .zero); panel.contentView?.addSubview(imageView); preview = imageView
        let stack = NSStackView(views: [button("撤回", self, #selector(undoScroll)), button("完成", self, #selector(finishScroll)), button("取消", self, #selector(cancelAction))])
        stack.frame = CGRect(x: 12, y: 8, width: 256, height: 32); panel.contentView?.addSubview(stack)
        toolbar = panel; panel.orderFrontRegardless()
        previousApp?.activate(options: .activateIgnoringOtherApps) // Return focus for manual scrolling.
        panel.orderFrontRegardless()
        let token = generation
        timer = Timer.scheduledTimer(withTimeInterval: 0.65, repeats: true) { [weak self] _ in self?.sample(token) }
    }
    func sample(_ token: Int) {
        guard token == generation, !sampling, let view = selectedView, assembler != nil else { return }
        sampling = true
        captureImage(screen: view.screen, region: view.selection, below: toolbar.map { CGWindowID($0.windowNumber) }) { [weak self] image in
            guard let self, token == self.generation, self.assembler != nil else { return }
            self.sampling = false
            guard let image else { self.hudLabel?.stringValue = "采集失败，请检查权限或取消"; return }
            let frame = PixelFrame(image)
            defer { self.stableFrame = frame }
            guard let previous = self.stableFrame, previous.difference(frame) < 1.5 else { return }
            switch self.assembler!.append(image) {
            case .unchanged: break
            case .appended(let height):
                self.hudLabel?.stringValue = "已截 \(height) 像素 · 继续向下滚动"
                self.preview?.image = NSImage(cgImage: self.assembler!.composed(), size: .zero)
            case .uncertain: self.hudLabel?.stringValue = "无法可靠拼接，请慢一点或返回上一位置；也可完成已有内容"
            case .limit: self.hudLabel?.stringValue = "已到长图容量上限，请点击完成"; self.timer?.invalidate()
            }
        }
    }
    @objc func undoScroll() {
        assembler?.undo(); stableFrame = nil
        if let assembler { preview?.image = NSImage(cgImage: assembler.composed(), size: .zero); hudLabel?.stringValue = "已撤回，请滚回最后保留的位置再继续" }
    }
    @objc func finishScroll() {
        guard let view = selectedView, assembler != nil else { return }
        timer?.invalidate(); timer = nil; generation += 1
        let token = generation
        hudLabel?.stringValue = "正在完成最后一帧…"
        captureImage(screen: view.screen, region: view.selection, below: toolbar.map { CGWindowID($0.windowNumber) }) { [weak self] image in
            guard let self, self.generation == token, self.assembler != nil else { return }
            if let image {
                let result = self.assembler!.append(image)
                if result == .uncertain { notify("最后一帧无法可靠拼接，将保留此前已完成的截图。") }
            }
            self.showEditor(self.assembler!.composed(), marks: self.scrollMarks)
        }
    }
    @objc func cancelAction() { cancel() }
    func cancel() {
        generation += 1; timer?.invalidate(); timer = nil; sampling = false
        overlays.forEach { $0.close() }; overlays.removeAll(); toolbar?.close(); toolbar = nil
        editor?.window.close(); editor = nil; selectedView = nil; assembler = nil; stableFrame = nil; scrollMarks = []; busy = false
    }
    func settings() {
        if let settingsWindow { settingsWindow.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true); return }
        let panel = CapturePanel(contentRect: CGRect(x: 0, y: 0, width: 460, height: 205), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        panel.title = "截图快捷键"; panel.isReleasedWhenClosed = false
        let enable = NSButton(checkboxWithTitle: "启用全局截图快捷键", target: self, action: #selector(toggleShortcut))
        enable.state = UserDefaults.standard.object(forKey: "capture.enabled") as? Bool == false ? .off : .on
        enable.frame = CGRect(x: 20, y: 160, width: 400, height: 28); panel.contentView?.addSubview(enable); enabled = enable
        let record = ShortcutRecorder(frame: CGRect(x: 20, y: 110, width: 420, height: 38)); record.owner = self
        panel.contentView?.addSubview(record); recorder = record
        let status = NSTextField(wrappingLabelWithString: hotkey == nil ? "快捷键未注册，点击输入框修改或重试" : "\(shortcutLabel) 已启用")
        status.frame = CGRect(x: 20, y: 55, width: 420, height: 45); panel.contentView?.addSubview(status); settingStatus = status
        let retry = button("重试注册", self, #selector(retryShortcut)); retry.frame = CGRect(x: 20, y: 12, width: 110, height: 32); panel.contentView?.addSubview(retry)
        let hint = NSTextField(labelWithString: "先注册者优先；其他应用的监听可能无法检测。")
        hint.font = .systemFont(ofSize: 10); hint.frame = CGRect(x: 140, y: 18, width: 310, height: 20); panel.contentView?.addSubview(hint)
        panel.delegate = self
        settingsWindow = panel; panel.center(); panel.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true)
    }
    func beginRecording() {
        if let hotkey { UnregisterEventHotKey(hotkey); self.hotkey = nil }
        settingStatus?.stringValue = "请按新快捷键（包含 Command 或 Control）"
    }
    func windowWillClose(_ notification: Notification) {
        if notification.object as? NSWindow === settingsWindow { registerShortcut() }
    }
    @objc func retryShortcut() { registerShortcut(); recorder?.needsDisplay = true }
    @objc func toggleShortcut() { UserDefaults.standard.set(enabled?.state == .on, forKey: "capture.enabled"); registerShortcut() }
    func shortcut(_ event: NSEvent) {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        guard flags.contains(.command) || flags.contains(.control), let key = event.charactersIgnoringModifiers, !key.isEmpty else { settingStatus?.stringValue = "请包含 Command 或 Control 修饰键"; return }
        var modifiers: UInt32 = 0; var label = ""
        if flags.contains(.control) { modifiers |= UInt32(controlKey); label += "⌃" }
        if flags.contains(.option) { modifiers |= UInt32(optionKey); label += "⌥" }
        if flags.contains(.shift) { modifiers |= UInt32(shiftKey); label += "⇧" }
        if flags.contains(.command) { modifiers |= UInt32(cmdKey); label += "⌘" }
        label += key.uppercased()
        UserDefaults.standard.set(UInt32(event.keyCode), forKey: "capture.key")
        UserDefaults.standard.set(modifiers, forKey: "capture.mods")
        UserDefaults.standard.set(label, forKey: "capture.shortcutLabel")
        registerShortcut(); recorder?.needsDisplay = true
    }
}

private final class ShortcutRecorder: NSView {
    weak var owner: CaptureController?
    override var acceptsFirstResponder: Bool { true }
    override func mouseDown(with event: NSEvent) { owner?.beginRecording(); window?.makeFirstResponder(self); needsDisplay = true }
    override func performKeyEquivalent(with event: NSEvent) -> Bool { if window?.firstResponder === self { owner?.shortcut(event); return true }; return false }
    override func keyDown(with event: NSEvent) { owner?.shortcut(event) }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.controlBackgroundColor.setFill(); NSBezierPath(roundedRect: bounds, xRadius: 5, yRadius: 5).fill()
        let label = "\(owner?.shortcutLabel ?? "")  · 点击后按新快捷键"
        (label as NSString).draw(at: CGPoint(x: 12, y: 10), withAttributes: [.font: NSFont.systemFont(ofSize: 14), .foregroundColor: NSColor.labelColor])
    }
}

private final class SelectionView: NSView {
    let screen: NSScreen
    let image: CGImage
    weak var owner: CaptureController?
    var selection = CGRect.zero
    var start = CGPoint.zero
    var original = CGRect.zero
    var mode = 0 // 0 new, 1 move, 2 resize bottom-right
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    init(screen: NSScreen, image: CGImage, owner: CaptureController) {
        self.screen = screen; self.image = image; self.owner = owner
        super.init(frame: CGRect(origin: .zero, size: screen.frame.size))
    }
    required init?(coder: NSCoder) { fatalError() }
    override func resetCursorRects() { addCursorRect(bounds, cursor: .crosshair) }
    override func draw(_ dirtyRect: NSRect) {
        NSImage(cgImage: image, size: bounds.size).draw(in: bounds, from: .zero, operation: .copy, fraction: 1, respectFlipped: true, hints: nil)
        NSColor.black.withAlphaComponent(0.42).setFill()
        let path = NSBezierPath(rect: bounds); if !selection.isEmpty { path.appendRect(selection) }; path.windingRule = .evenOdd; path.fill()
        if !selection.isEmpty {
            NSColor.systemBlue.setStroke(); let border = NSBezierPath(rect: selection); border.lineWidth = 2; border.stroke()
            NSColor.white.setFill(); NSBezierPath(rect: CGRect(x: selection.maxX-4, y: selection.maxY-4, width: 8, height: 8)).fill()
        }
        let text = selection.isEmpty ? "拖动框选截图区域 · 松开直接编辑 · Esc 取消" : "\(Int(selection.width * screen.backingScaleFactor)) × \(Int(selection.height * screen.backingScaleFactor)) · 松开直接编辑"
        (text as NSString).draw(at: CGPoint(x: 24, y: 24), withAttributes: [.font: NSFont.systemFont(ofSize: 16), .foregroundColor: NSColor.white])
    }
    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self); start = convert(event.locationInWindow, from: nil); original = selection
        mode = CGRect(x: selection.maxX-12, y: selection.maxY-12, width: 24, height: 24).contains(start) && !selection.isEmpty ? 2 : selection.contains(start) ? 1 : 0
        owner?.toolbar?.orderOut(nil)
    }
    override func mouseDragged(with event: NSEvent) {
        var point = convert(event.locationInWindow, from: nil); point.x = min(max(0, point.x), bounds.width); point.y = min(max(0, point.y), bounds.height)
        if mode == 1 { selection = original.offsetBy(dx: min(max(point.x-start.x, -original.minX), bounds.width-original.maxX), dy: min(max(point.y-start.y, -original.minY), bounds.height-original.maxY)) }
        else if mode == 2 { selection = CGRect(x: original.minX, y: original.minY, width: max(4, point.x-original.minX), height: max(4, point.y-original.minY)).intersection(bounds) }
        else { selection = CGRect(x: min(point.x,start.x), y: min(point.y,start.y), width: abs(point.x-start.x), height: abs(point.y-start.y)) }
        needsDisplay = true
    }
    override func mouseUp(with event: NSEvent) {
        if selection.width >= 4 && selection.height >= 4 {
            let scale = screen.backingScaleFactor
            let pixelRect = CGRect(x: selection.minX*scale, y: selection.minY*scale, width: selection.width*scale, height: selection.height*scale).integral
            selection = CGRect(x: pixelRect.minX/scale, y: pixelRect.minY/scale, width: pixelRect.width/scale, height: pixelRect.height/scale).intersection(bounds)
            owner?.selected(self)
        }
    }
    override func keyDown(with event: NSEvent) { if event.keyCode == 53 { owner?.cancel() } else if event.keyCode == 36 { owner?.editSelection() } }
    func cropped() -> CGImage? {
        let sx = CGFloat(image.width)/bounds.width, sy = CGFloat(image.height)/bounds.height
        return image.cropping(to: CGRect(x: selection.minX*sx, y: selection.minY*sy, width: selection.width*sx, height: selection.height*sy).integral)
    }
}

private final class EditorController: NSObject, NSWindowDelegate {
    let window: NSWindow
    let canvas: EditorCanvas
    let scroll = NSScrollView()
    let onClose: () -> Void
    let onScroll: (() -> Void)?
    let colors = CapturePopup()
    let widths = CapturePopup()
    let fonts = CapturePopup()
    init(image: CGImage, onScroll: (() -> Void)? = nil, onClose: @escaping () -> Void) {
        self.onClose = onClose; self.onScroll = onScroll
        canvas = EditorCanvas(image: image)
        window = CapturePanel(contentRect: CGRect(x: 0, y: 0, width: 960, height: 700), styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        super.init()
        window.title = "截图标注"; window.isReleasedWhenClosed = false; window.delegate = self
        window.minSize = CGSize(width: 850, height: 400)
        window.appearance = NSAppearance(named: .darkAqua)
        let root = window.contentView!
        colors.addItems(withTitles: ["红色", "黄色", "蓝色", "黑色", "白色"])
        widths.addItems(withTitles: ["细框", "中框", "粗框"]); widths.selectItem(at: 1)
        fonts.addItems(withTitles: ["16", "24", "36", "48", "72"]); fonts.selectItem(at: 1)
        for control in [colors, widths, fonts] { control.target = self; control.action = #selector(styleChanged) }
        let redact = button("打码", self, #selector(redactTool))
        redact.toolTip = "拖动框选，用不透明黑色遮挡内容"
        var controls: [NSView] = [button("选择", self, #selector(selectTool)), button("画框", self, #selector(rectTool)), button("文字", self, #selector(textTool)), redact, colors, widths, fonts, button("撤销", self, #selector(undo)), button("重做", self, #selector(redo)), button("−", self, #selector(zoomOut)), button("+", self, #selector(zoomIn))]
        if onScroll != nil { controls.append(button("滚动截图", self, #selector(scrollCapture))) }
        controls += [button("复制", self, #selector(copyImage)), button("保存", self, #selector(saveImage))]
        // Keep annotation and output controls on separate rows. A single row
        // depends on AppKit's platform-specific intrinsic control widths and
        // can detach or clip buttons when the editor is narrowed.
        let annotationTools = NSStackView(views: Array(controls.prefix(7)))
        let outputTools = NSStackView(views: Array(controls.dropFirst(7)))
        for row in [annotationTools, outputTools] {
            row.spacing = 6; row.detachesHiddenViews = false
            row.heightAnchor.constraint(equalToConstant: 34).isActive = true
        }
        let tools = NSStackView(views: [annotationTools, outputTools])
        tools.orientation = .vertical; tools.alignment = .leading
        tools.detachesHiddenViews = false
        tools.spacing = 6; tools.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(tools)
        scroll.translatesAutoresizingMaskIntoConstraints = false; scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
        scroll.backgroundColor = NSColor(white: 0.10, alpha: 1)
        scroll.documentView = canvas; root.addSubview(scroll)
        NSLayoutConstraint.activate([tools.topAnchor.constraint(equalTo: root.topAnchor, constant: 8), tools.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 8), tools.trailingAnchor.constraint(lessThanOrEqualTo: root.trailingAnchor, constant: -8), scroll.topAnchor.constraint(equalTo: tools.bottomAnchor, constant: 8), scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor), scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor), scroll.bottomAnchor.constraint(equalTo: root.bottomAnchor)])
        canvas.owner = self
        canvas.setZoom(min(1, 900 / CGFloat(image.width)))
        window.center(); window.makeFirstResponder(canvas)
    }
    func windowWillClose(_ notification: Notification) { onClose() }
    @objc func selectTool() { canvas.finishText(); canvas.tool = .select; window.makeFirstResponder(canvas) }
    @objc func rectTool() { canvas.finishText(); canvas.tool = .rectangle; window.makeFirstResponder(canvas) }
    @objc func textTool() { canvas.finishText(); canvas.tool = .text; window.makeFirstResponder(canvas) }
    @objc func redactTool() { canvas.finishText(); canvas.tool = .redact; window.makeFirstResponder(canvas) }
    @objc func scrollCapture() { onScroll?() }
    @objc func styleChanged() {
        canvas.finishText()
        canvas.color = [NSColor.systemRed, .systemYellow, .systemBlue, .black, .white][colors.indexOfSelectedItem]
        canvas.stroke = [CGFloat(1), 3, 6][widths.indexOfSelectedItem]
        canvas.fontSize = CGFloat(Int(fonts.titleOfSelectedItem ?? "24") ?? 24)
        if let index = canvas.selected {
            canvas.document.checkpoint()
            canvas.document.marks[index].color = canvas.color
            canvas.document.marks[index].lineWidth = canvas.stroke
            canvas.document.marks[index].fontSize = canvas.fontSize
            canvas.needsDisplay = true
        }
    }
    @objc func undo() { canvas.finishText(); canvas.document.undo(); canvas.selected = nil; canvas.needsDisplay = true }
    @objc func redo() { canvas.finishText(); canvas.document.redo(); canvas.selected = nil; canvas.needsDisplay = true }
    @objc func zoomIn() { canvas.finishText(); canvas.setZoom(min(3, canvas.zoom * 1.25)) }
    @objc func zoomOut() { canvas.finishText(); canvas.setZoom(max(0.05, canvas.zoom / 1.25)) }
    @objc func copyImage() {
        canvas.finishText()
        let bitmap = canvas.document.render()
        guard var data = bitmap.representation(using: .png, properties: [:]) else { notify("无法生成图片"); return }
        if data.count > 32 * 1024 * 1024 {
            let alert = NSAlert(); alert.messageText = "图片超过 32 MiB 同步上限"; alert.informativeText = "可以缩小后复制，或取消并保存原图。"; alert.addButton(withTitle: "缩小并复制"); alert.addButton(withTitle: "取消")
            guard alert.runModal() == .alertFirstButtonReturn, let original = bitmap.cgImage else { return }
            var scale: CGFloat = 0.75
            repeat {
                let width = max(1, Int(CGFloat(original.width)*scale)), height = max(1, Int(CGFloat(original.height)*scale))
                guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
                ctx.interpolationQuality = .high; ctx.draw(original, in: CGRect(x: 0, y: 0, width: width, height: height))
                guard let scaled = ctx.makeImage(), let encoded = NSBitmapImageRep(cgImage: scaled).representation(using: .png, properties: [:]) else { return }
                data = encoded; scale *= 0.75
            } while data.count > 32 * 1024 * 1024 && scale > 0.01
        }
        guard data.count <= 32 * 1024 * 1024 else { notify("图片过大，请保存原图"); return }
        let item = NSPasteboardItem(); item.setData(data, forType: .png)
        NSPasteboard.general.clearContents()
        if NSPasteboard.general.writeObjects([item]) { window.title = "截图已复制 · 可在已连接设备取用" } else { notify("写入剪贴板失败") }
    }
    @objc func saveImage() {
        canvas.finishText()
        let panel = NSSavePanel(); panel.allowedContentTypes = [.png]; panel.nameFieldStringValue = "截图-\(Int(Date().timeIntervalSince1970)).png"
        panel.beginSheetModal(for: window) { response in
            guard response == .OK, let url = panel.url else { return }
            do {
                guard let data = self.canvas.document.render().representation(using: .png, properties: [:]) else { throw NSError(domain: "Capture", code: 1) }
                try data.write(to: url, options: .atomic); self.window.title = "截图已保存"
            } catch { notify("保存失败：\(error.localizedDescription)") }
        }
    }
}

private final class EditorCanvas: NSView {
    enum Tool { case select, rectangle, text, redact }
    var document: CaptureDocument
    weak var owner: EditorController?
    var tool: Tool = .rectangle
    var selected: Int?
    var color: NSColor = .systemRed
    var stroke: CGFloat = 3
    var fontSize: CGFloat = 24
    var zoom: CGFloat = 1
    var start = CGPoint.zero
    var original = CGRect.zero
    var resizing = false
    var dragging = false
    var textView: NSTextView?
    var textIndex: Int?
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    init(image: CGImage) { document = CaptureDocument(image: image); super.init(frame: CGRect(x: 0, y: 0, width: image.width, height: image.height)) }
    required init?(coder: NSCoder) { fatalError() }
    func setZoom(_ value: CGFloat) { zoom = value; frame.size = CGSize(width: CGFloat(document.image.width)*zoom, height: CGFloat(document.image.height)*zoom); needsDisplay = true }
    func location(_ event: NSEvent) -> CGPoint {
        let p = convert(event.locationInWindow, from: nil)
        return CGPoint(x: min(max(0, p.x/zoom), CGFloat(document.image.width)), y: min(max(0, p.y/zoom), CGFloat(document.image.height)))
    }
    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        context.saveGState(); context.scaleBy(x: zoom, y: zoom)
        let imageRect = CGRect(x: 0, y: 0, width: document.image.width, height: document.image.height)
        NSImage(cgImage: document.image, size: imageRect.size).draw(in: imageRect, from: .zero, operation: .copy, fraction: 1, respectFlipped: true, hints: nil)
        for (index, mark) in document.marks.enumerated() where index != textIndex { drawMark(mark) }
        if let selected, document.marks.indices.contains(selected) {
            let rect = document.marks[selected].rect
            NSColor.systemBlue.setStroke(); let path = NSBezierPath(rect: rect.insetBy(dx: -3/zoom, dy: -3/zoom)); path.lineWidth = 1/zoom; path.stroke()
            NSColor.systemBlue.setFill(); NSBezierPath(rect: CGRect(x: rect.maxX-4/zoom, y: rect.maxY-4/zoom, width: 8/zoom, height: 8/zoom)).fill()
        }
        context.restoreGState()
    }
    override func mouseDown(with event: NSEvent) {
        finishText(); window?.makeFirstResponder(self); start = location(event); dragging = false
        if tool == .select {
            selected = document.marks.indices.reversed().first { document.marks[$0].rect.insetBy(dx: -8/zoom, dy: -8/zoom).contains(start) }
            if let selected {
                if event.clickCount == 2 && document.marks[selected].kind == .text { editText(selected); return }
                original = document.marks[selected].rect
                resizing = CGRect(x: original.maxX-12/zoom, y: original.maxY-12/zoom, width: 24/zoom, height: 24/zoom).contains(start)
                document.checkpoint(); dragging = true
            }
        } else if tool == .text {
            document.checkpoint()
            document.marks.append(CaptureMark(kind: .text, rect: CGRect(x: start.x, y: start.y, width: min(360, CGFloat(document.image.width)-start.x), height: min(180, CGFloat(document.image.height)-start.y)), color: color, fontSize: fontSize))
            selected = document.marks.count-1; editText(selected!, checkpoint: false)
        } else {
            document.checkpoint(); document.marks.append(CaptureMark(kind: tool == .redact ? .redact : .rectangle, rect: CGRect(origin: start, size: .zero), color: color, lineWidth: stroke))
            selected = document.marks.count-1; dragging = true
        }
        needsDisplay = true
    }
    override func mouseDragged(with event: NSEvent) {
        guard dragging, let selected else { return }; let p = location(event)
        if tool == .rectangle || tool == .redact { document.marks[selected].rect = CGRect(x: min(start.x,p.x), y: min(start.y,p.y), width: abs(start.x-p.x), height: abs(start.y-p.y)) }
        else if resizing { document.marks[selected].rect.size = CGSize(width: max(4,p.x-original.minX), height: max(4,p.y-original.minY)) }
        else {
            document.marks[selected].rect = original.offsetBy(dx: min(max(p.x-start.x,-original.minX),CGFloat(document.image.width)-original.maxX), dy: min(max(p.y-start.y,-original.minY),CGFloat(document.image.height)-original.maxY))
        }
        needsDisplay = true
    }
    override func mouseUp(with event: NSEvent) {
        if dragging, let selected, document.marks[selected].rect.width < 2 || document.marks[selected].rect.height < 2 { document.marks.remove(at: selected); self.selected = nil }
        dragging = false; needsDisplay = true
    }
    func editText(_ index: Int, checkpoint: Bool = true) {
        if checkpoint { document.checkpoint() }; textIndex = index
        let mark = document.marks[index]
        let text = NSTextView(frame: CGRect(x: mark.rect.minX*zoom, y: mark.rect.minY*zoom, width: max(40,mark.rect.width*zoom), height: max(40,mark.rect.height*zoom)))
        text.string = mark.text; text.font = .systemFont(ofSize: mark.fontSize*zoom); text.textColor = mark.color
        text.backgroundColor = NSColor(white: 0.1, alpha: 0.85); text.isRichText = false; text.isVerticallyResizable = false; text.textContainerInset = .zero
        text.textContainer?.lineFragmentPadding = 0
        addSubview(text); textView = text; window?.makeFirstResponder(text); needsDisplay = true
    }
    func finishText() {
        guard let textView, let index = textIndex else { return }
        textView.unmarkText()
        document.marks[index].text = textView.string
        let mark = document.marks[index]
        let size = (mark.text as NSString).boundingRect(with: CGSize(width: mark.rect.width, height: CGFloat.greatestFiniteMagnitude), options: [.usesLineFragmentOrigin, .usesFontLeading], attributes: [.font: NSFont.systemFont(ofSize: mark.fontSize)]).size
        document.marks[index].rect.size.height = min(CGFloat(document.image.height)-mark.rect.minY, max(mark.fontSize+4, ceil(size.height)+4))
        textView.removeFromSuperview(); self.textView = nil; textIndex = nil
        if document.marks[index].text.isEmpty { document.marks.remove(at: index); selected = nil }
        needsDisplay = true
    }
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard window?.firstResponder === self, event.modifierFlags.contains(.command) else { return false }
        switch event.charactersIgnoringModifiers?.lowercased() {
        case "c": owner?.copyImage()
        case "s": owner?.saveImage()
        case "z": if event.modifierFlags.contains(.shift) { owner?.redo() } else { owner?.undo() }
        default: return false
        }; return true
    }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 51 || event.keyCode == 117 {
            if let selected { document.checkpoint(); document.marks.remove(at: selected); self.selected = nil; needsDisplay = true }
        } else if event.keyCode == 53 { window?.close() }
    }
}

#if CAPTURE_TEST
func captureRunUITests(image: CGImage, output: URL) throws {
    func mouse(_ type: NSEvent.EventType, in view: NSView, at point: CGPoint) -> NSEvent {
        NSEvent.mouseEvent(with: type, location: view.convert(point, to: nil), modifierFlags: [], timestamp: 0, windowNumber: view.window!.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1)!
    }
    let controller = CaptureController()
    controller.busy = true
    let selection = SelectionView(screen: NSScreen.main!, image: image, owner: controller)
    let overlay = CapturePanel(contentRect: CGRect(x: 0, y: 0, width: image.width, height: image.height), styleMask: [.borderless], backing: .buffered, defer: false)
    overlay.isReleasedWhenClosed = false; overlay.contentView = selection
    selection.frame.size = CGSize(width: image.width, height: image.height)
    controller.overlays = [overlay]
    selection.mouseDown(with: mouse(.leftMouseDown, in: selection, at: CGPoint(x: 20, y: 20)))
    selection.mouseUp(with: mouse(.leftMouseUp, in: selection, at: CGPoint(x: 20, y: 20)))
    precondition(controller.editor == nil, "an empty click must not capture")
    selection.mouseDown(with: mouse(.leftMouseDown, in: selection, at: CGPoint(x: 20, y: 20)))
    selection.mouseDragged(with: mouse(.leftMouseDragged, in: selection, at: CGPoint(x: 220, y: 140)))
    selection.mouseUp(with: mouse(.leftMouseUp, in: selection, at: CGPoint(x: 220, y: 140)))
    precondition(controller.editor?.canvas.document.image.width == 200 && controller.editor?.canvas.document.image.height == 120, "mouse release must immediately show the cropped editor")
    precondition(controller.overlays.isEmpty && controller.toolbar == nil && controller.busy)
    precondition(controller.editor?.onScroll != nil && controller.selectedView === selection, "scroll capture must remain available")
    controller.editor?.window.close()
    precondition(!controller.busy && controller.editor == nil && controller.selectedView == nil)

    var scrollRequested = false
    let editor = EditorController(image: image, onScroll: { scrollRequested = true }, onClose: {})
    editor.window.setContentSize(CGSize(width: 960, height: 700))
    editor.window.makeKeyAndOrderFront(nil)
    editor.scrollCapture(); precondition(scrollRequested)
    editor.redactTool()
    let canvas = editor.canvas
    canvas.mouseDown(with: mouse(.leftMouseDown, in: canvas, at: CGPoint(x: 100, y: 90)))
    canvas.mouseDragged(with: mouse(.leftMouseDragged, in: canvas, at: CGPoint(x: 30, y: 30)))
    canvas.mouseUp(with: mouse(.leftMouseUp, in: canvas, at: CGPoint(x: 30, y: 30)))
    precondition(canvas.document.marks.count == 1 && canvas.document.marks[0].kind == .redact)
    precondition(canvas.document.marks[0].rect == CGRect(x: 30, y: 30, width: 70, height: 60))
    // Changing the text/frame color must never make redaction transparent.
    editor.colors.selectItem(at: 4); editor.styleChanged()
    let redacted = PixelFrame(canvas.document.render().cgImage!)
    let original = PixelFrame(image)
    for y in 0..<image.height { for x in 0..<image.width {
        let i = (y * image.width + x) * 4
        if (30..<100).contains(x) && (30..<90).contains(y) {
            precondition(Array(redacted.bytes[i..<i+4]) == [0, 0, 0, 255], "exported redaction must fully cover the source pixels")
        } else {
            precondition(redacted.bytes[i..<i+4] == original.bytes[i..<i+4], "redaction must preserve pixels outside its bounds")
        }
    } }
    editor.undo(); editor.undo(); precondition(canvas.document.marks.isEmpty)
    editor.redo(); precondition(canvas.document.marks[0].kind == .redact)
    editor.canvas.document.marks = [
        CaptureMark(kind: .rectangle, rect: CGRect(x: 20, y: 20, width: 160, height: 60), color: .systemRed),
        CaptureMark(kind: .text, rect: CGRect(x: 20, y: 110, width: 210, height: 60), text: "截图 Test", color: .white),
        CaptureMark(kind: .redact, rect: CGRect(x: 30, y: 85, width: 120, height: 20))
    ]
    editor.canvas.selected = 1
    editor.canvas.editText(1)
    editor.canvas.textView!.string = "中文输入\n第二行"
    editor.canvas.finishText()
    precondition(editor.canvas.document.marks[1].text == "中文输入\n第二行")
    editor.undo(); precondition(editor.canvas.document.marks[1].text == "截图 Test")
    editor.redo(); precondition(editor.canvas.document.marks[1].text == "中文输入\n第二行")
    editor.canvas.setZoom(2)
    precondition(editor.canvas.frame.width == CGFloat(image.width)*2)
    editor.window.contentView!.layoutSubtreeIfNeeded()
    let toolbar = editor.window.contentView!.subviews.compactMap { $0 as? NSStackView }.first!
    for width: CGFloat in [850, 960, 1200, 850] {
        editor.window.setContentSize(CGSize(width: width, height: 700))
        editor.window.contentView!.layoutSubtreeIfNeeded()
        let root = editor.window.contentView!
        // Half a point allows AppKit's subpixel rounding at row boundaries.
        let visibleBounds = root.bounds.insetBy(dx: -0.5, dy: -0.5)
        var controlCount = 0
        for row in toolbar.arrangedSubviews.compactMap({ $0 as? NSStackView }) {
            for control in row.arrangedSubviews {
                controlCount += 1
                let rect = control.convert(control.bounds, to: root)
                precondition(control.superview === row && !control.isHidden && control.frame.width >= control.fittingSize.width - 0.5 && control.frame.height > 0 && visibleBounds.contains(rect), "all editor controls must remain visible and fit at window width \(width): \(control), frame \(rect), content \(root.bounds)")
            }
        }
        precondition(controlCount == 14, "no editor tools may be detached when the window shrinks")
        precondition(editor.scroll.frame.maxY <= toolbar.frame.minY + 0.5, "the toolbar must not overlap the image")
    }
    editor.window.displayIfNeeded()
    RunLoop.current.run(until: Date().addingTimeInterval(0.3))
    let view = editor.window.contentView!
    let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)!
    view.effectiveAppearance.performAsCurrentDrawingAppearance { view.cacheDisplay(in: view.bounds, to: rep) }
    try rep.representation(using: .png, properties: [:])!.write(to: output)
    editor.window.close()
    // Register a rarely used test chord twice. The second registration must
    // fail rather than stealing the first registration; release our own ref.
    var first: EventHotKeyRef?, second: EventHotKeyRef?
    let mods = UInt32(cmdKey | controlKey | optionKey | shiftKey)
    let result = RegisterEventHotKey(UInt32(kVK_F19), mods, EventHotKeyID(signature: 0x54455354, id: 100), GetApplicationEventTarget(), 0, &first)
    if result == noErr {
        defer { if let first { UnregisterEventHotKey(first) }; if let second { UnregisterEventHotKey(second) } }
        precondition(RegisterEventHotKey(UInt32(kVK_F19), mods, EventHotKeyID(signature: 0x54455354, id: 101), GetApplicationEventTarget(), 0, &second) != noErr)
    }
    print("PASS: immediate editor on selection release, empty selection, close cleanup, scroll entry, opaque redaction pixels and undo/redo, native editor layout, Chinese multiline text commit, zoom and non-stealing hotkey registration")
}
#endif

import AppKit
import Carbon
import ImageIO
import QuickLookThumbnailing
import SwiftUI

private let history = ClipboardHistoryController()

@_cdecl("clip_history_start")
public func clipHistoryStart() { history.start() }

@_cdecl("clip_history_open")
public func clipHistoryOpen() { history.show() }

@_cdecl("clip_history_stop")
public func clipHistoryStop() { history.stop() }

private final class ClipboardHistoryController: NSObject, ObservableObject, NSWindowDelegate {
    @Published private(set) var records: [ClipboardRecord] = []
    @Published var query = "" { didSet { reconcileSelection() } }
    @Published var filter: ClipboardKind? { didSet { reconcileSelection() } }
    @Published private(set) var selection: String?
    @Published private(set) var preview: [ClipboardItem]?
    @Published private(set) var githubPreviews: [URL: GitHubSocialPreview] = [:]
    @Published private(set) var details: [ClipboardDetail] = []
    @Published var showsActions = false
    @Published private(set) var status: String?
    @Published private(set) var pasteTarget: String?
    @Published private(set) var canPasteDirectly = false

    private let thumbnailCache = NSCache<NSString, ClipboardThumbnail>()
    private let io = DispatchQueue(label: "com.cyeam.miclip.clipboard-history", qos: .utility)
    private lazy var store = Result {
        try ClipboardHistoryStore(root: FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/MiClip/ClipboardHistory", isDirectory: true))
    }
    private var timer: Timer?
    private var lastChange: Int?
    private var pendingBytes = 0
    private var panel: NSPanel?
    private var keyMonitor: Any?
    private var hotkey: EventHotKeyRef?
    private var hotkeyHandler: EventHandlerRef?
    private var previousApp: NSRunningApplication?
    private var previewRequest = 0
    private var pendingRestore: Bool?
    private var githubTask: Task<Void, Never>?
    private var pasteRequest = 0
    private var didPromptForAccessibility = false

    var shortcut: String? { hotkey == nil ? nil : "⌥·" }
    var visibleRecords: [ClipboardRecord] {
        let search = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return records.filter { record in
            (filter.map { record.kinds.contains($0) } ?? true) &&
                (search.isEmpty || record.title.localizedStandardContains(search) ||
                    record.searchable.localizedStandardContains(search) ||
                    record.source?.localizedStandardContains(search) == true)
        }
    }
    var selected: ClipboardRecord? { records.first { $0.id == selection } }

    func start() {
        guard timer == nil else { return }
        thumbnailCache.countLimit = 100
        mutate { _ in }
        let timer = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in self?.poll() }
        self.timer = timer
        RunLoop.main.add(timer, forMode: .common)
        var eventType = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        if InstallEventHandler(GetApplicationEventTarget(), { _, event, _ in
            var id = EventHotKeyID()
            guard GetEventParameter(event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
                                    nil, MemoryLayout<EventHotKeyID>.size, nil, &id) == noErr,
                  id.signature == 0x434C4849, id.id == 1 else { return OSStatus(eventNotHandledErr) }
            DispatchQueue.main.async {
                if history.panel?.isVisible == true { history.dismiss(restoreFocus: true) }
                else { history.show() }
            }
            return noErr
        }, 1, &eventType, nil, &hotkeyHandler) == noErr {
            RegisterEventHotKey(UInt32(kVK_ANSI_Grave), UInt32(optionKey),
                                EventHotKeyID(signature: 0x434C4849, id: 1), GetApplicationEventTarget(), 0, &hotkey)
        }
        poll()
    }

    func stop() {
        timer?.invalidate()
        timer = nil
        if let hotkey { UnregisterEventHotKey(hotkey); self.hotkey = nil }
        if let handler = hotkeyHandler { RemoveEventHandler(handler); hotkeyHandler = nil }
        dismiss(restoreFocus: false)
        io.sync {}
    }

    private func poll() {
        let board = NSPasteboard.general
        let change = board.changeCount
        guard lastChange != change else { return }
        let budget = ClipboardHistoryStore.capacity - pendingBytes
        // A burst of large clipboard writes must not queue unbounded snapshots.
        guard budget > 0 else { return }
        switch ClipboardSnapshot.read(board, change: change, limit: budget) {
        case .changed: return
        case .ignored: lastChange = change
        case .tooLarge:
            if pendingBytes == 0 {
                lastChange = change
                status = "此项目超过剪贴板历史的存储上限"
            }
        case let .captured(items):
            lastChange = change
            let bytes = items.reduce(0) { $0 + $1.formats.values.reduce(0) { $0 + $1.count } }
            let source = NSWorkspace.shared.frontmostApplication?.localizedName
            let date = Date()
            pendingBytes += bytes
            mutate(releasing: bytes) { try $0.save(items, date: date, source: source) }
        }
    }

    private func mutate(releasing bytes: Int = 0, _ operation: @escaping (ClipboardHistoryStore) throws -> Void) {
        io.async {
            let result = self.store.flatMap { store in Result { try operation(store); return store.records } }
            DispatchQueue.main.async {
                self.pendingBytes -= bytes
                switch result {
                case let .success(records):
                    self.status = nil
                    self.records = records
                    self.reconcileSelection()
                case .failure:
                    self.status = "无法保存或读取剪贴板历史"
                }
            }
        }
    }

    func show() {
        if timer == nil { start() }
        let alreadyVisible = panel?.isVisible == true
        if !alreadyVisible {
            pasteRequest += 1
            let front = NSWorkspace.shared.frontmostApplication
            previousApp = front?.processIdentifier == ProcessInfo.processInfo.processIdentifier ? nil : front
            pasteTarget = previousApp?.localizedName
            canPasteDirectly = AXIsProcessTrusted()
            poll()
            query = ""
            filter = nil
            installKeyMonitor()
            select(records.first?.id)
            if panel == nil {
                let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 700, height: 400),
                                    styleMask: [.titled, .closable, .resizable, .fullSizeContentView],
                                    backing: .buffered, defer: false)
                panel.title = "剪贴板历史"
                panel.titleVisibility = .hidden
                panel.titlebarAppearsTransparent = true
                panel.standardWindowButton(.closeButton)?.isHidden = true
                panel.standardWindowButton(.miniaturizeButton)?.isHidden = true
                panel.standardWindowButton(.zoomButton)?.isHidden = true
                panel.isMovableByWindowBackground = true
                panel.isReleasedWhenClosed = false
                panel.minSize = NSSize(width: 580, height: 360)
                panel.collectionBehavior = [.moveToActiveSpace, .fullScreenAuxiliary]
                panel.delegate = self
                self.panel = panel
            }
            panel?.contentView = NSHostingView(rootView: ClipboardHistoryView(model: self))
            if let panel, let screen = NSScreen.screens.first(where: { $0.frame.contains(NSEvent.mouseLocation) }) ?? NSScreen.main {
                let frame = screen.visibleFrame
                let size = panel.frame.size
                panel.setFrameOrigin(NSPoint(x: frame.midX - size.width / 2, y: frame.midY - size.height / 2))
            }
        }
        NSApp.activate(ignoringOtherApps: true)
        panel?.makeKeyAndOrderFront(nil)
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        dismiss(restoreFocus: true)
        return false
    }

    func close() { dismiss(restoreFocus: true) }

    private func dismiss(restoreFocus: Bool) {
        showsActions = false
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor); self.keyMonitor = nil }
        panel?.orderOut(nil)
        panel?.contentView = nil
        if restoreFocus, let previousApp {
            if #available(macOS 14.0, *), NSApp.isActive {
                NSApp.yieldActivation(to: previousApp)
                previousApp.activate(from: .current, options: [])
            } else {
                previousApp.activate(options: [])
            }
        }
        previousApp = nil
        previewRequest += 1
        pendingRestore = nil
        preview = nil
        details = []
        githubTask?.cancel()
        githubTask = nil
        githubPreviews = [:]
        selection = nil
    }

    private func reconcileSelection() {
        guard keyMonitor != nil else { return }
        let rows = visibleRecords
        if !rows.contains(where: { $0.id == selection }) { select(rows.first?.id) }
    }

    func select(_ id: String?) {
        guard keyMonitor != nil, selection != id else { return }
        selection = id
        pendingRestore = nil
        preview = nil
        details = []
        githubTask?.cancel()
        githubTask = nil
        githubPreviews = [:]
        previewRequest += 1
        guard let id else { return }
        let request = previewRequest
        io.async {
            let result = self.store.flatMap { store in
                Result {
                    let items = try store.load(id)
                    return (items, ClipboardDetail.make(from: items))
                }
            }
            DispatchQueue.main.async {
                guard request == self.previewRequest else { return }
                switch result {
                case let .success((items, details)):
                    self.preview = items
                    self.details = details
                    if let paste = self.pendingRestore {
                        self.pendingRestore = nil
                        self.restore(paste: paste)
                    } else {
                        self.loadGitHubPreviews(items)
                    }
                case .failure:
                    self.pendingRestore = nil
                    self.status = "无法读取所选项目"
                }
            }
        }
    }

    private func loadGitHubPreviews(_ items: [ClipboardItem]) {
        let urls = items.compactMap { $0.kind == .link ? $0.webURL : nil }
            .filter { GitHubPreviewClient.pageURL(for: $0) != nil }
        guard !urls.isEmpty else { return }
        githubTask = Task { @MainActor [weak self] in
            var visited: Set<URL> = []
            for url in urls where visited.insert(url).inserted {
                guard !Task.isCancelled else { return }
                let preview = await GitHubPreviewClient.load(url)
                guard !Task.isCancelled, let self else { return }
                self.githubPreviews[url] = preview
            }
        }
    }

    func loadThumbnail(for id: String, completion: @escaping (ClipboardThumbnail?) -> Void) {
        if let cached = thumbnailCache.object(forKey: id as NSString) { completion(cached); return }
        io.async {
            let items = try? self.store.get().load(id)
            let images = items?.first(where: { $0.kind == .image })?.images ?? []
            let thumbnail = ClipboardThumbnail.make(from: images, size: 56)
            if let thumbnail {
                self.thumbnailCache.setObject(thumbnail, forKey: id as NSString)
            }
            completion(thumbnail)
        }
    }

    func removeSelected() {
        guard let selection else { return }
        mutate {
            try $0.remove(selection)
            self.thumbnailCache.removeObject(forKey: selection as NSString)
        }
    }

    func clear() {
        poll()
        mutate {
            try $0.remove(nil)
            self.thumbnailCache.removeAllObjects()
        }
    }

    func requestAccessibility() {
        didPromptForAccessibility = true
        let key = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        canPasteDirectly = AXIsProcessTrustedWithOptions([key: true] as CFDictionary)
    }

    func restore(paste: Bool, recordID: String? = nil) {
        guard keyMonitor != nil else { return }
        if let recordID { select(recordID) }
        guard selection != nil else { return }
        guard let preview else {
            pendingRestore = paste
            return
        }
        let board = NSPasteboard.general
        // Keep a recent copy even if the user restores history before the next tick.
        poll()
        guard ClipboardSnapshot.restore(preview, to: board) else {
            status = "macOS 无法恢复此剪贴板项目"
            return
        }
        let count = board.changeCount
        lastChange = count
        let target = previousApp
        dismiss(restoreFocus: true)
        guard paste, let target else { return }
        if !AXIsProcessTrusted(), !didPromptForAccessibility { requestAccessibility() }
        guard AXIsProcessTrusted() else { return }
        pasteRequest += 1
        pasteWhenActive(target, count: count, request: pasteRequest, remaining: 20)
    }

    private func pasteWhenActive(_ target: NSRunningApplication, count: Int, request: Int, remaining: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.06) {
            guard request == self.pasteRequest, !target.isTerminated,
                  NSPasteboard.general.changeCount == count else { return }
            guard NSWorkspace.shared.frontmostApplication?.processIdentifier == target.processIdentifier else {
                if remaining > 0 { self.pasteWhenActive(target, count: count, request: request, remaining: remaining - 1) }
                return
            }
            let key = CGKeyCode(kVK_ANSI_V)
            let down = CGEvent(keyboardEventSource: nil, virtualKey: key, keyDown: true)
            let up = CGEvent(keyboardEventSource: nil, virtualKey: key, keyDown: false)
            down?.flags = .maskCommand
            up?.flags = .maskCommand
            down?.postToPid(target.processIdentifier)
            up?.postToPid(target.processIdentifier)
        }
    }

    private func installKeyMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self, event.window === self.panel,
                  (self.panel?.firstResponder as? NSTextView)?.hasMarkedText() != true else { return event }
            let mods = event.modifierFlags.intersection([.command, .control, .option, .shift])
            switch Int(event.keyCode) {
            case kVK_Escape where mods.isEmpty:
                if self.showsActions { self.showsActions = false }
                else { self.close() }
            case kVK_ANSI_K where mods == .command:
                self.showsActions.toggle()
            case kVK_Return, kVK_ANSI_KeypadEnter:
                guard mods.isEmpty || mods == .command else { return event }
                self.restore(paste: mods.isEmpty)
            case kVK_UpArrow, kVK_DownArrow:
                guard mods.isEmpty else { return event }
                let rows = self.visibleRecords
                guard let index = rows.firstIndex(where: { $0.id == self.selection }) else { return nil }
                let next = index + (Int(event.keyCode) == kVK_UpArrow ? -1 : 1)
                if rows.indices.contains(next) { self.select(rows[next].id) }
            default: return event
            }
            return nil
        }
    }
}

private struct ClipboardHistoryView: View {
    @ObservedObject var model: ClipboardHistoryController
    @FocusState private var searchFocused: Bool
    @State private var confirmClear = false

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HStack(spacing: 0) {
                list.frame(width: 224)
                Divider()
                detail.frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            Divider()
            footer
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .ignoresSafeArea(.container, edges: .top)
        .onAppear { DispatchQueue.main.async { searchFocused = true } }
    }

    private var header: some View {
        HStack(spacing: 7) {
            Button { model.close() } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 12, weight: .semibold))
                    .frame(width: 23, height: 23)
            }
            .buttonStyle(.borderless)
            .background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
            .accessibilityLabel("关闭剪贴板历史")
            HStack(spacing: 7) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("搜索剪贴板历史", text: $model.query)
                    .textFieldStyle(.plain)
                    .focused($searchFocused)
            }
            .padding(.horizontal, 9)
            .frame(height: 25)
            .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 6))
            Picker("类型", selection: $model.filter) {
                Text("全部类型").tag(nil as ClipboardKind?)
                ForEach(ClipboardKind.allCases, id: \.self) { kind in
                    Label(kind.label, systemImage: kind.icon).tag(Optional(kind))
                }
            }
            .labelsHidden()
            .controlSize(.small)
            .frame(width: 104)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    private var list: some View {
        let calendar = Calendar.current
        let groups = Dictionary(grouping: model.visibleRecords) { calendar.startOfDay(for: $0.copiedAt) }
        return ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(groups.keys.sorted(by: >), id: \.self) { day in
                        Text(calendar.isDateInToday(day) ? "今天" : calendar.isDateInYesterday(day) ? "昨天" :
                                day.formatted(date: .abbreviated, time: .omitted))
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .padding(.leading, 10)
                            .padding(.top, 7)
                            .padding(.bottom, 2)
                            .accessibilityAddTraits(.isHeader)
                        ForEach(groups[day] ?? []) { record in
                            ClipboardHistoryRow(record: record, selected: model.selection == record.id, model: model)
                                .id(record.id)
                        }
                    }
                }
                .padding(.horizontal, 6)
                .padding(.bottom, 6)
            }
            .onChange(of: model.selection) { id in if let id { proxy.scrollTo(id) } }
        }
        .background(Color(nsColor: .controlBackgroundColor).opacity(0.55))
    }

    @ViewBuilder private var detail: some View {
        if let record = model.selected {
            VStack(alignment: .leading, spacing: 0) {
                if let items = model.preview, items.count == 1, let item = items.first,
                   item.kind == .link, let url = item.webURL, let image = model.githubPreviews[url]?.image {
                    GitHubPreviewImage(url: url, image: image, title: model.githubPreviews[url]?.title)
                        .padding(6)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if let items = model.preview {
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 8) {
                            ForEach(items.indices, id: \.self) { index in
                                let item = items[index]
                                ClipboardItemPreview(item: item, githubPreview: item.webURL.flatMap { model.githubPreviews[$0] })
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(10)
                    }
                    .id(record.id)
                } else if model.status == nil {
                    ProgressView().controlSize(.small).frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    Text(model.status ?? "").frame(maxWidth: .infinity, maxHeight: .infinity)
                }
                Divider()
                information(record)
            }
        } else {
            VStack(spacing: 10) {
                Image(systemName: model.records.isEmpty ? "clipboard" : "magnifyingglass")
                    .font(.system(size: 28, weight: .light))
                Text(model.records.isEmpty ? "还没有剪贴板记录" : "没有符合条件的记录")
                    .font(.headline)
                if model.records.isEmpty {
                    Text("复制文本、文件、图片或链接后，会显示在这里。")
                }
            }
            .foregroundStyle(.secondary)
        }
    }

    private func information(_ record: ClipboardRecord) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("项目属性")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .accessibilityAddTraits(.isHeader)
            ScrollView {
                VStack(spacing: 0) {
                    property("来源", record.source ?? "未知")
                    let kind = model.details.first { $0.name == "类型" }?.value ?? record.kind.label
                    property("类型", kind)
                    if let items = model.preview, items.count == 1, let item = items.first,
                       item.kind == .link, let url = item.webURL {
                        if let title = model.githubPreviews[url]?.title { property("标题", title) }
                        property("网址", url.absoluteString)
                    }
                    ForEach(model.details.filter { $0.name != "类型" }, id: \.name) { detail in
                        property(detail.name, detail.value)
                    }
                    if record.itemCount > 1 { property("项目数", record.itemCount.formatted()) }
                    property("复制次数", (record.copyCount ?? 1).formatted())
                    property("复制时间", record.copiedAt.formatted(date: .abbreviated, time: .standard))
                    property("存储大小", ByteCountFormatter.string(fromByteCount: Int64(record.bytes), countStyle: .file))
                }
            }
            .id(record.id)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .frame(height: 128)
        .background(Color(nsColor: .controlBackgroundColor).opacity(0.48))
    }

    private func property(_ name: String, _ value: String) -> some View {
        VStack(spacing: 0) {
            HStack {
                Text(name).foregroundStyle(.secondary)
                Spacer(minLength: 8)
                Text(value).lineLimit(1).textSelection(.enabled)
            }
            .font(.callout)
            .padding(.vertical, 2)
            Divider()
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            Label("剪贴板历史", systemImage: "clipboard")
                .foregroundStyle(.secondary)
                .fixedSize()
            if let message = model.status {
                Text(message).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 0)
            Button(pasteLabel) { model.restore(paste: true) }
                .buttonStyle(.borderedProminent)
                .lineLimit(1)
                .disabled(model.preview == nil)
            Button {
                model.showsActions.toggle()
            } label: {
                Label("操作  ⌘K", systemImage: "ellipsis.circle")
            }
            .fixedSize()
            .popover(isPresented: $model.showsActions, arrowEdge: .top) {
                VStack(alignment: .leading, spacing: 5) {
                    Button("复制到剪贴板  ⌘↵") {
                        model.showsActions = false
                        model.restore(paste: false)
                    }
                    .disabled(model.preview == nil)
                    if model.pasteTarget != nil && !model.canPasteDirectly {
                        Button("授权直接粘贴…") {
                            model.showsActions = false
                            model.requestAccessibility()
                        }
                    }
                    Divider()
                    Button("删除所选项目") {
                        model.showsActions = false
                        model.removeSelected()
                    }
                    .disabled(model.selected == nil)
                    Button("清空历史…") {
                        model.showsActions = false
                        confirmClear = true
                    }
                    .disabled(model.records.isEmpty)
                }
                .controlSize(.small)
                .frame(width: 155, alignment: .leading)
                .padding(10)
            }
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
        .controlSize(.small)
        .alert("清空剪贴板历史？", isPresented: $confirmClear) {
            Button("取消", role: .cancel) {}
            Button("清空", role: .destructive) { model.clear() }
        }
    }

    private var pasteLabel: String {
        if let target = model.pasteTarget {
            return model.canPasteDirectly ? "粘贴到 \(target)  ↵" : "复制后手动粘贴  ↵"
        }
        return "恢复到剪贴板  ↵"
    }
}

private struct ClipboardHistoryRow: View {
    let record: ClipboardRecord
    let selected: Bool
    let model: ClipboardHistoryController
    @State private var thumbnail: ClipboardThumbnail?

    var body: some View {
        Button { model.select(record.id) } label: {
            HStack(spacing: 8) {
                Group {
                    if let thumbnail {
                        Image(decorative: thumbnail.image, scale: 1).resizable().scaledToFit()
                    } else {
                        Image(systemName: record.kind.icon)
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                    }
                }
                .frame(width: 20, height: 20)
                .clipShape(RoundedRectangle(cornerRadius: 4))
                Text(thumbnail.map { "图片（\($0.dimensions)）" } ?? record.title)
                    .font(.system(size: 13, weight: selected ? .medium : .regular))
                    .lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(selected ? Color.accentColor.opacity(0.13) : .clear,
                        in: RoundedRectangle(cornerRadius: 6))
            .overlay(alignment: .leading) {
                if selected {
                    Capsule().fill(Color.accentColor).frame(width: 2, height: 17).padding(.leading, 2)
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .simultaneousGesture(TapGesture(count: 2).onEnded {
            model.restore(paste: true, recordID: record.id)
        })
        .accessibilityAddTraits(selected ? .isSelected : [])
        .task(id: record.id) {
            guard record.kind == .image, record.itemCount == 1 else { return }
            let image = await withCheckedContinuation { continuation in
                model.loadThumbnail(for: record.id) { continuation.resume(returning: $0) }
            }
            if !Task.isCancelled { thumbnail = image }
        }
    }
}

private struct ClipboardDetail {
    let name: String
    let value: String

    static func make(from items: [ClipboardItem]) -> [Self] {
        guard items.count == 1, let item = items.first else { return [] }
        switch item.kind {
        case .text:
            guard let text = item.plainText else { return [] }
            let richTypes: Set<String> = [
                NSPasteboard.PasteboardType.rtf.rawValue,
                NSPasteboard.PasteboardType.rtfd.rawValue,
                NSPasteboard.PasteboardType.html.rawValue
            ]
            var words = 0
            text.enumerateSubstrings(in: text.startIndex..<text.endIndex,
                                     options: [.byWords, .substringNotRequired]) { _, _, _, _ in words += 1 }
            return [
                Self(name: "类型", value: richTypes.isDisjoint(with: item.formats.keys) ? "纯文本" : "富文本"),
                Self(name: "字符数", value: text.count.formatted()),
                Self(name: "词数", value: words.formatted())
            ]
        case .link: return []
        case .image:
            for data in item.images {
                if let source = CGImageSourceCreateWithData(data as CFData, nil),
                   let dimensions = ClipboardThumbnail.dimensions(of: source) {
                    return [Self(name: "像素尺寸", value: dimensions)]
                }
            }
            return []
        case .file, .other: return []
        }
    }
}

private final class ClipboardThumbnail {
    let image: CGImage
    let dimensions: String

    init(image: CGImage, dimensions: String) {
        self.image = image
        self.dimensions = dimensions
    }

    static func dimensions(of source: CGImageSource) -> String? {
        guard let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Int,
              let height = properties[kCGImagePropertyPixelHeight] as? Int else { return nil }
        return "\(width) × \(height)"
    }

    static func make(from images: [Data], size: Int) -> ClipboardThumbnail? {
        for data in images {
            guard let source = CGImageSourceCreateWithData(data as CFData, nil),
                  let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                    kCGImageSourceCreateThumbnailFromImageAlways: true,
                    kCGImageSourceCreateThumbnailWithTransform: true,
                    kCGImageSourceThumbnailMaxPixelSize: size
                  ] as CFDictionary) else { continue }
            return ClipboardThumbnail(image: image, dimensions: dimensions(of: source) ?? "\(image.width) × \(image.height)")
        }
        return nil
    }
}

private struct ClipboardItemPreview: View {
    let item: ClipboardItem
    let githubPreview: GitHubSocialPreview?
    @State private var image: NSImage?
    @State private var dimensions: String?

    @ViewBuilder var body: some View {
        switch item.kind {
        case .file:
            ForEach(Array(item.fileURLs.enumerated()), id: \.offset) { _, url in
                ClipboardFilePreview(url: url)
            }
        case .image:
            VStack(alignment: .leading, spacing: 8) {
                if let image {
                    Image(nsImage: image)
                        .resizable().scaledToFit()
                        .frame(maxWidth: .infinity, maxHeight: 290)
                        .padding(10)
                        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
                } else {
                    Label("图片预览", systemImage: "photo")
                }
                if let dimensions { Text(dimensions).font(.caption).foregroundStyle(.secondary) }
            }
            .task {
                let images = item.images
                let result = await Task.detached(priority: .userInitiated) {
                    ClipboardThumbnail.make(from: images, size: 1_400)
                }.value
                guard !Task.isCancelled else { return }
                if let result { image = NSImage(cgImage: result.image, size: .zero) }
                dimensions = result.map { "\($0.dimensions) px" } ?? "此格式无法生成预览"
            }
        case .link:
            if let url = item.webURL {
                if let image = githubPreview?.image {
                    GitHubPreviewImage(url: url, image: image, title: githubPreview?.title)
                        .frame(maxHeight: 188)
                } else {
                    ClipboardLinkPreview(url: url, title: githubPreview?.title)
                }
            }
        case .text:
            Text(item.plainText ?? "此格式没有可显示的文本")
                .textSelection(.enabled)
                .font(.system(size: 14))
                .frame(maxWidth: .infinity, alignment: .leading)
        case .other:
            VStack(alignment: .leading, spacing: 8) {
                ForEach(item.formats.keys.sorted(), id: \.self) { type in
                    LabeledClipboardFormat(type: type, size: item.formats[type]?.count ?? 0)
                }
            }
        }
    }
}

private struct LabeledClipboardFormat: View {
    let type: String
    let size: Int

    var body: some View {
        HStack {
            Label(type, systemImage: "doc.zipper").textSelection(.enabled)
            Spacer()
            Text(ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file))
                .foregroundStyle(.secondary)
        }
    }
}

private struct ClipboardFilePreview: View {
    let url: URL
    @State private var thumbnail: NSImage?
    @State private var location: URL?
    @State private var exists = false
    @State private var size: Int?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let thumbnail {
                Image(nsImage: thumbnail)
                    .resizable().scaledToFit()
                    .frame(maxWidth: .infinity, maxHeight: 195)
            }
            HStack(alignment: .top, spacing: 10) {
                let file = location ?? url
                Image(nsImage: NSWorkspace.shared.icon(forFile: file.path))
                    .resizable().frame(width: 24, height: 24)
                VStack(alignment: .leading, spacing: 4) {
                    Text(file.lastPathComponent).font(.headline).textSelection(.enabled)
                    Text(file.path).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                    if let size {
                        Text(ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    if !exists { Text("源文件不可用").font(.caption).foregroundStyle(.secondary) }
                }
                Spacer(minLength: 0)
                Button("在 Finder 显示") { NSWorkspace.shared.activateFileViewerSelecting([file]) }
                    .disabled(!exists)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
        .task {
            let file = url.clipboardFilePath
            location = file
            exists = FileManager.default.fileExists(atPath: file.path)
            guard exists else { return }
            size = try? file.resourceValues(forKeys: [.fileSizeKey]).fileSize
            let request = QLThumbnailGenerator.Request(fileAt: file, size: CGSize(width: 460, height: 260),
                                                       scale: 2, representationTypes: [.lowQualityThumbnail, .thumbnail])
            if let result = try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: request),
               !Task.isCancelled {
                thumbnail = result.nsImage
            }
        }
    }
}

private struct GitHubPreviewImage: View {
    let url: URL
    let image: CGImage
    let title: String?

    var body: some View {
        Button { NSWorkspace.shared.open(url) } label: {
            Image(decorative: image, scale: 1)
                .resizable()
                .scaledToFit()
                .clipShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(title ?? "GitHub 社交预览")
        .accessibilityHint("在浏览器打开")
        .help("在浏览器打开")
    }
}

private struct ClipboardLinkPreview: View {
    let url: URL
    let title: String?

    var body: some View {
        let github = GitHubClipboardLink(url)
        VStack(alignment: .leading, spacing: 8) {
            Label(title ?? github?.title ?? url.host ?? "链接",
                  systemImage: github?.icon ?? "globe")
                .font(.system(size: 14, weight: .semibold))
                .textSelection(.enabled)
            Text(url.absoluteString)
                .font(.callout)
                .textSelection(.enabled)
                .foregroundStyle(.secondary)
            Button("在浏览器打开") { NSWorkspace.shared.open(url) }
        }
        .padding(13)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 8))
    }
}

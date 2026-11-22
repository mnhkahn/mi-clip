import AppKit
import CryptoKit
import UniformTypeIdentifiers

enum ClipboardKind: String, CaseIterable, Codable {
    case text, image, file, link, other

    var label: String {
        switch self {
        case .text: return "文本"
        case .image: return "图片"
        case .file: return "文件"
        case .link: return "链接"
        case .other: return "其他"
        }
    }

    var icon: String {
        switch self {
        case .text: return "text.alignleft"
        case .image: return "photo"
        case .file: return "doc"
        case .link: return "link"
        case .other: return "clipboard"
        }
    }
}

extension URL {
    var clipboardFilePath: URL { (self as NSURL).filePathURL ?? self }
}

struct ClipboardItem: Codable {
    let types: [String]
    let formats: [String: Data]

    func string(_ type: NSPasteboard.PasteboardType) -> String? {
        formats[type.rawValue].flatMap { String(data: $0, encoding: .utf8) }
    }

    var fileURLs: [URL] {
        if let value = string(.fileURL), let url = URL(string: value), url.isFileURL,
           url.host == nil || url.host == "" || url.host == "localhost" {
            return [url]
        }
        if let data = formats["NSFilenamesPboardType"],
           let paths = (try? PropertyListSerialization.propertyList(from: data, format: nil)) as? [String] {
            return paths.map { URL(fileURLWithPath: $0) }
        }
        return []
    }

    var images: [Data] {
        let types = formats.keys.filter { UTType($0)?.conforms(to: .image) == true }
        let preferred = ["public.png", "public.jpeg", "com.compuserve.gif", "public.tiff"]
        return (preferred + types.sorted().filter { !preferred.contains($0) }).compactMap { formats[$0] }
    }

    var plainText: String? {
        if let text = string(.string) { return text }
        if let rtf = formats[NSPasteboard.PasteboardType.rtf.rawValue] {
            return (try? NSAttributedString(data: rtf, options: [.documentType: NSAttributedString.DocumentType.rtf], documentAttributes: nil))?.string
        }
        return string(.html)
    }

    var webURL: URL? {
        for candidate in [string(.URL), string(.string)] {
            guard let value = candidate?.trimmingCharacters(in: .whitespacesAndNewlines),
                  value.rangeOfCharacter(from: .whitespacesAndNewlines) == nil,
                  let url = URL(string: value), let host = url.host, !host.isEmpty,
                  ["https", "http"].contains(url.scheme?.lowercased() ?? "") else { continue }
            return url
        }
        return nil
    }

    var kind: ClipboardKind {
        if !fileURLs.isEmpty { return .file }
        if !images.isEmpty { return .image }
        if webURL != nil { return .link }
        if formats.keys.contains(where: { UTType($0)?.conforms(to: .text) == true }) { return .text }
        return .other
    }

    var label: String {
        switch kind {
        case .file: return fileURLs.first.map { $0.clipboardFilePath.lastPathComponent } ?? kind.label
        case .image: return "剪贴板图片"
        case .link:
            guard let url = webURL else { return kind.label }
            return GitHubClipboardLink(url)?.title ?? url.absoluteString
        case .text:
            // Use the first nonempty line in the list, with the full text in the preview.
            return plainText?.split(whereSeparator: \.isNewline)
                .first(where: { !$0.trimmingCharacters(in: .whitespaces).isEmpty })
                .map { String($0.prefix(160)) } ?? "空白文本"
        case .other: return formats.keys.sorted().first ?? kind.label
        }
    }
}

struct GitHubClipboardLink {
    let repository: String
    let title: String
    let detail: String
    let icon: String

    init?(_ url: URL) {
        guard ["github.com", "www.github.com", "gist.github.com"].contains(url.host?.lowercased() ?? "") else {
            return nil
        }
        let parts = url.pathComponents.filter { $0 != "/" }
        repository = parts.prefix(2).joined(separator: "/")
        let tail = Array(parts.dropFirst(2))
        if tail.count >= 2, tail[0] == "pull", Int(tail[1]) != nil {
            detail = "Pull Request"
            icon = "arrow.triangle.branch"
            title = "\(repository) #\(tail[1])"
        } else if tail.count >= 2, tail[0] == "issues", Int(tail[1]) != nil {
            detail = "Issue"
            icon = "exclamationmark.circle"
            title = "\(repository) #\(tail[1])"
        } else if tail.count >= 2, tail[0] == "commit" {
            detail = "Commit"
            icon = "point.topleft.down.to.point.bottomright.curvepath"
            title = "\(repository) · \(tail[1].prefix(7))"
        } else if tail.count >= 3, tail[0] == "blob" {
            detail = "文件"
            icon = "doc.text"
            title = tail.last ?? repository
        } else if tail.first == "releases" {
            detail = "Release"
            icon = "tag"
            title = tail.count > 2 ? "\(repository) · \(tail[2])" : repository
        } else {
            detail = url.host?.lowercased() == "gist.github.com" ? "Gist" : parts.count >= 2 ? "仓库" : "GitHub"
            icon = "book.closed"
            title = repository.isEmpty ? "GitHub" : repository
        }
    }
}

struct ClipboardRecord: Identifiable, Codable {
    let id: String
    let title: String
    let kinds: [ClipboardKind]
    let searchable: String
    let itemCount: Int
    let bytes: Int
    var copiedAt: Date
    var source: String?
    var copyCount: Int?

    var kind: ClipboardKind { kinds.first ?? .other }

    init(id: String, items: [ClipboardItem], bytes: Int, copiedAt: Date, source: String?) {
        self.id = id
        self.bytes = bytes
        self.copiedAt = copiedAt
        self.source = source
        copyCount = 1
        itemCount = max(items.count, items.reduce(0) { $0 + $1.fileURLs.count })
        kinds = [.file, .image, .link, .text, .other].filter { kind in items.contains { $0.kind == kind } }
        let primaryKind = kinds.first
        let first = items.first(where: { $0.kind == primaryKind })
        let label = first?.label ?? "剪贴板项目"
        title = itemCount > 1 ? "\(String(label.prefix(160))) +\(itemCount - 1)" : String(label.prefix(160))
        var fields: [String] = []
        var remaining = 8192
        for item in items where remaining > 0 {
            let candidates = item.fileURLs.map { $0.clipboardFilePath.path }
                + [item.webURL?.absoluteString, item.plainText].compactMap { $0 }
            for value in candidates where remaining > 0 {
                let snippet = value.prefix(remaining)
                fields.append(String(snippet))
                remaining -= max(1, snippet.count)
            }
        }
        searchable = fields.joined(separator: "\n")
    }
}

enum ClipboardSnapshot {
    case changed
    case ignored
    case tooLarge
    case captured([ClipboardItem])

    static func read(_ board: NSPasteboard, change: Int, limit: Int) -> Self {
        let items = board.pasteboardItems ?? []
        let privateTypes: Set<String> = ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType"]
        let promisedTypes: Set<String> = [
            "com.apple.pasteboard.promised-file-url", "com.apple.pasteboard.promised-file-content-type", "NSFilesPromisePboardType"
        ]
        let types = items.flatMap { $0.types.map(\.rawValue) }
        if !privateTypes.isDisjoint(with: types) || !promisedTypes.isDisjoint(with: types) {
            return board.changeCount == change ? .ignored : .changed
        }
        var result: [ClipboardItem] = []
        var remaining = limit
        for item in items {
            var formats: [String: Data] = [:]
            for type in item.types {
                guard let data = item.data(forType: type) else { continue }
                guard data.count <= remaining else { return board.changeCount == change ? .tooLarge : .changed }
                remaining -= data.count
                formats[type.rawValue] = data
            }
            if !formats.isEmpty { result.append(ClipboardItem(types: item.types.map(\.rawValue).filter { formats[$0] != nil }, formats: formats)) }
        }
        guard board.changeCount == change else { return .changed }
        return result.isEmpty ? .ignored : .captured(result)
    }

    static func restore(_ items: [ClipboardItem], to board: NSPasteboard) -> Bool {
        guard !items.isEmpty else { return false }
        let restored = items.map { item in
            let result = NSPasteboardItem()
            for type in item.types {
                if let data = item.formats[type] { result.setData(data, forType: .init(type)) }
            }
            return result
        }
        guard restored.allSatisfy({ !$0.types.isEmpty }) else { return false }
        board.clearContents()
        return board.writeObjects(restored)
    }
}

// All mutations run on the history I/O queue. Payload files are immutable and
// the index is replaced atomically before old payloads are removed.
final class ClipboardHistoryStore {
    static let capacity = 512 * 1024 * 1024
    static let entryLimit = 500
    private let root: URL
    private var index: URL { root.appendingPathComponent("index.json") }
    private let capacity: Int
    private let entryLimit: Int
    private(set) var records: [ClipboardRecord] = []

    init(root: URL, capacity: Int = capacity, entryLimit: Int = entryLimit) throws {
        self.root = root
        self.capacity = capacity
        self.entryLimit = entryLimit
        let files = FileManager.default
        try files.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try files.setAttributes([.posixPermissions: 0o700], ofItemAtPath: root.path)
        if files.fileExists(atPath: index.path) {
            records = try JSONDecoder().decode([ClipboardRecord].self, from: Data(contentsOf: index))
                .filter { $0.id.count == 64 && $0.id.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) } }
                .filter { files.fileExists(atPath: payload($0.id).path) }
        }
        let live = Set(records.map { payload($0.id).lastPathComponent })
        for file in try files.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            where file.pathExtension == "plist" && !live.contains(file.lastPathComponent) {
            try? files.removeItem(at: file)
        }
    }

    private func payload(_ id: String) -> URL { root.appendingPathComponent(id + ".plist") }

    func save(_ items: [ClipboardItem], date: Date, source: String?) throws {
        // Hash in deterministic order. Length prefixes separate adjacent fields.
        var hash = SHA256()
        func append(_ data: Data) {
            var count = UInt64(data.count).bigEndian
            withUnsafeBytes(of: &count) { hash.update(bufferPointer: $0) }
            hash.update(data: data)
        }
        for item in items {
            append(Data(String(item.types.count).utf8))
            for type in item.types {
                append(Data(type.utf8))
                append(item.formats[type] ?? Data())
            }
        }
        let id = hash.finalize().map { String(format: "%02x", $0) }.joined()
        if var record = records.first(where: { $0.id == id }) {
            record.copiedAt = date
            record.source = source
            record.copyCount = (record.copyCount ?? 1) + 1
            try commit([record] + records.filter { $0.id != id })
            return
        }
        let encoder = PropertyListEncoder()
        encoder.outputFormat = .binary
        let data = try encoder.encode(items)
        guard data.count <= capacity else { throw CocoaError(.fileWriteOutOfSpace) }
        let record = ClipboardRecord(id: id, items: items, bytes: data.count, copiedAt: date, source: source)
        var next = [record]
        var available = capacity - record.bytes
        for old in records.prefix(entryLimit - 1) {
            guard old.bytes <= available else { break }
            available -= old.bytes
            next.append(old)
        }
        try data.write(to: payload(id), options: .atomic)
        do {
            try commit(next)
        } catch {
            try? FileManager.default.removeItem(at: payload(id))
            throw error
        }
    }

    func load(_ id: String) throws -> [ClipboardItem] {
        guard records.contains(where: { $0.id == id }) else { throw CocoaError(.fileNoSuchFile) }
        return try PropertyListDecoder().decode([ClipboardItem].self, from: Data(contentsOf: payload(id)))
    }

    func remove(_ id: String?) throws {
        try commit(id.map { id in records.filter { $0.id != id } } ?? [])
    }

    private func commit(_ next: [ClipboardRecord]) throws {
        try JSONEncoder().encode(next).write(to: index, options: .atomic)
        let retained = Set(next.map(\.id))
        let removed = records.filter { !retained.contains($0.id) }
        records = next
        for record in removed {
            try? FileManager.default.removeItem(at: payload(record.id))
        }
    }
}

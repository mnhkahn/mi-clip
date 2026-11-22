import AppKit

@main
struct ClipboardHistoryTest {
    static func items(_ board: NSPasteboard) -> [ClipboardItem] {
        guard case let .captured(items) = ClipboardSnapshot.read(board, change: board.changeCount, limit: 65_536) else {
            fatalError("Expected captured items")
        }
        return items
    }

    static func main() throws {
        let board = NSPasteboard.withUniqueName()
        let destination = NSPasteboard.withUniqueName()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer {
            board.releaseGlobally()
            destination.releaseGlobally()
            try? FileManager.default.removeItem(at: root)
        }

        let richText = NSPasteboardItem()
        let text = String(repeating: "长文本", count: 1_100)
        let rtf = Data("{\\rtf1\\ansi {\\b bold}}".utf8)
        precondition(richText.setString(text, forType: .string))
        precondition(richText.setData(rtf, forType: .rtf))
        precondition(board.writeObjects([richText]))
        let rich = items(board)
        precondition(rich.first?.plainText == text && rich.first?.kind == .text)
        precondition(ClipboardSnapshot.restore(rich, to: destination))
        precondition(destination.string(forType: .string) == text)
        precondition(destination.data(forType: .rtf) == rtf)
        precondition(destination.pasteboardItems?.first?.types == board.pasteboardItems?.first?.types)

        let urls = ["/tmp/测试图片.png", "/tmp/text file.txt"].map { URL(fileURLWithPath: $0) }
        board.clearContents()
        precondition(board.writeObjects(urls.map { $0 as NSURL }))
        let files = items(board)
        precondition(files.count == 2 && files.allSatisfy { $0.kind == .file })
        precondition(files.flatMap(\.fileURLs) == urls)
        precondition(ClipboardSnapshot.restore(files, to: destination))
        let pastedFiles = destination.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]
        precondition(pastedFiles == urls)

        let png = Data(base64Encoded: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jR1sAAAAASUVORK5CYII=")!
        let image = NSPasteboardItem()
        precondition(image.setData(png, forType: .png))
        precondition(image.setString("fallback", forType: .string))
        board.clearContents()
        precondition(board.writeObjects([image]))
        let images = items(board)
        precondition(images.first?.kind == .image && images.first?.images.first == png)
        precondition(ClipboardSnapshot.restore(images, to: destination))
        precondition(destination.data(forType: .png) == png)
        guard case .tooLarge = ClipboardSnapshot.read(board, change: board.changeCount, limit: 2) else {
            fatalError("Expected size limit")
        }
        guard case .changed = ClipboardSnapshot.read(board, change: board.changeCount - 1, limit: 8192) else {
            fatalError("Expected stale snapshot to be rejected")
        }

        let addresses = [
            "https://github.com/cyeam/mi-clip",
            "https://github.com/cyeam/mi-clip/issues/23",
            "https://github.com/cyeam/mi-clip/pull/24",
            "https://github.com/cyeam/mi-clip/commit/0123456789",
            "https://github.com/cyeam/mi-clip/blob/main/README.md",
            "https://github.com/cyeam/mi-clip/releases/tag/v0.3.2",
            "https://gist.github.com/cyeam/0123456789",
            "https://github.com.example.org/cyeam/mi-clip/issues/23"
        ]
        let styles = ["仓库", "Issue", "Pull Request", "Commit", "文件", "Release", "Gist", nil]
        for (address, style) in zip(addresses, styles) {
            board.clearContents()
            let url = URL(string: address)!
            precondition(board.writeObjects([url as NSURL]))
            let link = items(board)
            precondition(link.first?.kind == .link && link.first?.webURL == url)
            precondition(GitHubClipboardLink(url)?.detail == style)
            precondition(ClipboardSnapshot.restore(link, to: destination))
            precondition(destination.string(forType: .URL) == address)
        }

        let graph = GitHubPreviewClient.OpenGraph(html: """
            <head>
                <META content='GitHub - Test &amp; tools &#x2192; &quot;Preview&quot; > files' property='og:title'>
                <meta property="og:image" content="https://repository-images.githubusercontent.com/test.png?one=1&amp;two=2" />
            </head>
            <body><meta property="og:title" content="Wrong title"></body>
            """)
        precondition(graph.title == "Test & tools → \"Preview\" > files")
        precondition(graph.image == "https://repository-images.githubusercontent.com/test.png?one=1&two=2")
        let bodyOnly = GitHubPreviewClient.OpenGraph(html: "<head></head><meta property='og:title' content='Wrong'>")
        precondition(bodyOnly.title == nil && bodyOnly.image == nil)
        let publicPage = GitHubPreviewClient.pageURL(for: URL(string: "http://www.github.com/owner/repo?token=secret#anchor")!)
        precondition(publicPage?.absoluteString == "https://github.com/owner/repo")
        for unsafe in ["https://github.com.example.org/test", "https://user:secret@github.com/test",
                       "https://github.com:8443/test", "file://github.com/test"] {
            precondition(GitHubPreviewClient.pageURL(for: URL(string: unsafe)!) == nil)
        }

        for marker in ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "com.apple.pasteboard.promised-file-url"] {
            let marked = NSPasteboardItem()
            precondition(marked.setString("synthetic", forType: .string))
            precondition(marked.setData(Data(), forType: .init(marker)))
            board.clearContents()
            precondition(board.writeObjects([marked]))
            guard case .ignored = ClipboardSnapshot.read(board, change: board.changeCount, limit: 8192) else {
                fatalError("Expected protected or non-replayable clipboard item to be ignored")
            }
        }

        let date = Date(timeIntervalSince1970: 100)
        let store = try ClipboardHistoryStore(root: root, entryLimit: 2)
        try store.save(rich, date: date, source: "Pages")
        let richID = store.records[0].id
        try store.save(files, date: date, source: "Finder")
        precondition(store.records[0].itemCount == 2)
        try store.save(rich, date: date.addingTimeInterval(10), source: "Notes")
        precondition(store.records.count == 2 && store.records[0].id == richID)
        precondition(store.records[0].source == "Notes" && store.records[0].copyCount == 2)
        let reopened = try ClipboardHistoryStore(root: root, entryLimit: 2)
        precondition(reopened.records[0].copyCount == 2)
        let reloaded = try reopened.load(richID)
        precondition(reloaded.first?.plainText == text && reloaded.first?.formats[NSPasteboard.PasteboardType.rtf.rawValue] == rtf)
        try reopened.save(images, date: date, source: nil)
        precondition(reopened.records.map(\.kind) == [.image, .text])
        try reopened.remove(richID)
        precondition(reopened.records.map(\.kind) == [.image])
        try reopened.remove(nil)
        let cleared = try ClipboardHistoryStore(root: root)
        precondition(cleared.records.isEmpty)
        let payloads = try FileManager.default.contentsOfDirectory(atPath: root.path).filter { $0.hasSuffix(".plist") }
        precondition(payloads.isEmpty)
        print("PASS: original formats, long text, files, images, GitHub links, private markers, and persistent history")
    }
}

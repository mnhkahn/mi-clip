import Foundation
import ImageIO

final class GitHubSocialPreview {
    let title: String?
    let image: CGImage?

    init(title: String?, image: CGImage?) {
        self.title = title
        self.image = image
    }
}

enum GitHubPreviewClient {
    private static let pageHosts: Set<String> = ["github.com", "www.github.com", "gist.github.com"]
    private static let imageHosts: Set<String> = [
        "repository-images.githubusercontent.com", "opengraph.githubassets.com",
        "avatars.githubusercontent.com", "github.githubassets.com"
    ]

    private static let cache: NSCache<NSURL, GitHubSocialPreview> = {
        let cache = NSCache<NSURL, GitHubSocialPreview>()
        cache.countLimit = 32
        cache.totalCostLimit = 32 * 1024 * 1024
        return cache
    }()

    private static let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.httpCookieStorage = nil
        config.urlCredentialStorage = nil
        config.urlCache = nil
        config.timeoutIntervalForRequest = 8
        config.timeoutIntervalForResource = 12
        return URLSession(configuration: config, delegate: RedirectPolicy(), delegateQueue: nil)
    }()

    static func pageURL(for url: URL) -> URL? {
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let host = components.host?.lowercased(), pageHosts.contains(host),
              ["https", "http"].contains(components.scheme?.lowercased() ?? ""),
              components.port == nil || (components.scheme?.lowercased() == "https" && components.port == 443),
              components.user == nil, components.password == nil else { return nil }
        var publicURL = components
        publicURL.scheme = "https"
        publicURL.host = host == "www.github.com" ? "github.com" : host
        publicURL.port = nil
        publicURL.query = nil
        publicURL.fragment = nil
        return publicURL.url
    }

    static func load(_ url: URL) async -> GitHubSocialPreview? {
        guard let page = pageURL(for: url) else { return nil }
        if let cached = cache.object(forKey: page as NSURL) { return cached }
        do {
            let html = try await download(page, mimePrefix: "text/html", limit: 768 * 1024, headOnly: true)
            let metadata = OpenGraph(html: String(decoding: html, as: UTF8.self))
            let imageURL = metadata.image.flatMap { URL(string: $0) }.flatMap(validImageURL)
            var image: CGImage?
            if let imageURL,
               let bytes = try? await download(imageURL, mimePrefix: "image/", limit: 5 * 1024 * 1024),
               let source = CGImageSourceCreateWithData(bytes as CFData, nil) {
                image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                    kCGImageSourceCreateThumbnailFromImageAlways: true,
                    kCGImageSourceCreateThumbnailWithTransform: true,
                    kCGImageSourceThumbnailMaxPixelSize: 1_280
                ] as CFDictionary)
            }
            try Task.checkCancellation()
            guard metadata.title != nil || image != nil else { return nil }
            let preview = GitHubSocialPreview(title: metadata.title, image: image)
            if imageURL == nil || image != nil {
                cache.setObject(preview, forKey: page as NSURL,
                                cost: image.map { $0.bytesPerRow * $0.height } ?? 0)
            }
            return preview
        } catch {
            return nil
        }
    }

    private static func validImageURL(_ url: URL) -> URL? {
        guard let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
              parts.scheme?.lowercased() == "https",
              parts.port == nil || parts.port == 443,
              parts.user == nil, parts.password == nil,
              let host = parts.host?.lowercased(), imageHosts.contains(host) else { return nil }
        return url
    }

    private static func download(_ url: URL, mimePrefix: String, limit: Int, headOnly: Bool = false) async throws -> Data {
        let (stream, response) = try await session.bytes(from: url)
        guard let http = response as? HTTPURLResponse, http.statusCode == 200,
              http.mimeType?.lowercased().hasPrefix(mimePrefix) == true,
              headOnly || http.expectedContentLength <= Int64(limit) else {
            stream.task.cancel()
            throw URLError(.badServerResponse)
        }
        var data = Data()
        data.reserveCapacity(min(limit, 128 * 1024))
        let endOfHead = Data("</head>".utf8)
        for try await byte in stream {
            data.append(byte)
            if data.count == limit || (headOnly && data.count.isMultiple(of: 8192) &&
                data.range(of: endOfHead, in: max(0, data.count - 8200)..<data.count) != nil) {
                stream.task.cancel()
                if headOnly { return data }
                throw URLError(.dataLengthExceedsMaximum)
            }
        }
        return data
    }

    private final class RedirectPolicy: NSObject, URLSessionTaskDelegate {
        func urlSession(_ session: URLSession, task: URLSessionTask,
                        willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
                        completionHandler: @escaping (URLRequest?) -> Void) {
            guard let original = task.originalRequest?.url, let target = request.url,
                  target.scheme?.lowercased() == "https" else { completionHandler(nil); return }
            let safeURL = pageURL(for: original) != nil ? pageURL(for: target) : validImageURL(target)
            completionHandler(safeURL.map { URLRequest(url: $0) })
        }
    }

    struct OpenGraph {
        let title: String?
        let image: String?

        private static let meta = try! NSRegularExpression(pattern: #"<meta\b(?:[^"'<>]|"[^"]*"|'[^']*')*+>"#, options: .caseInsensitive)
        private static let attribute = try! NSRegularExpression(pattern: #"([\w:-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
        private static let entity = try! NSRegularExpression(pattern: #"&(#x[0-9a-fA-F]+|#[0-9]+|amp|quot|apos|lt|gt|nbsp);"#)

        init(html: String) {
            var fields: [String: String] = [:]
            let head = html.range(of: "</head>", options: .caseInsensitive).map { String(html[..<$0.lowerBound]) } ?? html
            for match in Self.meta.matches(in: head, range: NSRange(head.startIndex..., in: head)) {
                guard let range = Range(match.range, in: head) else { continue }
                let tag = String(head[range])
                var attributes: [String: Substring] = [:]
                for entry in Self.attribute.matches(in: tag, range: NSRange(tag.startIndex..., in: tag)) {
                    guard let key = Range(entry.range(at: 1), in: tag),
                          let value = Range(entry.range(at: 2), in: tag) ?? Range(entry.range(at: 3), in: tag) else { continue }
                    attributes[tag[key].lowercased()] = tag[value]
                }
                if let key = attributes["property"]?.lowercased(), fields[key] == nil,
                   let raw = attributes["content"], ["og:title", "og:image", "og:image:secure_url"].contains(key) {
                    fields[key] = Self.decode(String(raw))
                }
            }
            let title = fields["og:title"]?.trimmingCharacters(in: .whitespacesAndNewlines)
            self.title = title.flatMap { $0.isEmpty ? nil :
                $0.hasPrefix("GitHub - ") ? String($0.dropFirst("GitHub - ".count)) : $0 }
            image = fields["og:image:secure_url"] ?? fields["og:image"]
        }

        private static func decode(_ value: String) -> String {
            let decoded = NSMutableString(string: value)
            let names = ["amp": "&", "quot": "\"", "apos": "'", "lt": "<", "gt": ">", "nbsp": " "]
            for match in entity.matches(in: value, range: NSRange(value.startIndex..., in: value)).reversed() {
                guard let tokenRange = Range(match.range(at: 1), in: value) else { continue }
                let token = value[tokenRange]
                let code = token.hasPrefix("#x") ? UInt32(token.dropFirst(2), radix: 16) : UInt32(token.dropFirst())
                if let replacement = names[String(token)] ?? code.flatMap(UnicodeScalar.init).map(String.init) {
                    decoded.replaceCharacters(in: match.range, with: replacement)
                }
            }
            return decoded as String
        }
    }
}

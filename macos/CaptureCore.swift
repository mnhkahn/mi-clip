import Cocoa

// Pixel coordinates always have their origin at the top-left. No screen data
// leaves this process until the user explicitly copies or saves the result.
struct PixelFrame {
    let width: Int
    let height: Int
    var bytes: [UInt8]
    init(_ image: CGImage) {
        width = image.width; height = image.height
        bytes = [UInt8](repeating: 0, count: width * height * 4)
        bytes.withUnsafeMutableBytes { memory in
            let context = CGContext(data: memory.baseAddress, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        }
    }
    var image: CGImage {
        let provider = CGDataProvider(data: Data(bytes) as CFData)!
        return CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue), provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)!
    }
    init(width: Int, height: Int, bytes: [UInt8]) { self.width = width; self.height = height; self.bytes = bytes }
    func difference(_ other: PixelFrame, shift: Int = 0) -> Double {
        guard width == other.width, height == other.height else { return 255 }
        let margin = max(2, height / 12)
        let bottom = min(height - margin, height - shift)
        guard bottom > margin else { return 255 }
        var sum: Double = 0; var count = 0
        for y in stride(from: margin, to: bottom, by: max(1, height / 100)) {
            for x in stride(from: max(1, width / 20), to: width * 19 / 20, by: max(1, width / 80)) {
                let a = ((y + shift) * width + x) * 4, b = (y * width + x) * 4
                for c in 0..<3 { sum += Double(abs(Int(bytes[a+c]) - Int(other.bytes[b+c]))); count += 1 }
            }
        }
        return sum / Double(max(1, count))
    }
}

struct ScrollAssembler {
    enum Result: Equatable { case unchanged, appended(Int), uncertain, limit }
    private(set) var frames: [PixelFrame]
    private(set) var shifts: [Int] = []
    static let maxPixels = 40_000_000
    init(_ image: CGImage) { frames = [PixelFrame(image)] }
    var height: Int { frames[0].height + shifts.reduce(0, +) }
    mutating func append(_ image: CGImage) -> Result {
        let next = PixelFrame(image), previous = frames.last!
        guard next.width == previous.width, next.height == previous.height else { return .uncertain }
        if previous.difference(next) < 1.2 { return .unchanged }
        // Retain at least 25% overlap. Refuse weak/ambiguous matches instead of
        // silently producing a corrupted long screenshot.
        let maximum = previous.height * 3 / 4
        guard maximum > 8 else { return .uncertain }
        let step = max(1, maximum / 180)
        var candidates: [(Int, Double)] = []
        for shift in stride(from: 4, through: maximum, by: step) { candidates.append((shift, previous.difference(next, shift: shift))) }
        let coarse = candidates.min(by: { $0.1 < $1.1 })!
        var best = coarse
        for shift in max(1, coarse.0-step)...min(maximum, coarse.0+step) {
            let error = previous.difference(next, shift: shift)
            if error < best.1 { best = (shift, error) }
        }
        let alternative = candidates.filter { abs($0.0-best.0) > max(12, step*3) }.map(\.1).min() ?? 255
        guard best.1 < 10, alternative > best.1 + 2 else { return .uncertain }
        guard (height + best.0) * next.width <= Self.maxPixels,
              frames.reduce(0, { $0 + $1.bytes.count }) + next.bytes.count <= 256 * 1024 * 1024 else { return .limit }
        frames.append(next); shifts.append(best.0)
        return .appended(height)
    }
    mutating func undo() { if frames.count > 1 { frames.removeLast(); shifts.removeLast() } }
    func composed() -> CGImage {
        let width = frames[0].width
        var bytes = frames[0].bytes
        for index in 1..<frames.count {
            let frame = frames[index], shift = shifts[index-1]
            bytes.append(contentsOf: frame.bytes.suffix(shift * width * 4))
        }
        return PixelFrame(width: width, height: height, bytes: bytes).image
    }
}

struct CaptureMark {
    enum Kind { case rectangle, text, redact }
    var kind: Kind
    var rect: CGRect
    var text = ""
    var color: NSColor = .systemRed
    var lineWidth: CGFloat = 3
    var fontSize: CGFloat = 24
}

struct CaptureDocument {
    let image: CGImage
    var marks: [CaptureMark] = []
    var undoStack: [[CaptureMark]] = []
    var redoStack: [[CaptureMark]] = []
    mutating func checkpoint() { undoStack.append(marks); if undoStack.count > 100 { undoStack.removeFirst() }; redoStack.removeAll() }
    mutating func undo() { if let previous = undoStack.popLast() { redoStack.append(marks); marks = previous } }
    mutating func redo() { if let next = redoStack.popLast() { undoStack.append(marks); marks = next } }
    func render() -> NSBitmapImageRep {
        let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: image.width, pixelsHigh: image.height, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        let context = NSGraphicsContext(bitmapImageRep: bitmap)!
        NSGraphicsContext.saveGraphicsState(); NSGraphicsContext.current = context
        let cg = context.cgContext
        let rect = CGRect(x: 0, y: 0, width: image.width, height: image.height)
        cg.draw(image, in: rect)
        cg.translateBy(x: 0, y: CGFloat(image.height)); cg.scaleBy(x: 1, y: -1)
        NSGraphicsContext.current = NSGraphicsContext(cgContext: cg, flipped: true)
        for mark in marks { drawMark(mark) }
        NSGraphicsContext.restoreGraphicsState()
        return bitmap
    }
}
func drawMark(_ mark: CaptureMark) {
    mark.color.setStroke()
    if mark.kind == .rectangle {
        let path = NSBezierPath(rect: mark.rect); path.lineWidth = mark.lineWidth; path.stroke()
    } else if mark.kind == .redact {
        // Always opaque, independent of annotation color and stroke settings.
        NSColor.black.setFill()
        NSBezierPath(rect: mark.rect.integral).fill()
    } else {
        (mark.text as NSString).draw(in: mark.rect, withAttributes: [.font: NSFont.systemFont(ofSize: mark.fontSize), .foregroundColor: mark.color])
    }
}

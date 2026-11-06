import Cocoa

@main struct CaptureTests {
    static func main() throws {
        let _ = NSApplication.shared
        NSApp.appearance = NSAppearance(named: .darkAqua)
        let width = 240, height = 180
        var bytes = [UInt8](repeating: 255, count: width * 540 * 4)
        var seed: UInt32 = 1234
        for y in 0..<540 { for x in 0..<width {
            seed = seed &* 1664525 &+ 1013904223
            let i = (y*width+x)*4
            bytes[i] = UInt8(truncatingIfNeeded: seed >> 24)
            bytes[i+1] = UInt8(truncatingIfNeeded: seed >> 16)
            bytes[i+2] = UInt8(truncatingIfNeeded: seed >> 8)
        } }
        func frame(_ top: Int) -> CGImage {
            PixelFrame(width: width, height: height, bytes: Array(bytes[(top*width*4)..<((top+height)*width*4)])).image
        }
        let first = frame(0)
        precondition(PixelFrame(first).bytes == Array(bytes.prefix(width*height*4)), "pixel conversion must preserve image orientation")
        var assembler = ScrollAssembler(first)
        precondition(assembler.append(first) == .unchanged)
        precondition(assembler.append(frame(60)) == .appended(240), "60-pixel scroll must align exactly")
        precondition(assembler.append(frame(130)) == .appended(310))
        precondition(PixelFrame(assembler.composed()).bytes == Array(bytes.prefix(310*width*4)), "stitched pixels must match original document")
        assembler.undo(); precondition(assembler.height == 240)
        precondition(assembler.append(frame(350)) == .uncertain, "missing overlap must not corrupt image")
        var document = CaptureDocument(image: first)
        let roundtrip = PixelFrame(document.render().cgImage!)
        precondition(roundtrip.bytes == PixelFrame(first).bytes, "export must not flip screenshot")
        document.checkpoint()
        document.marks.append(CaptureMark(kind: .rectangle, rect: CGRect(x: 20, y: 20, width: 120, height: 70)))
        document.checkpoint()
        document.marks.append(CaptureMark(kind: .text, rect: CGRect(x: 20, y: 110, width: 200, height: 50), text: "截图 Test", color: .white, fontSize: 24))
        document.undo(); precondition(document.marks.count == 1)
        document.redo(); precondition(document.marks.count == 2)
        let png = document.render().representation(using: .png, properties: [:])!
        try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
        precondition(NSBitmapImageRep(data: png)?.pixelsHigh == height)
        try captureRunUITests(image: first, output: URL(fileURLWithPath: CommandLine.arguments[1]).deletingLastPathComponent().appendingPathComponent("editor.png"))
        print("PASS: exact scroll stitching, duplicate frames, missing overlap, rollback, pixel orientation, annotation undo/redo, PNG export")
    }
}

// Draws the README banner: `swift assets/make-banner.swift assets/banner.png` (2560x800).
import AppKit

let (width, height): (CGFloat, CGFloat) = (2560, 800)
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "banner.png"
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(width), pixelsHigh: Int(height), bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat(hex >> 16 & 0xff) / 255, green: CGFloat(hex >> 8 & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
}

// Card with the icon's gradient.
let card = NSBezierPath(roundedRect: NSRect(x: 0, y: 0, width: width, height: height), xRadius: 48, yRadius: 48)
NSGradient(starting: color(0x2b313c), ending: color(0x1b1e25))!.draw(in: card, angle: -90)
card.addClip()

// Faint code down the right-hand side, highlighted in the editor's default colours.
let codeFont = NSFont.monospacedSystemFont(ofSize: 30, weight: .regular)
let code: [[(String, UInt32)]] = [
    [("fn ", 0xb48ead), ("main", 0x8fa1b3), ("() {", 0xc0c5ce)],
    [("    let ", 0xb48ead), ("doc", 0xc0c5ce), (" = ", 0xc0c5ce), ("Document", 0xebcb8b), ("::open(path)?;", 0xc0c5ce)],
    [("    // opens instantly, edits text well", 0x65737e)],
    [("    ", 0xc0c5ce), ("editor", 0xbf616a), (".", 0xc0c5ce), ("insert_text", 0x8fa1b3), ("(", 0xc0c5ce), ("\"hello\"", 0xa3be8c), (");", 0xc0c5ce)],
    [("    ", 0xc0c5ce), ("editor", 0xbf616a), (".", 0xc0c5ce), ("add_next_occurrence", 0x8fa1b3), ("();", 0xc0c5ce)],
    [("    ", 0xc0c5ce), ("doc", 0xc0c5ce), (".", 0xc0c5ce), ("save", 0x8fa1b3), ("()?;", 0xc0c5ce)],
    [("    ", 0xc0c5ce), ("Ok", 0xebcb8b), ("(())", 0xc0c5ce)],
    [("}", 0xc0c5ce)],
]
for (i, line) in code.enumerated() {
    let s = NSMutableAttributedString()
    for (text, hex) in line {
        s.append(NSAttributedString(string: text, attributes: [.font: codeFont, .foregroundColor: color(hex, 0.32)]))
    }
    s.draw(at: NSPoint(x: 1540, y: height - 190 - CGFloat(i) * 56))
}
// Wordmark: "oxide" and a rust-orange caret, as in the icon.
let font = NSFont.monospacedSystemFont(ofSize: 230, weight: .semibold)
let word = NSAttributedString(string: "oxide", attributes: [.font: font, .foregroundColor: color(0xeef0f4)])
let left: CGFloat = 170, baseline: CGFloat = 400
word.draw(at: NSPoint(x: left, y: baseline + font.descender))
let caretX = left + word.size().width + 22
color(0xe8743b).setFill()
NSBezierPath(roundedRect: NSRect(x: caretX, y: baseline + font.descender * 0.55, width: 30,
                                 height: font.capHeight * 1.08 - font.descender * 0.55),
             xRadius: 7, yRadius: 7).fill()

let tagline = NSAttributedString(string: "A super light text editor, built in Rust.", attributes: [
    .font: NSFont.systemFont(ofSize: 64, weight: .medium), .foregroundColor: color(0xaab2bf),
])
tagline.draw(at: NSPoint(x: left + 8, y: baseline - 190))
let platforms = NSAttributedString(string: "macOS  ·  Windows  ·  Linux", attributes: [
    .font: NSFont.systemFont(ofSize: 40, weight: .regular), .foregroundColor: color(0x6c7584),
])
platforms.draw(at: NSPoint(x: left + 10, y: baseline - 280))

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))

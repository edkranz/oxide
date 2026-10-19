// Draws the app icon: `swift make-icon.swift out.png` (1024x1024).
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat(hex >> 16 & 0xff) / 255, green: CGFloat(hex >> 8 & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
}

// macOS icon grid: an 824pt rounded square centred in 1024, with a soft shadow.
let body = NSRect(x: 100, y: 100, width: 824, height: 824)
let shape = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
NSGraphicsContext.saveGraphicsState()
let shadow = NSShadow()
shadow.shadowColor = color(0x000000, 0.35)
shadow.shadowBlurRadius = 24
shadow.shadowOffset = NSSize(width: 0, height: -12)
shadow.set()
color(0x2b303b).setFill()
shape.fill()
NSGraphicsContext.restoreGraphicsState()
NSGradient(starting: color(0x3a4350), ending: color(0x20242c))!.draw(in: shape, angle: -90)

// "ox" in a monospace face, followed by a rust-orange caret; the pair is centred on the caret.
let font = NSFont.monospacedSystemFont(ofSize: 340, weight: .semibold)
let text = NSAttributedString(string: "ox", attributes: [.font: font, .foregroundColor: color(0xe6e9ef)])
let textWidth = text.size().width
let caretWidth: CGFloat = 44, gap: CGFloat = 22
let caretBottom = font.descender * 0.55, caretTop = font.capHeight * 1.08
let baseline = size / 2 - (caretBottom + caretTop) / 2
let originX = (size - textWidth - gap - caretWidth) / 2
// draw(at:) positions the line box, whose baseline sits `-descender` above its origin.
text.draw(at: NSPoint(x: originX, y: baseline + font.descender))
let caret = NSRect(x: originX + textWidth + gap, y: baseline + caretBottom,
                   width: caretWidth, height: caretTop - caretBottom)
color(0xe8743b).setFill()
NSBezierPath(roundedRect: caret, xRadius: 10, yRadius: 10).fill()

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))

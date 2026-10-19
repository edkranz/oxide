// Puts a rendered oxide frame in a macOS-style window with a shadow:
// `frame IN.png TITLE OUT.png`. IN is at 2x scale.
import AppKit

let args = CommandLine.arguments
let content = NSBitmapImageRep(data: try! Data(contentsOf: URL(fileURLWithPath: args[1])))!
let title = args[2]
let (w, h) = (CGFloat(content.pixelsWide), CGFloat(content.pixelsHigh))
let scale: CGFloat = 2, bar = 28 * scale, radius = 10 * scale, margin = 40 * scale
let (outW, outH) = (w + margin * 2, h + bar + margin * 2)
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(outW), pixelsHigh: Int(outH), bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// The title bar takes the colour of the tab strip just below it, a touch darker.
let strip = content.colorAt(x: 2, y: 2)!.usingColorSpace(.sRGB)!
let barColor = strip.blended(withFraction: 0.12, of: .black)!
let light = strip.brightnessComponent > 0.6

let window = NSRect(x: margin, y: margin, width: w, height: h + bar)
let shape = NSBezierPath(roundedRect: window, xRadius: radius, yRadius: radius)
NSGraphicsContext.saveGraphicsState()
let shadow = NSShadow()
shadow.shadowColor = NSColor(white: 0, alpha: 0.45)
shadow.shadowBlurRadius = 30 * scale
shadow.shadowOffset = NSSize(width: 0, height: -10 * scale)
shadow.set()
barColor.setFill()
shape.fill()
NSGraphicsContext.restoreGraphicsState()

NSGraphicsContext.saveGraphicsState()
shape.addClip()
barColor.setFill()
NSRect(x: margin, y: margin + h, width: w, height: bar).fill()
content.draw(in: NSRect(x: margin, y: margin, width: w, height: h))
NSGraphicsContext.restoreGraphicsState()
NSColor(white: light ? 0 : 1, alpha: 0.12).setStroke()
let edge = NSBezierPath(roundedRect: window.insetBy(dx: 0.5, dy: 0.5), xRadius: radius, yRadius: radius)
edge.lineWidth = 1
edge.stroke()

// Traffic lights.
for (i, hex) in [0xff5f57, 0xfebc2e, 0x28c840].enumerated() {
    let d = 12 * scale
    let x = margin + 20 * scale + CGFloat(i) * 20 * scale - d / 2
    let y = margin + h + (bar - d) / 2
    NSColor(srgbRed: CGFloat(hex >> 16 & 0xff) / 255, green: CGFloat(hex >> 8 & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: 1).setFill()
    NSBezierPath(ovalIn: NSRect(x: x, y: y, width: d, height: d)).fill()
}
let text = NSAttributedString(string: title, attributes: [
    .font: NSFont.systemFont(ofSize: 13 * scale, weight: .semibold),
    .foregroundColor: NSColor(white: light ? 0.3 : 0.75, alpha: 1),
])
let size = text.size()
text.draw(at: NSPoint(x: margin + (w - size.width) / 2, y: margin + h + (bar - size.height) / 2))

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[3]))

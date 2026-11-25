// Renders the Android launcher icons from the desktop artwork:
//   swift scripts/android-icons.swift src-tauri/icons/app-icon-windows.png \
//     src-tauri/icons/app-icon.png src-tauri/gen/android/app/src/main/res
// (adaptive foreground = the bare prism; legacy icons = the macOS tile).
import Foundation
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

let args = CommandLine.arguments
let prism = load(args[1]), tile = load(args[2]), res = args[3]

func load(_ p: String) -> CGImage {
    let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: p) as CFURL, nil)!
    return CGImageSourceCreateImageAtIndex(src, 0, nil)!
}
func save(_ img: CGImage, _ p: String) {
    let d = CGImageDestinationCreateWithURL(URL(fileURLWithPath: p) as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(d, img, nil)
    CGImageDestinationFinalize(d)
}
func canvas(_ n: Int, _ draw: (CGContext) -> Void) -> CGImage {
    let c = CGContext(data: nil, width: n, height: n, bitsPerComponent: 8, bytesPerRow: 0,
                      space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.interpolationQuality = .high
    draw(c)
    return c.makeImage()!
}
func lavender(_ c: CGContext, _ n: CGFloat) {
    let g = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!, colors: [
        CGColor(red: 1, green: 1, blue: 1, alpha: 1), CGColor(red: 0.925, green: 0.906, blue: 0.98, alpha: 1)] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(g, start: CGPoint(x: 0, y: n), end: CGPoint(x: 0, y: 0), options: [])
}

// densities: (folder, launcher px, adaptive layer px)
for (dir, l, a) in [("mdpi", 48, 108), ("hdpi", 72, 162), ("xhdpi", 96, 216), ("xxhdpi", 144, 324), ("xxxhdpi", 192, 432)] {
    let out = "\(res)/mipmap-\(dir)"
    try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)
    // adaptive foreground: the prism inside the 66dp safe zone of a 108dp layer
    save(canvas(a) { c in
        let s = CGFloat(a) * 0.66, o = (CGFloat(a) - s) / 2
        c.draw(prism, in: CGRect(x: o, y: o, width: s, height: s))
    }, "\(out)/ic_launcher_foreground.png")
    // legacy square icon (Android 7 and older): the macOS tile, cropped to its edges
    let crop = tile.cropping(to: CGRect(x: 92, y: 88, width: 840, height: 840))!
    save(canvas(l) { c in c.draw(crop, in: CGRect(x: 0, y: 0, width: l, height: l)) }, "\(out)/ic_launcher.png")
    // legacy round icon
    save(canvas(l) { c in
        let n = CGFloat(l)
        c.addEllipse(in: CGRect(x: 0, y: 0, width: n, height: n)); c.clip()
        lavender(c, n)
        let s = n * 1.12, o = (n - s) / 2
        c.draw(prism, in: CGRect(x: o, y: o, width: s, height: s))
    }, "\(out)/ic_launcher_round.png")
}

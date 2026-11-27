import AppKit
import CoreGraphics
import ScreenCaptureKit

private let displayConfigurationChanged: CGDisplayReconfigurationCallBack = { _, flags, context in
    guard !flags.contains(.beginConfigurationFlag), let context else { return }
    Unmanaged<DisplayConfigurationWatch>.fromOpaque(context).takeUnretainedValue().invalidate()
}

final class DisplayConfigurationWatch {
    private let lock = NSLock()
    private var invalid = false
    init() throws {
        guard CGDisplayRegisterReconfigurationCallback(displayConfigurationChanged, Unmanaged.passUnretained(self).toOpaque()) == .success else {
            throw CaptureError.runtime("Could not monitor display configuration changes")
        }
    }
    deinit { CGDisplayRemoveReconfigurationCallback(displayConfigurationChanged, Unmanaged.passUnretained(self).toOpaque()) }
    func invalidate() { lock.lock(); invalid=true; lock.unlock() }
    var changed: Bool { lock.lock(); defer {lock.unlock()}; return invalid }
}

struct CaptureRegion: Codable, Equatable {
    var x: Double
    var y: Double
    var width: Double
    var height: Double
    var origin = "top-left"
    var unit = "display-logical"

    var rect: CGRect { CGRect(x: x, y: y, width: width, height: height) }
    init(_ rect: CGRect) {
        x = rect.minX; y = rect.minY; width = rect.width; height = rect.height
    }
    func validate(in bounds: CGRect) throws {
        guard origin == "top-left", unit == "display-logical",
              [x, y, width, height].allSatisfy(\.isFinite), width >= 16, height >= 16,
              x >= 0, y >= 0, x + width <= bounds.width, y + height <= bounds.height else {
            throw CaptureError.known("capture.region_invalid", "Region must be finite, at least 16 logical units, and inside the selected display", [:])
        }
    }
}

struct CaptureArea: Codable, Equatable {
    let mode: String
    let displayRef: String
    let snapshotToken: String
    let region: CaptureRegion?
}

struct PixelSize: Codable, Equatable {
    let width: Int
    let height: Int
}

struct CaptureGeometry: Codable, Equatable {
    let area: CaptureArea
    let sourceRect: CaptureRegion
    let pointPixelScale: Double
    let sourcePixels: PixelSize
    let outputPixels: PixelSize
    let paddingRight: Double
    let paddingBottom: Double
    let contentWidth: Double
    let contentHeight: Double
    let scaleFactor: Double
}

struct CaptureSourceGeometry {
    let rect: CaptureRegion
    let pixels: PixelSize
    let pointPixelScale: Double
}

// Stateless math, shared by overlay confirmation and recording configuration.
func pixelAligned(_ rect: CGRect, bounds: CGRect, scale: Double) -> CGRect {
    let x0 = max(0, min(bounds.width * scale, (rect.minX * scale).rounded()))
    let y0 = max(0, min(bounds.height * scale, (rect.minY * scale).rounded()))
    let x1 = max(x0, min(bounds.width * scale, (rect.maxX * scale).rounded()))
    let y1 = max(y0, min(bounds.height * scale, (rect.maxY * scale).rounded()))
    return CGRect(x: x0 / scale, y: y0 / scale, width: (x1-x0) / scale, height: (y1-y0) / scale)
}

func constrainRegion(_ rect: CGRect, bounds: CGRect) -> CGRect {
    let width = min(bounds.width, max(16, rect.width))
    let height = min(bounds.height, max(16, rect.height))
    return CGRect(x: min(max(0, rect.minX), bounds.width-width),
                  y: min(max(0, rect.minY), bounds.height-height), width: width, height: height)
}

func dragRegion(original: CGRect, start: CGPoint, current: CGPoint, handle: Int, bounds: CGRect, scale: Double) -> CGRect {
    // Outside drags do not create/replace a saved rectangle. Keep exact bytes
    // for no motion too; a grab offset must not snap the edge to the pointer.
    guard handle >= -1 && handle <= 7, current != start else { return original }
    let dx = current.x-start.x, dy = current.y-start.y
    var r=original
    if handle == -1 { r=original.offsetBy(dx:dx,dy:dy) }
    else {
        var x0=original.minX, x1=original.maxX, y0=original.minY, y1=original.maxY
        if [0,6,7].contains(handle) { x0=max(0,min(original.maxX-16,original.minX+dx)) }
        if [2,3,4].contains(handle) { x1=min(bounds.width,max(original.minX+16,original.maxX+dx)) }
        if [0,1,2].contains(handle) { y0=max(0,min(original.maxY-16,original.minY+dy)) }
        if [4,5,6].contains(handle) { y1=min(bounds.height,max(original.minY+16,original.maxY+dy)) }
        r=CGRect(x:x0,y:y0,width:x1-x0,height:y1-y0)
    }
    return pixelAligned(constrainRegion(r,bounds:bounds),bounds:bounds,scale:scale)
}

func keyboardRegion(original: CGRect, keyCode: UInt16, accelerated: Bool, resize: Bool, bounds: CGRect, scale: Double) -> CGRect? {
    let amount: CGFloat = accelerated ? 10 : 1
    let deltas: [UInt16:CGPoint] = [123:CGPoint(x:-amount,y:0),124:CGPoint(x:amount,y:0),125:CGPoint(x:0,y:amount),126:CGPoint(x:0,y:-amount)]
    guard let d = deltas[keyCode] else { return nil }
    var r = original
    if resize { r.size.width += d.x; r.size.height += d.y }
    else { r.origin.x += d.x; r.origin.y += d.y }
    return pixelAligned(constrainRegion(r,bounds:bounds),bounds:bounds,scale:scale)
}

@MainActor func screenForDisplay(_ id: UInt32) throws -> NSScreen {
    guard let screen = NSScreen.screens.first(where: {
        ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
    }) else { throw CaptureError.known("capture.display_missing", "Selected display is no longer available", ["id": String(id)]) }
    return screen
}

@MainActor func displaySnapshot(_ id: UInt32) throws -> String {
    let s = try screenForDisplay(id)
    let b = s.convertRectToBacking(s.frame)
    return "\(id):\(s.frame.origin.x):\(s.frame.origin.y):\(s.frame.width):\(s.frame.height):\(b.width):\(b.height):\(s.backingScaleFactor):\(CGDisplayRotation(id))"
}

func displayReference(_ id: UInt32) throws -> String {
    guard let uuid = CGDisplayCreateUUIDFromDisplayID(id)?.takeRetainedValue() else {
        throw CaptureError.known("capture.display_missing", "Display identity is unavailable", ["id":String(id)])
    }
    return "macos-sck:" + (CFUUIDCreateString(nil, uuid) as String).lowercased()
}

@MainActor func describeForUI(_ display: SCDisplay) throws -> DisplayDescription {
    let ref = try displayReference(display.displayID)
    let snapshot = try displaySnapshot(display.displayID)
    var description = DisplayDescription(id: display.displayID, widthPoints: display.width, heightPoints: display.height,
        isMain: display.displayID == CGMainDisplayID(), displayRef: ref, snapshotToken: snapshot)
    // Discover the source grid without creating a stream or predicting encoded dimensions.
    guard let bounds = try? captureBounds(display) else { return description }
    let filter = SCContentFilter(display: display, excludingWindows: [])
    let scale = Double(filter.pointPixelScale)
    guard let source = try? resolveSourceGeometry(region: CaptureRegion(bounds), bounds: bounds, scale: scale),
          (try? displaySnapshot(display.displayID)) == snapshot else { return description }
    description.pointPixelScale = source.pointPixelScale
    description.sourcePixels = source.pixels
    return description
}

@MainActor private func captureBounds(_ display: SCDisplay) throws -> CGRect {
    let bounds = CGRect(x: 0, y: 0, width: display.width, height: display.height)
    let screen = try screenForDisplay(display.displayID)
    guard screen.frame.size == bounds.size else {
        throw CaptureError.known("capture.region_invalid", "Display geometry changed while preparing capture", [:])
    }
    return bounds
}

@MainActor func resolveGeometry(display: SCDisplay, filter: SCContentFilter, area: CaptureArea?, approvedOutput: PixelSize? = nil) throws -> CaptureGeometry {
    let snapshot = try displaySnapshot(display.displayID)
    let ref = try displayReference(display.displayID)
    let bounds = try captureBounds(display)
    let selected = area ?? CaptureArea(mode: "display", displayRef: ref, snapshotToken: snapshot, region: nil)
    guard selected.displayRef == ref, selected.snapshotToken == snapshot,
          ["display", "region"].contains(selected.mode),
          (selected.mode == "region") == (selected.region != nil) else {
        throw CaptureError.known("capture.region_invalid", "Selected display or display mode changed; select the region again", [:])
    }
    let region = selected.region ?? CaptureRegion(bounds)
    let source = try resolveSourceGeometry(region: region, bounds: bounds, scale: Double(filter.pointPixelScale))
    return try resolveOutputGeometry(area: selected, source: source, approvedOutput: approvedOutput)
}

func resolveSourceGeometry(region: CaptureRegion, bounds: CGRect, scale: Double) throws -> CaptureSourceGeometry {
    try region.validate(in: bounds)
    guard scale.isFinite, scale > 0 else { throw CaptureError.runtime("Invalid ScreenCaptureKit pointPixelScale") }
    let aligned = pixelAligned(region.rect, bounds: bounds, scale: scale)
    // Overlay already shows pixel-aligned geometry. Never silently change a confirmed rectangle here.
    guard abs(aligned.minX-region.x) < 0.0001, abs(aligned.minY-region.y) < 0.0001,
          abs(aligned.width-region.width) < 0.0001, abs(aligned.height-region.height) < 0.0001 else {
        throw CaptureError.known("capture.region_invalid", "Region is not aligned to the current capture pixel grid; select it again", [:])
    }
    let w = Int((aligned.width * scale).rounded()), h = Int((aligned.height * scale).rounded())
    return CaptureSourceGeometry(rect: CaptureRegion(aligned), pixels: PixelSize(width: w, height: h), pointPixelScale: scale)
}

func resolveOutputGeometry(area: CaptureArea, source: CaptureSourceGeometry, approvedOutput: PixelSize? = nil) throws -> CaptureGeometry {
    let w = source.pixels.width, h = source.pixels.height
    let right = w % 2, bottom = h % 2
    let output = approvedOutput ?? PixelSize(width:w+right,height:h+bottom)
    guard output.width >= 16, output.height >= 16, output.width%2 == 0, output.height%2 == 0,
          approvedOutput == nil || (output.width <= w+right && output.height <= h+bottom) else {
        throw CaptureError.known("capture.region_invalid", "Invalid approved output dimensions; enlargement is not allowed", [:])
    }
    let ratio = min(1, min(Double(output.width)/Double(w),Double(output.height)/Double(h)))
    return CaptureGeometry(area: area, sourceRect: source.rect, pointPixelScale: source.pointPixelScale,
        sourcePixels: source.pixels, outputPixels: output,
        paddingRight: Double(output.width)-Double(w)*ratio, paddingBottom: Double(output.height)-Double(h)*ratio,
        contentWidth: Double(w)*ratio, contentHeight: Double(h)*ratio, scaleFactor: ratio)
}

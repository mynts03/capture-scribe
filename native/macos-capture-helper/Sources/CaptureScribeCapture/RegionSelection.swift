import AppKit
import Foundation

enum SelectionMode: String, Codable { case display, region }

// A selection session is a draft. Full-screen confirmation deliberately has no
// region payload; React keeps the previously committed rectangle unchanged.
struct SelectionDraft {
    var mode: SelectionMode
    var region: CGRect
    let bounds: CGRect
    var shownRect: CGRect { mode == .display ? bounds : region }
    func confirmedArea(displayRef: String, snapshotToken: String) -> CaptureArea {
        CaptureArea(mode:mode.rawValue,displayRef:displayRef,snapshotToken:snapshotToken,
                    region:mode == .region ? CaptureRegion(region) : nil)
    }
}

struct RegionSelectionRequest: Codable {
    let protocolVersion: Int
    let requestId: String
    let displayId: UInt32
    let displayRef: String
    let snapshotToken: String
    let initialMode: SelectionMode
    let initialRegion: CaptureRegion?
    let language: String
    var isSupported: Bool { protocolVersion == 2 && !requestId.isEmpty && ["ja","en"].contains(language) }
}

struct RegionSelectionResponse: Codable {
    let protocolVersion: Int
    let requestId: String
    let status: String
    let area: CaptureArea?
    let raw: String?
}

// Keep a small but nonzero alpha across the entire input surface. A completely
// clear hole is not a reliable WindowServer mouse target, even when NSView's
// bounds contain the point. Actual on-screen event delivery still needs a Mac test.
func drawSelectionBackdrop(_ context: CGContext, bounds: CGRect, selection: CGRect) {
    context.saveGState()
    defer { context.restoreGState() }
    context.setBlendMode(.copy) // Replace old backing pixels; do not accumulate tint while dragging.
    context.setFillColor(SelectionColors.inputTint.cgColor)
    context.fill(bounds)
    context.setBlendMode(.normal)
    let mask = CGMutablePath(); mask.addRect(bounds); mask.addRect(selection)
    context.addPath(mask)
    context.setFillColor(SelectionColors.shade.cgColor)
    context.fillPath(using: .evenOdd)
}

func selectionHandlePoints(_ r: CGRect) -> [CGPoint] {
    [CGPoint(x:r.minX,y:r.minY),CGPoint(x:r.midX,y:r.minY),CGPoint(x:r.maxX,y:r.minY),
     CGPoint(x:r.maxX,y:r.midY),CGPoint(x:r.maxX,y:r.maxY),CGPoint(x:r.midX,y:r.maxY),
     CGPoint(x:r.minX,y:r.maxY),CGPoint(x:r.minX,y:r.midY)]
}

func selectionDragHandle(at point: CGPoint, selection: CGRect) -> Int {
    // Provisional 20x20pt corners / 16pt-wide edge bands. Reduce each axis
    // for small rectangles so opposing bands never swallow the move area.
    let cx = min(10, selection.width / 4), cy = min(10, selection.height / 4)
    let ex = min(8, selection.width / 4), ey = min(8, selection.height / 4)
    let points = selectionHandlePoints(selection)
    for index in [0, 2, 4, 6] {
        if abs(point.x-points[index].x) <= cx && abs(point.y-points[index].y) <= cy { return index }
    }
    if point.x >= selection.minX && point.x <= selection.maxX {
        if abs(point.y-selection.minY) <= ey { return 1 }
        if abs(point.y-selection.maxY) <= ey { return 5 }
    }
    if point.y >= selection.minY && point.y <= selection.maxY {
        if abs(point.x-selection.minX) <= ex { return 7 }
        if abs(point.x-selection.maxX) <= ex { return 3 }
    }
    return selection.contains(point) ? -1 : -2 // Outside is inert, never a new rectangle.
}

// SelectionView is flipped: minY is the top edge. Use the same handle selected
// by hit testing for both the native cursor and the entire drag transaction.
func selectionResizePosition(for handle: Int) -> NSCursor.FrameResizePosition? {
    switch handle {
    case 0: return .topLeft
    case 1: return .top
    case 2: return .topRight
    case 3: return .right
    case 4: return .bottomRight
    case 5: return .bottom
    case 6: return .bottomLeft
    case 7: return .left
    default: return nil
    }
}

@MainActor private func selectionCursor(for handle: Int, dragging: Bool) -> NSCursor {
    if let position = selectionResizePosition(for: handle) {
        return .frameResize(position: position, directions: .all) // macOS 15+, same as this helper.
    }
    if handle == -1 { return dragging ? .closedHand : .openHand }
    return .arrow
}

// Opt-in, bounded stderr diagnostics. No global monitor, extra permission, or
// changes to the single JSON response on stdout. Only this window's events.
@MainActor final class SelectionInputTrace {
    private let enabled = ProcessInfo.processInfo.environment["CAPTURE_SCRIBE_SELECTION_DIAGNOSTICS"] == "1"
    private var remaining = 80
    func emit(_ phase: String, window: NSWindow?, event: NSEvent? = nil) {
        guard enabled, remaining > 0 else { return }; remaining -= 1
        var data: [String: Any] = ["event":"selection_input", "phase":phase,
            "keyWindow":window?.isKeyWindow ?? false,
            "ignoresMouseEvents":window?.ignoresMouseEvents ?? true]
        if let event, let root = window?.contentView {
            let point = root.superview?.convert(event.locationInWindow, from:nil) ?? event.locationInWindow
            data["hitView"] = root.hitTest(point)?.className ?? "nil"
        }
        if let bytes = try? JSONSerialization.data(withJSONObject:data,options:.sortedKeys) {
            try? FileHandle.standardError.write(contentsOf:bytes + Data([10]))
        }
    }
}

private final class SelectionWindow: NSWindow {
    var inputTrace: SelectionInputTrace?
    private var loggedDrag = false
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
    override func sendEvent(_ event: NSEvent) {
        if event.type == .keyDown, let selector=contentView as? SelectionView {
            if event.keyCode == 53 { selector.cancelSelection(); return }
            if event.keyCode == 36 || event.keyCode == 76 { selector.confirmSelection(); return }
        }
        if event.type == .leftMouseDown {
            loggedDrag = false; inputTrace?.emit("window_mouse_down",window:self,event:event)
        } else if event.type == .leftMouseDragged && !loggedDrag {
            loggedDrag = true; inputTrace?.emit("window_first_drag",window:self,event:event)
        }
        super.sendEvent(event) // Normal AppKit dispatch, never forward to another app.
    }
}

// Geometry is independent of AppKit windows. No auto-reposition while editing.
func clampedControlBar(_ frame: CGRect, in usable: CGRect) -> CGRect {
    let width=min(frame.width,usable.width), height=min(frame.height,usable.height)
    return CGRect(x:max(usable.minX,min(frame.minX,usable.maxX-width)),
                  y:max(usable.minY,min(frame.minY,usable.maxY-height)),width:width,height:height)
}
func initialControlBar(size: CGSize, usable: CGRect, selection: CGRect) -> CGRect {
    let safe=usable.insetBy(dx:12,dy:12)
    let x=safe.midX-size.width/2
    let candidates=[safe.maxY-size.height-44, selection.maxY+16, selection.minY-size.height-16, safe.minY+24]
        .map { clampedControlBar(CGRect(origin:CGPoint(x:x,y:$0),size:size),in:safe) }
    return candidates.enumerated().min { a,b in
        let ia=a.element.intersection(selection), ib=b.element.intersection(selection)
        let sa=ia.isNull ? 0 : ia.width*ia.height, sb=ib.isNull ? 0 : ib.width*ib.height
        return sa == sb ? a.offset < b.offset : sa < sb
    }!.element
}
func draggedControlBar(original: CGRect, start: CGPoint, current: CGPoint, usable: CGRect) -> CGRect {
    clampedControlBar(original.offsetBy(dx:current.x-start.x,dy:current.y-start.y),in:usable)
}

struct SelectionControlLayout {
    let cancelWidth: CGFloat
    let confirmWidth: CGFloat
    let vertical: Bool
    let contentWidth: CGFloat
    let buttonHeight: CGFloat = 34
    let padding: CGFloat = 10
    let spacing: CGFloat = 8
    let gripWidth: CGFloat = 20
    var width: CGFloat { contentWidth + gripWidth + spacing + 2 * padding }
    var height: CGFloat { (vertical ? 2 * buttonHeight + spacing : buttonHeight) + 28 + spacing + 2 * padding }
}

func selectionControlLayout(cancel: String, confirm: String, availableWidth: CGFloat) -> SelectionControlLayout {
    let font = NSFont.systemFont(ofSize: 14, weight: .semibold)
    let cancelWidth = ceil(max(100, (cancel as NSString).size(withAttributes: [.font: font]).width + 24))
    let confirmWidth = ceil(max(136, (confirm as NSString).size(withAttributes: [.font: font]).width + 24))
    let limit = max(16, availableWidth - 24 - 48)
    let vertical = cancelWidth + confirmWidth + 8 > limit
    let contentWidth=min(limit,max(220,vertical ? max(cancelWidth,confirmWidth) : cancelWidth+confirmWidth+8))
    return SelectionControlLayout(cancelWidth:min(cancelWidth,contentWidth),confirmWidth:min(confirmWidth,contentWidth),vertical:vertical,contentWidth:contentWidth)
}

// Only the explicit grip moves the panel. Buttons/segmented controls keep their
// standard AppKit hit testing, tracking, focus and accessibility semantics.
@MainActor private final class ControlBarGrip: NSView {
    var usable=CGRect.zero
    private var start=CGPoint.zero
    private var original=CGRect.zero
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event:NSEvent?) -> Bool { true }
    override func resetCursorRects() { addCursorRect(bounds,cursor:.openHand) }
    override func draw(_ dirtyRect:NSRect) {
        SelectionColors.grip.setFill()
        for x in [bounds.midX-3,bounds.midX+3] { for dy in [-6.0,0,6] {
            NSBezierPath(ovalIn:CGRect(x:x-1,y:bounds.midY+dy-1,width:2,height:2)).fill()
        } }
    }
    override func mouseDown(with event:NSEvent) {
        guard let bar=superview,let parent=bar.superview else { return }
        window?.makeFirstResponder(self)
        start=parent.convert(event.locationInWindow,from:nil);original=bar.frame;NSCursor.closedHand.set()
    }
    override func mouseDragged(with event:NSEvent) {
        guard let bar=superview,let parent=bar.superview else { return }
        bar.frame=draggedControlBar(original:original,start:start,current:parent.convert(event.locationInWindow,from:nil),usable:usable)
    }
    override func mouseUp(with event:NSEvent) { NSCursor.openHand.set() }
    override func keyDown(with event:NSEvent) {
        guard let bar=superview else { return }
        let delta:[UInt16:CGPoint]=[123:CGPoint(x:-8,y:0),124:CGPoint(x:8,y:0),125:CGPoint(x:0,y:8),126:CGPoint(x:0,y:-8)]
        if let d=delta[event.keyCode] { bar.frame=clampedControlBar(bar.frame.offsetBy(dx:d.x,dy:d.y),in:usable) }
        else { super.keyDown(with:event) }
    }
}

@MainActor final class SelectionView: NSView {
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    var inputTrace: SelectionInputTrace?
    private var loggedDrag = false
    var draft: SelectionDraft
    var selection: CGRect { get { draft.region } set { draft.region = newValue } }
    let scale: Double
    private var dragStart = CGPoint.zero
    private var original = CGRect.zero
    private var dragHandle = -2 // -2 inert outside, -1 move, 0...7 resize
    private var dragging = false
    private var pointerTracking: NSTrackingArea?
    private var controlBar: NSView?
    private var barGrip: NSView?
    var finish: ((Bool) -> Void)?

    init(frame: CGRect, initial: CGRect, mode: SelectionMode, scale: Double, language: String, usable: CGRect) {
        self.draft = SelectionDraft(mode:mode,region:initial,bounds:CGRect(origin:.zero,size:frame.size)); self.scale = scale
        super.init(frame: frame)
        setAccessibilityLabel(RegionLocalization.text(.accessibility, language: language))
        let cancel = NSButton(title: RegionLocalization.text(.cancel, language: language), target: self, action: #selector(cancelSelection))
        let confirm = NSButton(title: RegionLocalization.text(.confirm, language: language), target: self, action: #selector(confirmSelection))
        cancel.keyEquivalent = "\u{1b}"; cancel.keyEquivalentModifierMask = []
        confirm.keyEquivalent = "\r"; confirm.keyEquivalentModifierMask = []
        let metrics = selectionControlLayout(cancel: cancel.title, confirm: confirm.title, availableWidth: frame.width)
        for button in [cancel, confirm] {
            button.bezelStyle = .rounded
            button.controlSize = .regular
            button.font = NSFont.systemFont(ofSize: 14, weight: .semibold)
            button.contentTintColor = SelectionColors.text
            button.focusRingType = .exterior
            button.cell?.wraps = true
            button.cell?.lineBreakMode = .byWordWrapping
            button.translatesAutoresizingMaskIntoConstraints = false
            button.heightAnchor.constraint(equalToConstant: metrics.buttonHeight).isActive = true
        }
        cancel.bezelColor = SelectionColors.secondary
        confirm.bezelColor = SelectionColors.primary
        confirm.contentTintColor = SelectionColors.primaryText
        cancel.widthAnchor.constraint(equalToConstant: metrics.cancelWidth).isActive = true
        confirm.widthAnchor.constraint(equalToConstant: metrics.confirmWidth).isActive = true
        let stack = NSStackView(views: [cancel, confirm]); stack.spacing = metrics.spacing
        stack.orientation = metrics.vertical ? .vertical : .horizontal
        stack.alignment = .centerY
        if metrics.vertical { stack.alignment = .centerX }
        let modes = NSSegmentedControl(labels:[RegionLocalization.text(.fullScreen,language:language),RegionLocalization.text(.region,language:language)],trackingMode:.selectOne,target:self,action:#selector(changeMode(_:)))
        modes.selectedSegment = mode == .display ? 0 : 1
        modes.controlSize = .regular
        modes.font = NSFont.systemFont(ofSize:13,weight:.medium)
        modes.translatesAutoresizingMaskIntoConstraints = false
        modes.widthAnchor.constraint(equalToConstant:metrics.contentWidth).isActive = true
        modes.heightAnchor.constraint(equalToConstant:28).isActive = true
        modes.setAccessibilityLabel(RegionLocalization.text(.modeLabel,language:language))
        let contents = NSStackView(views:[modes,stack]); contents.orientation = .vertical; contents.spacing = metrics.spacing
        let panel = NSView()
        panel.wantsLayer = true
        panel.layer?.backgroundColor = SelectionColors.panel.cgColor
        panel.layer?.cornerRadius = 10
        panel.layer?.borderWidth = 1
        panel.layer?.borderColor = SelectionColors.border.cgColor
        panel.appearance = NSAppearance(named: .darkAqua)
        controlBar = panel
        panel.frame=initialControlBar(size:CGSize(width:metrics.width,height:metrics.height),usable:usable,selection:draft.shownRect)
        let grip=ControlBarGrip(frame:CGRect(x:metrics.padding,y:metrics.padding,width:metrics.gripWidth,height:metrics.height-2*metrics.padding))
        grip.usable=usable.insetBy(dx:8,dy:8)
        grip.setAccessibilityElement(true);grip.setAccessibilityRole(.group)
        grip.setAccessibilityLabel(RegionLocalization.text(.dragBar,language:language));barGrip=grip
        contents.translatesAutoresizingMaskIntoConstraints = false
        addSubview(panel); panel.addSubview(contents);panel.addSubview(grip)
        NSLayoutConstraint.activate([
            contents.leadingAnchor.constraint(equalTo:panel.leadingAnchor,constant:metrics.padding+metrics.gripWidth+metrics.spacing),
            contents.centerYAnchor.constraint(equalTo: panel.centerYAnchor),
        ])
    }
    @objc private func changeMode(_ sender: NSSegmentedControl) {
        draft.mode = sender.selectedSegment == 0 ? .display : .region
        dragging = false; needsDisplay = true
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }
    @objc func cancelSelection() { NSCursor.arrow.set(); finish?(false) }
    @objc func confirmSelection() { NSCursor.arrow.set(); finish?(true) }
    // Controls win over an overlapping border; they still receive normal
    // NSButton dispatch. No toolbar motion, fade, or click-through is added.
    private func interactionHandle(at point: CGPoint) -> Int {
        if controlBar?.frame.contains(point) == true { return -2 }
        if draft.mode == .display { return -2 }
        return selectionDragHandle(at:point,selection:selection)
    }
    private func updatePointer(at point: CGPoint) {
        if let grip=barGrip,grip.convert(grip.bounds,to:self).contains(point), !dragging { NSCursor.openHand.set(); return }
        selectionCursor(for:dragging ? dragHandle : interactionHandle(at:point),dragging:dragging).set()
    }
    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let pointerTracking { removeTrackingArea(pointerTracking) }
        let area = NSTrackingArea(rect:.zero,options:[.inVisibleRect,.activeInKeyWindow,.mouseMoved,.mouseEnteredAndExited,.cursorUpdate],owner:self,userInfo:nil)
        addTrackingArea(area); pointerTracking = area
    }
    override func cursorUpdate(with event: NSEvent) { updatePointer(at:convert(event.locationInWindow,from:nil)) }
    override func mouseMoved(with event: NSEvent) { updatePointer(at:convert(event.locationInWindow,from:nil)) }
    override func mouseEntered(with event: NSEvent) { updatePointer(at:convert(event.locationInWindow,from:nil)) }
    override func mouseExited(with event: NSEvent) { if !dragging { NSCursor.arrow.set() } }
    private var handles: [CGPoint] {
        selectionHandlePoints(selection)
    }
    override func draw(_ dirtyRect: NSRect) {
        if let context = NSGraphicsContext.current?.cgContext {
            drawSelectionBackdrop(context,bounds:bounds,selection:draft.shownRect)
        }
        SelectionColors.selection.setStroke(); let border = NSBezierPath(rect: draft.shownRect); border.lineWidth = 2; border.stroke()
        SelectionColors.text.setFill()
        if draft.mode == .region { for p in handles { NSBezierPath(roundedRect: CGRect(x:p.x-4,y:p.y-4,width:8,height:8),xRadius:2,yRadius:2).fill() } }
        let shown = draft.shownRect
        let label = String(format:"%g × %g pt",shown.width,shown.height)
        label.draw(at: CGPoint(x: max(8,shown.minX), y: max(8,shown.minY-26)), withAttributes: [.font:NSFont.monospacedSystemFont(ofSize:14,weight:.medium),.foregroundColor:SelectionColors.text,.backgroundColor:SelectionColors.labelBackground])
    }
    override func mouseDown(with event: NSEvent) {
        loggedDrag = false; inputTrace?.emit("view_mouse_down",window:window,event:event)
        window?.makeFirstResponder(self)
        let p = convert(event.locationInWindow, from: nil)
        dragStart = p; original = selection
        dragHandle = interactionHandle(at:p)
        dragging = dragHandle >= -1
        updatePointer(at:p)
    }
    override func mouseDragged(with event: NSEvent) {
        if !loggedDrag { loggedDrag = true; inputTrace?.emit("view_first_drag",window:window,event:event) }
        guard dragging else { return }
        selection = dragRegion(original:original,start:dragStart,current:convert(event.locationInWindow,from:nil),handle:dragHandle,bounds:bounds,scale:scale)
        updatePointer(at:convert(event.locationInWindow,from:nil))
        needsDisplay = true
    }
    override func mouseUp(with event: NSEvent) {
        dragging = false
        updatePointer(at:convert(event.locationInWindow,from:nil))
    }
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 { cancelSelection(); return }
        if event.keyCode == 36 || event.keyCode == 76 { confirmSelection(); return }
        guard draft.mode == .region else { super.keyDown(with:event); return }
        if let changed = keyboardRegion(original:selection,keyCode:event.keyCode,
            accelerated:event.modifierFlags.contains(.shift),resize:event.modifierFlags.contains(.option),bounds:bounds,scale:scale) {
            selection = changed; needsDisplay = true
            if let window { updatePointer(at:convert(window.mouseLocationOutsideOfEventStream,from:nil)) }
        } else { super.keyDown(with:event) }
    }
}

@MainActor func selectRegion() throws {
    // One bounded JSON line, without stdio read-ahead consuming a subsequent cancel.
    var input = Data()
    while let byte = try FileHandle.standardInput.read(upToCount: 1), !byte.isEmpty {
        if byte[0] == 10 { break }
        input.append(byte)
        guard input.count <= 65536 else { throw CaptureError.runtime("Selection request too large") }
    }
    let request = try JSONDecoder().decode(RegionSelectionRequest.self, from: input)
    var response = RegionSelectionResponse(protocolVersion: 2,requestId:request.requestId,status:"failed",area:nil,raw:"Selection did not complete")
    let app = NSApplication.shared
    var window: NSWindow?
    var done = false
    func complete(_ status: String, _ area: CaptureArea? = nil, _ raw: String? = nil) {
        guard !done else { return }; done = true
        response = RegionSelectionResponse(protocolVersion:2,requestId:request.requestId,status:status,area:area,raw:raw)
        NSCursor.arrow.set()
        window?.orderOut(nil); window?.close()
        app.stop(nil)
        if let e = NSEvent.otherEvent(with:.applicationDefined,location:.zero,modifierFlags:[],timestamp:0,windowNumber:0,context:nil,subtype:0,data1:0,data2:0) { app.postEvent(e,atStart:false) }
    }
    do {
        guard request.isSupported,
              request.displayRef == (try displayReference(request.displayId)), try displaySnapshot(request.displayId) == request.snapshotToken else {
            throw CaptureError.runtime("Display snapshot or selection protocol changed")
        }
        let screen = try screenForDisplay(request.displayId)
        let bounds = CGRect(origin:.zero,size:screen.frame.size)
        let scale = screen.backingScaleFactor
        let initial: CGRect
        if let region=request.initialRegion { try region.validate(in:bounds); initial=region.rect }
        else { initial=CGRect(x:bounds.width*0.15,y:bounds.height*0.15,width:bounds.width*0.7,height:bounds.height*0.7) }
        app.setActivationPolicy(.accessory)
        let w = SelectionWindow(contentRect:screen.frame,styleMask:.borderless,backing:.buffered,defer:false)
        w.isReleasedWhenClosed=false; w.animationBehavior = .none; w.level = .screenSaver
        w.backgroundColor = .clear; w.isOpaque=false; w.hasShadow=false
        w.ignoresMouseEvents = false
        w.acceptsMouseMovedEvents = true
        w.collectionBehavior=[.canJoinAllSpaces,.fullScreenAuxiliary]
        let visible=screen.visibleFrame
        let usable=CGRect(x:visible.minX-screen.frame.minX,y:screen.frame.maxY-visible.maxY,width:visible.width,height:visible.height).intersection(bounds)
        let view = SelectionView(frame:bounds,initial:pixelAligned(initial,bounds:bounds,scale:scale),mode:request.initialMode,scale:scale,language:request.language,usable:usable)
        let inputTrace = SelectionInputTrace(); w.inputTrace = inputTrace; view.inputTrace = inputTrace
        w.contentView=view; window=w
        view.finish = { confirmed in
            guard (try? displaySnapshot(request.displayId)) == request.snapshotToken else { complete("invalidated",nil,"Display changed during selection"); return }
            if confirmed { complete("confirmed",view.draft.confirmedArea(displayRef:request.displayRef,snapshotToken:request.snapshotToken)) } else { complete("cancelled") }
        }
        let observer = NotificationCenter.default.addObserver(forName:NSApplication.didChangeScreenParametersNotification,object:nil,queue:.main) { _ in
            MainActor.assumeIsolated {
                if (try? displaySnapshot(request.displayId)) != request.snapshotToken { complete("invalidated",nil,"Display changed during selection") }
            }
        }
        defer { NotificationCenter.default.removeObserver(observer); FileHandle.standardInput.readabilityHandler=nil }
        FileHandle.standardInput.readabilityHandler = { handle in
            _ = handle.availableData
            DispatchQueue.main.async { complete("cancelled",nil,"Parent cancelled or disconnected") }
        }
        let timer = Timer(timeInterval:600,repeats:false) { _ in MainActor.assumeIsolated { complete("failed",nil,"Selection timed out") } }
        RunLoop.main.add(timer,forMode:.common); defer { timer.invalidate() }
        w.makeKeyAndOrderFront(nil); app.activate(ignoringOtherApps:true); w.makeFirstResponder(view)
        inputTrace.emit("ready",window:w)
        app.run()
    } catch { response = RegionSelectionResponse(protocolVersion:2,requestId:request.requestId,status:"invalidated",area:nil,raw:String(describing:error)) }
    let data=try JSONEncoder().encode(response)
    print(String(decoding:data,as:UTF8.self)); fflush(stdout)
}

import AppKit

// UIColors is generated from the same static source imported by React.
// NSCursor, native focus rings and segmented-control system semantics stay OS-owned.
enum SelectionColors {
    static func color(_ hex: String) -> NSColor {
        // The build tool rejects malformed input before emitting Swift source.
        let digits=String(hex.dropFirst()),value=UInt64(digits,radix:16)!
        let rgba=digits.count == 6 ? (value << 8) | 255 : value
        return NSColor(srgbRed:CGFloat((rgba >> 24) & 255)/255,
            green:CGFloat((rgba >> 16) & 255)/255,blue:CGFloat((rgba >> 8) & 255)/255,alpha:CGFloat(rgba & 255)/255)
    }
    static var panel: NSColor { color(UIColors.card) }
    static var border: NSColor { color(UIColors.secondaryBorder) }
    static var secondary: NSColor { color(UIColors.secondary) }
    static var text: NSColor { color(UIColors.textStrong) }
    static var primary: NSColor { color(UIColors.primary) }
    static var primaryText: NSColor { color(UIColors.primaryText) }
    static var selection: NSColor { color(UIColors.accent) }
    static var inputTint: NSColor { color(UIColors.selectionInput) }
    static var shade: NSColor { color(UIColors.selectionShade) }
    static var labelBackground: NSColor { color(UIColors.selectionLabel) }
    static var grip: NSColor { color(UIColors.muted) }
}

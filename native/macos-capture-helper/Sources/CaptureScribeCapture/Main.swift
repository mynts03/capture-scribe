import CoreGraphics
import Foundation
import ScreenCaptureKit

@main
struct CaptureScribeCaptureMain {
    static func main() async {
        let log = EventLog()
        do {
            switch try RecordOptions.parse(Array(CommandLine.arguments.dropFirst())) {
            case .help:
                print(usage)
            case .selectRegion:
                try selectRegion()
            case .list:
                try requestScreenCapturePermission(log: log)
                let content = try await SCShareableContent.current
                let displays = try await MainActor.run { try content.displays.map(describeForUI) }
                let encoder = JSONEncoder()
                encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
                print(String(decoding: try encoder.encode(displays), as: UTF8.self))
            case .record(let options):
                try requestScreenCapturePermission(log: log)
                let content = try await SCShareableContent.current
                let requestedID = options.displayID ?? CGMainDisplayID()
                guard let display = content.displays.first(where: { $0.displayID == requestedID }) else {
                    throw CaptureError.known("capture.display_missing", "display not found: \(requestedID)", ["id": String(requestedID)])
                }
                try await Recorder(options: options, log: log).run(display: display)
            }
        } catch {
            let message = (error as? CaptureError)?.messagePayload ?? ["raw": String(describing: error)]
            log.emit("capture_error", fields: ["message": message])
            fputs("capture-scribe-capture: \(error)\n", stderr)
            exit(1)
        }
    }
}

private func requestScreenCapturePermission(log: EventLog) throws {
    if CGPreflightScreenCaptureAccess() { return }
    if CommandLine.arguments.contains("--no-permission-prompt") {
        throw CaptureError.known("capture.permission_denied", "Screen recording access is not available to this launch path; no permission prompt was requested", [:])
    }
    log.emit("screen_capture_permission_requested")
    guard CGRequestScreenCaptureAccess() else {
        throw CaptureError.known(
            "capture.permission_denied",
            "Screen & System Audio Recording permission is not enabled. Enable it in System Settings > Privacy & Security, then retry.",
            [:]
        )
    }
    log.emit("screen_capture_permission_granted")
}

import Foundation

enum CommandOptions {
    case list
    case selectRegion
    case record(RecordOptions)
    case help
}

struct RecordOptions: Codable {
    let outputURL: URL
    let reportURL: URL
    let durationSeconds: Double
    let displayID: UInt32?
    let width: Int
    let height: Int
    let fps: Int
    let capturesSystemAudio: Bool
    let capturesMicrophone: Bool
    let area: CaptureArea?
    let approvedOutput: PixelSize?

    static func parse(_ arguments: [String]) throws -> CommandOptions {
        guard let first = arguments.first else { return .help }
        if first == "list" { return .list }
        if first == "select-region" { return .selectRegion }
        if first == "help" || first == "--help" || first == "-h" { return .help }
        guard first == "record" else { throw CaptureError.usage("unknown command: \(first)") }

        var values: [String: String] = [:]
        var flags: Set<String> = []
        var index = 1
        while index < arguments.count {
            let argument = arguments[index]
            guard argument.hasPrefix("--") else { throw CaptureError.usage("unexpected argument: \(argument)") }
            if ["--system-audio", "--no-system-audio", "--microphone", "--no-microphone", "--no-permission-prompt"].contains(argument) {
                flags.insert(argument)
                index += 1
                continue
            }
            guard index + 1 < arguments.count else { throw CaptureError.usage("missing value after \(argument)") }
            values[argument] = arguments[index + 1]
            index += 2
        }

        guard let outputPath = values["--output"] else { throw CaptureError.usage("--output is required") }
        let outputURL = URL(fileURLWithPath: outputPath).standardizedFileURL
        let reportURL = URL(fileURLWithPath: values["--report"] ?? outputURL.path + ".run.json").standardizedFileURL
        let duration = try positiveDouble(values["--duration"] ?? "7200", name: "--duration")
        let width = try values["--width"].map { try positiveInt($0, name: "--width") } ?? 0
        let height = try values["--height"].map { try positiveInt($0, name: "--height") } ?? 0
        // Legacy explicit dimensions cannot silently override the new native-size policy.
        guard width == 0 && height == 0 else { throw CaptureError.usage("Explicit output dimensions require an approved capture plan") }
        let area = try values["--area"].map { try JSONDecoder().decode(CaptureArea.self, from: Data($0.utf8)) }
        let approvedOutput = try values["--approved-output"].map { try JSONDecoder().decode(PixelSize.self, from: Data($0.utf8)) }
        let fps = try positiveInt(values["--fps"] ?? "30", name: "--fps")
        let displayID: UInt32?
        if let text = values["--display-id"] {
            guard let parsed = UInt32(text) else { throw CaptureError.usage("invalid --display-id: \(text)") }
            displayID = parsed
        } else {
            displayID = nil
        }
        return .record(RecordOptions(
            outputURL: outputURL,
            reportURL: reportURL,
            durationSeconds: duration,
            displayID: displayID,
            width: width,
            height: height,
            fps: fps,
            capturesSystemAudio: !flags.contains("--no-system-audio"),
            capturesMicrophone: flags.contains("--microphone") && !flags.contains("--no-microphone"),
            area: area,
            approvedOutput: approvedOutput
        ))
    }

    private static func positiveInt(_ text: String, name: String) throws -> Int {
        guard let value = Int(text), value > 0 else { throw CaptureError.usage("\(name) must be positive") }
        return value
    }

    private static func positiveDouble(_ text: String, name: String) throws -> Double {
        guard let value = Double(text), value > 0 else { throw CaptureError.usage("\(name) must be positive") }
        return value
    }
}

enum CaptureError: Error, CustomStringConvertible {
    case usage(String)
    case runtime(String)
    case known(String, String, [String: String])

    var description: String {
        switch self {
        case .usage(let message), .runtime(let message): message
        case .known(_, let message, _): message
        }
    }

    var messagePayload: [String: Any] {
        if case .known(let code, let raw, let params) = self {
            return ["code": code, "params": params, "raw": raw]
        }
        return ["raw": description]
    }
}

let usage = """
Usage:
  capture-scribe-capture list
  capture-scribe-capture record --output PATH [options]

The record command writes JSON events to stdout and accepts `stop` on stdin.
If stdin closes or --duration expires, the helper finalizes the recording.
"""

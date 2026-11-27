import CoreMedia
import Foundation

func monotonicMilliseconds() -> Double {
    Double(DispatchTime.now().uptimeNanoseconds) / 1_000_000.0
}

final class EventLog {
    private let lock = NSLock()

    func emit(_ event: String, fields: [String: Any] = [:]) {
        var object = fields
        object["event"] = event
        object["monotonicMs"] = monotonicMilliseconds()
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              let line = String(data: data, encoding: .utf8) else { return }
        lock.lock()
        print(line)
        fflush(stdout)
        lock.unlock()
    }
}

struct SampleMetrics: Codable {
    var sampleCount = 0
    var invalidSampleCount = 0
    var firstPTSSeconds: Double?
    var lastPTSSeconds: Double?
    var largestGapSeconds: Double = 0

    mutating func observe(_ sampleBuffer: CMSampleBuffer) {
        sampleCount += 1
        guard CMSampleBufferIsValid(sampleBuffer), CMSampleBufferDataIsReady(sampleBuffer) else {
            invalidSampleCount += 1
            return
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer).seconds
        guard pts.isFinite else {
            invalidSampleCount += 1
            return
        }
        if firstPTSSeconds == nil { firstPTSSeconds = pts }
        if let previous = lastPTSSeconds { largestGapSeconds = max(largestGapSeconds, pts - previous) }
        lastPTSSeconds = pts
    }
}

struct DisplayDescription: Codable {
    let id: UInt32
    let widthPoints: Int
    let heightPoints: Int
    let isMain: Bool
    var displayRef: String? = nil
    var snapshotToken: String? = nil
    var pointPixelScale: Double? = nil
    var sourcePixels: PixelSize? = nil
}

struct RunReport: Codable {
    let schemaVersion: Int
    let status: String
    let stopReason: String
    let startedAt: String
    let finishedAt: String
    let options: RecordOptions
    let selectedDisplay: DisplayDescription
    let finalizeMilliseconds: Double
    let outputSizeBytes: Int64?
    let recordingDurationSeconds: Double?
    let systemAudioMetrics: SampleMetrics?
    let microphoneMetrics: SampleMetrics?
    let captureGeometry: CaptureGeometry
}

func writeReport(_ report: RunReport, to url: URL) throws {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
    let data = try encoder.encode(report)
    try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    try data.write(to: url, options: .atomic)
}

extension CMTime {
    var finiteSeconds: Double? {
        seconds.isFinite ? seconds : nil
    }
}

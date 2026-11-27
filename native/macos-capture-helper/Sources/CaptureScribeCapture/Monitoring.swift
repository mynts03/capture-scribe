import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

struct TimelineAnchor {
    let samplePTSSeconds: Double
    let displayTimeSeconds: Double
    let pixelWidth: Int
    let pixelHeight: Int
}

final class TimelineAnchorMonitor: NSObject, SCStreamOutput {
    private let condition = NSCondition()
    private var anchor: TimelineAnchor?

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of outputType: SCStreamOutputType) {
        guard outputType == .screen,
              CMSampleBufferIsValid(sampleBuffer), CMSampleBufferDataIsReady(sampleBuffer),
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let first = attachments.first,
              let statusRaw = (first[.status] as? NSNumber)?.intValue,
              SCFrameStatus(rawValue: statusRaw) == .complete,
              let displayTimeRaw = (first[.displayTime] as? NSNumber)?.uint64Value else { return }
        let samplePTS = CMSampleBufferGetPresentationTimeStamp(sampleBuffer).seconds
        let displayTime = CMClockMakeHostTimeFromSystemUnits(displayTimeRaw).seconds
        guard samplePTS.isFinite, displayTime.isFinite else { return }
        condition.lock()
        if anchor == nil, let image = CMSampleBufferGetImageBuffer(sampleBuffer) {
            anchor = TimelineAnchor(samplePTSSeconds: samplePTS, displayTimeSeconds: displayTime,
                pixelWidth: CVPixelBufferGetWidth(image), pixelHeight: CVPixelBufferGetHeight(image))
            condition.broadcast()
        }
        condition.unlock()
    }

    func waitUntilAvailable(timeout: TimeInterval) throws -> TimelineAnchor {
        condition.lock()
        defer { condition.unlock() }
        let deadline = Date().addingTimeInterval(timeout)
        while anchor == nil, condition.wait(until: deadline) {}
        guard let anchor else { throw CaptureError.runtime("timed out waiting for the first complete screen sample") }
        return anchor
    }
}

final class OutputMonitor: NSObject, SCStreamOutput {
    private let lock = NSLock()
    private var systemAudio = SampleMetrics()
    private var microphone = SampleMetrics()

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of outputType: SCStreamOutputType) {
        lock.lock()
        defer { lock.unlock() }
        switch outputType {
        case .audio: systemAudio.observe(sampleBuffer)
        case .microphone: microphone.observe(sampleBuffer)
        default: break
        }
    }

    func snapshot(for type: SCStreamOutputType) -> SampleMetrics {
        lock.lock()
        defer { lock.unlock() }
        return type == .audio ? systemAudio : microphone
    }
}

final class RecordingDelegate: NSObject, SCRecordingOutputDelegate, SCStreamDelegate {
    private let condition = NSCondition()
    private let log: EventLog
    private var didStart = false
    private var didFinish = false
    private var failure: Error?
    private var startUnixMs: Double?
    private var startMonotonicMs: Double?

    init(log: EventLog) { self.log = log }

    func recordingOutputDidStartRecording(_ recordingOutput: SCRecordingOutput) {
        condition.lock()
        startUnixMs = Date().timeIntervalSince1970 * 1_000
        startMonotonicMs = monotonicMilliseconds()
        didStart = true; condition.broadcast(); condition.unlock()
        log.emit("recording_output_started")
    }

    func recordingOutput(_ recordingOutput: SCRecordingOutput, didFailWithError error: Error) {
        condition.lock(); failure = error; didFinish = true; condition.broadcast(); condition.unlock()
        log.emit("recording_failed", fields: ["message": error.localizedDescription])
    }

    func recordingOutputDidFinishRecording(_ recordingOutput: SCRecordingOutput) {
        condition.lock(); didFinish = true; condition.broadcast(); condition.unlock()
        log.emit("recording_finished", fields: [
            "recordedFileSize": recordingOutput.recordedFileSize,
            "recordedDurationSeconds": recordingOutput.recordedDuration.seconds,
        ])
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        condition.lock(); failure = error; condition.broadcast(); condition.unlock()
        log.emit("stream_failed", fields: ["message": error.localizedDescription])
    }

    func waitUntilStarted(timeout: TimeInterval) throws { try wait(timeout: timeout) { didStart } }
    func waitUntilFinished(timeout: TimeInterval) throws { try wait(timeout: timeout) { didFinish } }

    func startTiming() throws -> (unixMs: Double, monotonicMs: Double) {
        condition.lock(); defer { condition.unlock() }
        guard let unix = startUnixMs, let monotonic = startMonotonicMs else { throw CaptureError.runtime("recording start callback missing") }
        return (unix, monotonic)
    }

    private func wait(timeout: TimeInterval, predicate: () -> Bool) throws {
        condition.lock()
        defer { condition.unlock() }
        let deadline = Date().addingTimeInterval(timeout)
        while !predicate(), failure == nil, condition.wait(until: deadline) {}
        if let failure { throw failure }
        if !predicate() { throw CaptureError.runtime("timed out waiting for recording callback") }
    }
}

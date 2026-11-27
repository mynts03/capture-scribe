import AVFoundation
import CoreGraphics
import CoreMedia
import Darwin
import Foundation
import ScreenCaptureKit

struct Recorder {
    let options: RecordOptions
    let log: EventLog

    func run(display: SCDisplay) async throws {
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let displayWatch = try DisplayConfigurationWatch()
        let geometry = try await resolveGeometry(display: display, filter: filter, area: options.area, approvedOutput: options.approvedOutput)
        try prepareOutput(options.outputURL)
        let configuration = SCStreamConfiguration()
        configuration.width = geometry.outputPixels.width
        configuration.height = geometry.outputPixels.height
        configuration.sourceRect = geometry.sourceRect.rect
        configuration.destinationRect = CGRect(x:0,y:0,width:geometry.contentWidth,height:geometry.contentHeight)
        let background = CGColor(gray:0,alpha:1)
        defer { withExtendedLifetime(background) {} }
        configuration.backgroundColor = background
        configuration.captureResolution = .best
        configuration.scalesToFit = false
        configuration.preservesAspectRatio = true
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(options.fps))
        configuration.queueDepth = 8
        configuration.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        configuration.showsCursor = true
        configuration.capturesAudio = options.capturesSystemAudio
        configuration.sampleRate = 48_000
        configuration.channelCount = 2
        configuration.excludesCurrentProcessAudio = true
        configuration.captureMicrophone = options.capturesMicrophone

        let delegate = RecordingDelegate(log: log)
        let stream = SCStream(filter: filter, configuration: configuration, delegate: delegate)
        let timeline = TimelineAnchorMonitor()
        let monitor = OutputMonitor()
        let queue = DispatchQueue(label: "studio.capturescribe.capture.samples")
        try stream.addStreamOutput(timeline, type: .screen, sampleHandlerQueue: queue)
        if options.capturesSystemAudio { try stream.addStreamOutput(monitor, type: .audio, sampleHandlerQueue: queue) }
        if options.capturesMicrophone { try stream.addStreamOutput(monitor, type: .microphone, sampleHandlerQueue: queue) }

        let outputConfiguration = SCRecordingOutputConfiguration()
        outputConfiguration.outputURL = options.outputURL
        outputConfiguration.videoCodecType = .h264
        outputConfiguration.outputFileType = .mp4
        let recordingOutput = SCRecordingOutput(configuration: outputConfiguration, delegate: delegate)
        try stream.addRecordingOutput(recordingOutput)

        do {
            try await stream.startCapture()
            try delegate.waitUntilStarted(timeout: 10)
        } catch {
            try? await stream.stopCapture()
            throw CaptureError.known("capture.output_configuration_failed", String(describing:error),
                ["width":String(geometry.outputPixels.width),"height":String(geometry.outputPixels.height)])
        }
        let timing = try delegate.startTiming()
        let anchor = try timeline.waitUntilAvailable(timeout: 10)
        guard anchor.pixelWidth == geometry.outputPixels.width, anchor.pixelHeight == geometry.outputPixels.height else {
            try? await stream.stopCapture()
            throw CaptureError.known("capture.geometry_mismatch", "ScreenCaptureKit output dimensions differ from the requested capture plan", [:])
        }
        try stream.removeStreamOutput(timeline, type: .screen)
        log.emit("recording_ready", fields: [
            "samplePTSSeconds": anchor.samplePTSSeconds,
            "displayTimeSeconds": anchor.displayTimeSeconds,
            "output": options.outputURL.path,
            "startedAtUnixMs": UInt64(timing.unixMs),
            "maxDurationOrigin": "recording-output-started-callback",
        ])

        let stopReason = try waitForStopOrDeadline(deadlineMilliseconds: timing.monotonicMs + options.durationSeconds * 1_000, invalidated: {displayWatch.changed})
        log.emit("stop_requested", fields: ["reason": stopReason])
        let finalizeStart = monotonicMilliseconds()
        try await stream.stopCapture()
        try delegate.waitUntilFinished(timeout: 15)
        guard !displayWatch.changed else {
            throw CaptureError.known("capture.region_invalid", "Display configuration changed during recording; working files retained", [:])
        }
        let finalizeMilliseconds = monotonicMilliseconds() - finalizeStart
        let attributes = try FileManager.default.attributesOfItem(atPath: options.outputURL.path)
        let outputSize = (attributes[.size] as? NSNumber)?.int64Value
        let report = RunReport(
            schemaVersion: 1,
            status: "completed",
            stopReason: stopReason,
            startedAt: ISO8601DateFormatter().string(from: Date(timeIntervalSince1970: timing.unixMs / 1_000)),
            finishedAt: ISO8601DateFormatter().string(from: Date()),
            options: options,
            selectedDisplay: describe(display),
            finalizeMilliseconds: finalizeMilliseconds,
            outputSizeBytes: outputSize,
            recordingDurationSeconds: recordingOutput.recordedDuration.finiteSeconds,
            systemAudioMetrics: options.capturesSystemAudio ? monitor.snapshot(for: .audio) : nil,
            microphoneMetrics: options.capturesMicrophone ? monitor.snapshot(for: .microphone) : nil,
            captureGeometry: geometry
        )
        try writeReport(report, to: options.reportURL)
        log.emit("capture_completed", fields: [
            "finalizeMs": finalizeMilliseconds,
            "outputSizeBytes": outputSize ?? -1,
            "report": options.reportURL.path,
        ])
    }
}

func remainingDeadlineMilliseconds(deadline: Double, now: Double) -> Int32 {
    Int32(min(max(0, ceil(deadline - now)), Double(Int32.max)))
}

private func waitForStopOrDeadline(deadlineMilliseconds: Double, invalidated: () -> Bool) throws -> String {
    var descriptor = pollfd(fd: STDIN_FILENO, events: Int16(POLLIN), revents: 0)
    while true {
        if invalidated() { return "configuration_changed" }
        let timeout=remainingDeadlineMilliseconds(deadline:deadlineMilliseconds,now:monotonicMilliseconds())
        if timeout == 0 { return "max_duration" }
        let result=Darwin.poll(&descriptor,1,min(timeout,200))
        if result == 0 { continue }
        if result < 0 {
            if errno == EINTR { continue }
            throw CaptureError.runtime("poll(stdin) failed: \(String(cString:strerror(errno)))")
        }
        guard let line=readLine(strippingNewline:true) else { return "parent_stdin_closed" }
        return line.trimmingCharacters(in:.whitespacesAndNewlines) == "stop" ? "user_stop" : "unknown_control"
    }
}

private func prepareOutput(_ url: URL) throws {
    try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    if FileManager.default.fileExists(atPath: url.path) { throw CaptureError.known("capture.output_exists", "output already exists: \(url.path)", ["path": url.path]) }
}

func describe(_ display: SCDisplay) -> DisplayDescription {
    DisplayDescription(
        id: display.displayID,
        widthPoints: display.width,
        heightPoints: display.height,
        isMain: display.displayID == CGMainDisplayID()
    )
}

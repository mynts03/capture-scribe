import XCTest
import Speech
@testable import AppleTranscriber

final class MessageTests: XCTestCase {
    func testInstalledModelNeedsNoDownload() async throws {
        var installed = false
        try await ensureModelInstalled(locale: "ja-JP", status: { .installed }, install: { installed = true })
        XCTAssertFalse(installed)
    }

    func testPreparationMustActuallyReachInstalled() async throws {
        var installed = false
        try await ensureModelInstalled(locale: "en-US", status: { installed ? .installed : .supported }, install: { installed = true })
        XCTAssertTrue(installed)
        do {
            try await ensureModelInstalled(locale: "en-US", status: { .supported }, install: {})
            XCTFail("Must not claim success without installed assets")
        } catch let error as CLIError { XCTAssertEqual(error.messagePayload["code"] as? String, "transcription.model_failed") }
    }

    func testPreparationFailureKeepsRequestedLocaleAndRaw() async {
        do {
            try await ensureModelInstalled(locale: "en-US", status: { .supported }, install: { throw NSError(domain: "FixtureOffline", code: 7) })
            XCTFail("Expected model failure")
        } catch let error as CLIError {
            XCTAssertEqual(error.messagePayload["code"] as? String, "transcription.model_failed")
            XCTAssertEqual((error.messagePayload["params"] as? [String: String])?["locale"], "en-US")
            XCTAssertTrue((error.messagePayload["raw"] as? String)?.contains("FixtureOffline") == true)
        } catch { XCTFail("Unexpected error: \(error)") }
    }

    func testUnsupportedLocaleDoesNotDownloadOrFallback() async {
        var attempted = false
        do {
            try await ensureModelInstalled(locale: "zz-ZZ", status: { .unsupported }, install: { attempted = true })
            XCTFail("Expected unsupported locale")
        } catch let error as CLIError { XCTAssertEqual(error.messagePayload["code"] as? String, "transcription.locale_unavailable") }
        catch { XCTFail("Unexpected error: \(error)") }
        XCTAssertFalse(attempted)
    }

    func testEmptyTranscriptCodeAndIndependentRawSurviveJSON() throws {
        let payload = CLIError.emptyTranscript.messagePayload
        let data = try JSONSerialization.data(withJSONObject: ["event": "error", "message": payload])
        let event = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let message = try XCTUnwrap(event["message"] as? [String: Any])
        XCTAssertEqual(message["code"] as? String, "transcription.empty")
        XCTAssertTrue((message["raw"] as? String)?.contains("emptyTranscript") == true)
    }
}

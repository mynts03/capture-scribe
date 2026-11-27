import AVFoundation
import Darwin
import Foundation
import Speech

enum CLIError: LocalizedError {
    case usage
    case inputNotFound(String)
    case outputDirectoryMissing(String)
    case speechTranscriberUnavailable
    case localeUnavailable(String)
    case noAudio
    case emptyTranscript
    case assetPreparationFailed(String, String)

    var messagePayload: [String: Any] {
        var payload: [String: Any] = ["raw": "CLIError.\(self): \(errorDescription ?? String(describing: self))"]
        switch self {
        case .emptyTranscript: payload["code"] = "transcription.empty"
        case .noAudio: payload["code"] = "transcription.no_audio"
        case .speechTranscriberUnavailable: payload["code"] = "transcription.unavailable"
        case .localeUnavailable(let identifier):
            payload["code"] = "transcription.locale_unavailable"
            payload["params"] = ["locale": identifier]
        case .assetPreparationFailed(let locale, _):
            payload["code"] = "transcription.model_failed"
            payload["params"] = ["locale": locale]
        default: break
        }
        return payload
    }

    var errorDescription: String? {
        switch self {
        case .usage:
            return "Usage: apple-transcriber <input> <output> --locale <BCP-47> | --locales"
        case .inputNotFound(let path):
            return "入力ファイルが見つかりません: \(path)"
        case .outputDirectoryMissing(let path):
            return "出力先フォルダが見つかりません: \(path)"
        case .speechTranscriberUnavailable:
            return "このMacではSpeechTranscriberを利用できません。macOS 26、対応ハードウェア、言語モデルを確認してください。"
        case .localeUnavailable(let identifier):
            return "指定言語を利用できません: \(identifier)"
        case .noAudio:
            return "音声データを解析できませんでした。"
        case .emptyTranscript:
            return "文字起こし結果が空でした。音声内容とファイル形式を確認してください。"
        case .assetPreparationFailed(let locale, let details):
            return "Speech model preparation failed (\(locale)): \(details)"
        }
    }
}

actor TranscriptCollector {
    private var parts: [String] = []

    func append(_ text: AttributedString) {
        let value = String(text.characters).trimmingCharacters(in: .whitespacesAndNewlines)
        if !value.isEmpty {
            parts.append(value)
        }
    }

    func joined() -> String {
        parts.joined(separator: "\n")
    }
}

// Injectable operations keep failure tests independent of real model downloads.
func ensureModelInstalled(locale: String, status: () async -> AssetInventory.Status,
                          install: () async throws -> Void) async throws {
    if await status() == .installed { return }
    do {
        guard await status() != .unsupported else { throw CLIError.localeUnavailable(locale) }
        try await install()
        guard await status() == .installed else {
            throw CLIError.assetPreparationFailed(locale, "Assets are not installed after preparation")
        }
    } catch let error as CLIError { throw error }
    catch { throw CLIError.assetPreparationFailed(locale, error.localizedDescription) }
}

func prepareAssets(for transcriber: SpeechTranscriber, locale: String) async throws {
    try await ensureModelInstalled(locale: locale, status: {
        await AssetInventory.status(forModules: [transcriber])
    }, install: {
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            try await request.downloadAndInstall()
        }
    })
}

func writeJSON(_ value: [String: Any]) throws {
    let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    print(String(decoding: data, as: UTF8.self))
}

func localeInventory() async -> [String: Any] {
    let available = SpeechTranscriber.isAvailable
    let supported = await SpeechTranscriber.supportedLocales
    let installed = await SpeechTranscriber.installedLocales
    var choices: [[String: Any]] = []
    // Product choices are deliberately limited to Japanese and US English.
    for identifier in ["ja-JP", "en-US"] {
        var choice: [String: Any] = ["locale": identifier, "supported": false, "installed": false, "modelState": "unsupported"]
        if available, let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: identifier)) {
            choice["supported"] = true
            choice["resolvedLocale"] = locale.identifier(.bcp47)
            choice["installed"] = installed.contains(locale)
            choice["modelState"] = String(describing: await AssetInventory.status(forModules: [SpeechTranscriber(locale: locale, preset: .transcription)]))
        }
        choices.append(choice)
    }
    return ["available": available, "supportedLocales": supported.map { $0.identifier(.bcp47) },
            "installedLocales": installed.map { $0.identifier(.bcp47) }, "choices": choices]
}

@main
struct AppleTranscriberCLI {
    static func main() async {
        do {
            try await run()
        } catch {
            let native = error as NSError
            let message = (error as? CLIError)?.messagePayload
                ?? ["raw": "\(native.domain) Code=\(native.code): \(error.localizedDescription)"]
            let event: [String: Any] = ["event": "error", "message": message]
            if let data = try? JSONSerialization.data(withJSONObject: event, options: [.sortedKeys]) {
                FileHandle.standardError.write(data)
                FileHandle.standardError.write(Data("\n".utf8))
            }
            Darwin.exit(EXIT_FAILURE)
        }
    }

    static func run() async throws {
        let arguments = CommandLine.arguments
        if arguments.count == 2, arguments[1] == "--locales" {
            try writeJSON(await localeInventory())
            return
        }
        guard arguments.count == 5, arguments[3] == "--locale", !arguments[4].isEmpty else { throw CLIError.usage }

        let fileManager = FileManager.default
        let inputURL = URL(fileURLWithPath: arguments[1]).standardizedFileURL
        let outputURL = URL(fileURLWithPath: arguments[2]).standardizedFileURL

        guard fileManager.fileExists(atPath: inputURL.path) else {
            throw CLIError.inputNotFound(inputURL.path)
        }
        guard fileManager.fileExists(atPath: outputURL.deletingLastPathComponent().path) else {
            throw CLIError.outputDirectoryMissing(outputURL.deletingLastPathComponent().path)
        }
        guard SpeechTranscriber.isAvailable else {
            throw CLIError.speechTranscriberUnavailable
        }

        let requestedLocale = Locale(identifier: arguments[4])
        guard let locale = await SpeechTranscriber.supportedLocale(
            equivalentTo: requestedLocale
        ) else {
            throw CLIError.localeUnavailable(requestedLocale.identifier)
        }
        // Equivalent regional spelling is allowed, never a different language.
        guard locale.language.languageCode == requestedLocale.language.languageCode else {
            throw CLIError.localeUnavailable(requestedLocale.identifier)
        }

        let transcriber = SpeechTranscriber(locale: locale, preset: .transcription)
        let actualLocale = locale.identifier(.bcp47)
        try await prepareAssets(for: transcriber, locale: actualLocale)

        let audioFile = try AVAudioFile(forReading: inputURL)
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let collector = TranscriptCollector()
        let resultTask = Task {
            for try await result in transcriber.results {
                await collector.append(result.text)
            }
        }

        do {
            let lastSampleTime = try await analyzer.analyzeSequence(from: audioFile)
            if let lastSampleTime {
                try await analyzer.finalizeAndFinish(through: lastSampleTime)
            } else {
                await analyzer.cancelAndFinishNow()
                throw CLIError.noAudio
            }
            try await resultTask.value
        } catch {
            resultTask.cancel()
            await analyzer.cancelAndFinishNow()
            throw error
        }

        let transcript = await collector.joined()
        guard !transcript.isEmpty else { throw CLIError.emptyTranscript }
        try (transcript + "\n").write(
            to: outputURL,
            atomically: true,
            encoding: .utf8
        )
        try writeJSON(["locale": actualLocale, "outputPath": outputURL.path])
    }
}

import Foundation

struct InvalidCatalog: Error, CustomStringConvertible { let description: String }
let regionKeys = ["cancel", "confirm", "accessibility", "modeLabel", "fullScreen", "region", "dragBar"]
func readCatalog(_ path: String) throws -> [String: String] {
    let value = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path)))
    guard let dictionary = value as? [String: String], !dictionary.isEmpty,
          dictionary.allSatisfy({ !$0.value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }) else {
        throw InvalidCatalog(description: "Expected nonempty flat string translations: \(path)")
    }
    let keySyntax = try NSRegularExpression(pattern: #"^[A-Za-z][A-Za-z0-9_.]*$"#)
    guard dictionary.keys.allSatisfy({ keySyntax.firstMatch(in: $0, range: NSRange($0.startIndex..., in: $0)) != nil }) else {
        throw InvalidCatalog(description: "Invalid translation key: \(path)")
    }
    return dictionary
}
func parameters(_ text: String) throws -> [String] {
    let regex = try NSRegularExpression(pattern: #"\{([A-Za-z0-9_]+)\}"#)
    return regex.matches(in: text, range: NSRange(text.startIndex..., in: text)).map {
        String(text[Range($0.range(at: 1), in: text)!])
    }.sorted()
}
func literal(_ text: String) -> String {
    var result = "\""
    for scalar in text.unicodeScalars {
        switch scalar.value {
        case 0x22: result += "\\\""
        case 0x5c: result += "\\\\"
        case 0x0a: result += "\\n"
        case 0x0d: result += "\\r"
        case 0x09: result += "\\t"
        case 0...0x1f, 0x7f: result += "\\u{\(String(scalar.value, radix: 16))}"
        default: result.unicodeScalars.append(scalar)
        }
    }
    return result + "\""
}
do {
    guard CommandLine.arguments.count == 5 else { throw InvalidCatalog(description: "Expected ja.json en.json ui-colors.json output.swift") }
    let ja = try readCatalog(CommandLine.arguments[1]), en = try readCatalog(CommandLine.arguments[2])
    guard Set(ja.keys) == Set(en.keys) else { throw InvalidCatalog(description: "Japanese and English translation keys differ") }
    for key in ja.keys {
        guard try parameters(ja[key]!) == parameters(en[key]!) else { throw InvalidCatalog(description: "Translation placeholders differ: \(key)") }
    }
    let expected = Set(regionKeys.map { "regionSelection.\($0)" })
    guard Set(ja.keys.filter { $0.hasPrefix("regionSelection.") }) == expected else {
        throw InvalidCatalog(description: "Region selection translation keys differ")
    }
    var lines = ["// Generated from src/locales/{ja,en}.json by SwiftPM. Do not edit.",
        "enum RegionLocalization {", "    enum Key: CaseIterable { case \(regionKeys.joined(separator: ", ")) }",
        "    static func text(_ key: Key, language: String) -> String {", "        switch key {"]
    for key in regionKeys {
        let name = "regionSelection.\(key)"
        lines.append("        case .\(key): return language == \"ja\" ? \(literal(ja[name]!)) : \(literal(en[name]!))")
    }
    lines += ["        }", "    }", "}", ""]
    let colors=try readCatalog(CommandLine.arguments[3])
    let nativeRoles:Set<String>=["card","secondary","secondaryBorder","textStrong","primary","primaryText","accent","selectionInput","selectionShade","selectionLabel","muted"]
    let roleSyntax=try NSRegularExpression(pattern:#"^[a-z][A-Za-z0-9]*$"#)
    let hexSyntax=try NSRegularExpression(pattern:#"^#[0-9a-f]{6}([0-9a-f]{2})?$"#)
    guard nativeRoles.isSubset(of:Set(colors.keys)),colors.allSatisfy({ role,value in
        roleSyntax.firstMatch(in:role,range:NSRange(role.startIndex...,in:role)) != nil &&
        hexSyntax.firstMatch(in:value,range:NSRange(value.startIndex...,in:value)) != nil
    }) else { throw InvalidCatalog(description:"Invalid or missing UI colors") }
    lines += ["// Static color tokens from src/ui-colors.json; no runtime file access.","enum UIColors {"]
    for role in colors.keys.sorted() { lines.append("    static let \(role) = \(literal(colors[role]!))") }
    lines += ["}", ""]
    try lines.joined(separator: "\n").write(toFile: CommandLine.arguments[4], atomically: true, encoding: .utf8)
} catch {
    FileHandle.standardError.write(Data("Localization validation failed: \(error)\n".utf8))
    exit(1)
}

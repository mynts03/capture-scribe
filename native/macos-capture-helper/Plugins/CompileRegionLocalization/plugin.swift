import Foundation
import PackagePlugin

@main
struct CompileRegionLocalization: BuildToolPlugin {
    func createBuildCommands(context: PluginContext, target: Target) throws -> [Command] {
        // Shared development originals; never a resource loaded by the shipped helper.
        let root = context.package.directoryURL.deletingLastPathComponent().deletingLastPathComponent()
        let inputs = ["ja", "en"].map { root.appendingPathComponent("src/locales/\($0).json") }
            + [root.appendingPathComponent("src/ui-colors.json")]
        let output = context.pluginWorkDirectoryURL.appendingPathComponent("RegionLocalization.generated.swift")
        return [.buildCommand(displayName: "Validate and embed region selection translations",
            executable: try context.tool(named: "RegionLocalizationGenerator").url,
            arguments: inputs.map(\.path) + [output.path], inputFiles: inputs, outputFiles: [output])]
    }
}

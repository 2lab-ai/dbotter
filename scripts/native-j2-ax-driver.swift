import AppKit
import ApplicationServices
import Darwin
import Foundation

private let bundleIdentifier = "ai.2lab.dbotter.preview"
private let observationSchema = "dbotter.installed-j2-ax-observations.v1"
private let primaryProfileFilter = "J2 Primary"
private let healthyProfileFilter = "J2 Healthy"
private let firstTitle = "J2 Alpha"
private let secondTitle = "J2 Beta"
private let firstSource = "SELECT 41 AS j2_alpha"
private let secondSource = "SELECT 41 AS j2_first; SELECT 42 AS j2_second"
private let failedSource = "SELECT * FROM dbotter_j2_missing_relation"
private let healthySource = "SELECT 84 AS j2_healthy"
private let historySearch = "j2_second"

private enum DriverFailure: Error, CustomStringConvertible {
    case message(String)

    var description: String {
        switch self {
        case let .message(message):
            return message
        }
    }
}

private struct Options {
    let values: [String: String]

    init(_ arguments: [String]) throws {
        var values: [String: String] = [:]
        var index = 0
        while index < arguments.count {
            let key = arguments[index]
            guard key.hasPrefix("--"), index + 1 < arguments.count else {
                throw DriverFailure.message("invalid or incomplete option: \(key)")
            }
            guard values[key] == nil else {
                throw DriverFailure.message("duplicate option: \(key)")
            }
            values[key] = arguments[index + 1]
            index += 2
        }
        self.values = values
    }

    func require(_ key: String) throws -> String {
        guard let value = values[key], !value.isEmpty else {
            throw DriverFailure.message("\(key) is required")
        }
        return value
    }

    func optional(_ key: String) -> String? {
        values[key]
    }

    func rejectUnknown(_ allowed: Set<String>) throws {
        let unknown = Set(values.keys).subtracting(allowed)
        guard unknown.isEmpty else {
            throw DriverFailure.message(
                "unknown options: \(unknown.sorted().joined(separator: ", "))"
            )
        }
    }
}

private struct AXRecord {
    let element: AXUIElement
    let order: [Int]
}

private struct Observation: Encodable {
    let checkpoints: [String: Bool]
    let phase: String
    let pid: Int32
    let schema: String
    let splitValue: Double?

    enum CodingKeys: String, CodingKey {
        case checkpoints
        case phase
        case pid
        case schema
        case splitValue = "split_value"
    }
}

private struct SeedObservation: Decodable {
    let phase: String
    let pid: Int32
    let schema: String
    let splitValue: Double?

    enum CodingKeys: String, CodingKey {
        case phase
        case pid
        case schema
        case splitValue = "split_value"
    }
}

private func fail(_ message: String) -> DriverFailure {
    DriverFailure.message(message)
}

private func pump(_ duration: TimeInterval) {
    let deadline = Date().addingTimeInterval(duration)
    repeat {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.04))
    } while Date() < deadline
}

@discardableResult
private func waitUntil(
    timeout: TimeInterval,
    interval: TimeInterval = 0.08,
    _ condition: () -> Bool
) -> Bool {
    let deadline = Date().addingTimeInterval(timeout)
    repeat {
        if condition() {
            return true
        }
        pump(interval)
    } while Date() < deadline
    return condition()
}

private func requireRegularFile(_ path: String, label: String) throws {
    var status = stat()
    guard path.withCString({ lstat($0, &status) }) == 0,
          status.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
        throw fail("\(label) must be a regular file and not a symlink")
    }
}

private func requireDirectory(_ path: String, label: String) throws {
    var status = stat()
    guard path.withCString({ lstat($0, &status) }) == 0,
          status.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR) else {
        throw fail("\(label) must be a directory and not a symlink")
    }
}

private func requireAbsent(_ path: String, label: String) throws {
    var status = stat()
    if path.withCString({ lstat($0, &status) }) == 0 || errno != ENOENT {
        throw fail("\(label) must not already exist")
    }
}

private func writeAll(_ descriptor: Int32, data: Data) throws {
    try data.withUnsafeBytes { buffer in
        guard let base = buffer.baseAddress else {
            return
        }
        var offset = 0
        while offset < buffer.count {
            let written = Darwin.write(descriptor, base.advanced(by: offset), buffer.count - offset)
            guard written > 0 else {
                throw fail("could not write observation")
            }
            offset += written
        }
    }
}

private func writeObservation(_ observation: Observation, path: String) throws {
    try requireAbsent(path, label: "--output")
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    var data = try encoder.encode(observation)
    data.append(0x0A)
    let descriptor = path.withCString {
        Darwin.open($0, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0o600)
    }
    guard descriptor >= 0 else {
        throw fail("could not create --output without replacement")
    }
    defer { _ = Darwin.close(descriptor) }
    try writeAll(descriptor, data: data)
    guard Darwin.fsync(descriptor) == 0 else {
        throw fail("could not sync --output")
    }
}

private func copyAttribute(
    _ element: AXUIElement,
    _ attribute: CFString
) -> (AXError, CFTypeRef?) {
    var value: CFTypeRef?
    let error = AXUIElementCopyAttributeValue(element, attribute, &value)
    return (error, value)
}

private func stringAttribute(_ element: AXUIElement, _ attribute: CFString) -> String? {
    let (error, value) = copyAttribute(element, attribute)
    guard error == .success else {
        return nil
    }
    if let string = value as? String {
        return string
    }
    if let number = value as? NSNumber {
        return number.stringValue
    }
    return nil
}

private func numberAttribute(_ element: AXUIElement, _ attribute: CFString) -> Double? {
    let (error, value) = copyAttribute(element, attribute)
    guard error == .success else {
        return nil
    }
    if let number = value as? NSNumber {
        return number.doubleValue
    }
    if let string = value as? String {
        return Double(string)
    }
    return nil
}

private func selectedRange(_ element: AXUIElement) -> CFRange? {
    let (error, value) = copyAttribute(element, kAXSelectedTextRangeAttribute as CFString)
    guard error == .success, let value else {
        return nil
    }
    guard CFGetTypeID(value) == AXValueGetTypeID() else {
        return nil
    }
    let axValue = unsafeBitCast(value, to: AXValue.self)
    guard AXValueGetType(axValue) == .cfRange else {
        return nil
    }
    var range = CFRange()
    guard AXValueGetValue(axValue, .cfRange, &range) else {
        return nil
    }
    return range
}

private func setSelectedRange(_ element: AXUIElement, location: Int) throws {
    var range = CFRange(location: location, length: 0)
    guard let value = AXValueCreate(.cfRange, &range) else {
        throw fail("could not construct selected text range")
    }
    let error = AXUIElementSetAttributeValue(
        element,
        kAXSelectedTextRangeAttribute as CFString,
        value
    )
    guard error == .success else {
        throw fail("could not set editor caret: \(error.rawValue)")
    }
}

private func snapshot(_ application: AXUIElement) throws -> [String: [AXRecord]] {
    var records: [String: [AXRecord]] = [:]
    var visited = 0

    func visit(_ element: AXUIElement, order: [Int], depth: Int) throws {
        guard depth <= 64 else {
            throw fail("AX tree exceeds depth bound")
        }
        visited += 1
        guard visited <= 20_000 else {
            throw fail("AX tree exceeds element bound")
        }
        if let authorID = stringAttribute(element, kAXIdentifierAttribute as CFString) {
            records[authorID, default: []].append(AXRecord(element: element, order: order))
        }
        let (error, value) = copyAttribute(element, kAXChildrenAttribute as CFString)
        guard error == .success, let children = value as? [AXUIElement] else {
            return
        }
        for (index, child) in children.enumerated() {
            try visit(child, order: order + [index], depth: depth + 1)
        }
    }

    try visit(application, order: [], depth: 0)
    return records
}

private func single(
    _ identifier: String,
    application: AXUIElement,
    timeout: TimeInterval = 15
) throws -> AXRecord {
    var found: AXRecord?
    let appeared = waitUntil(timeout: timeout) {
        guard let records = try? snapshot(application),
              let matches = records[identifier],
              matches.count == 1 else {
            return false
        }
        found = matches[0]
        return true
    }
    guard appeared, let found else {
        throw fail("AX identifier was not uniquely observed: \(identifier)")
    }
    return found
}

private func optionalSingle(
    _ identifier: String,
    application: AXUIElement
) -> AXRecord? {
    guard let records = try? snapshot(application),
          let matches = records[identifier],
          matches.count == 1 else {
        return nil
    }
    return matches[0]
}

private func prefixRecords(
    _ prefix: String,
    application: AXUIElement
) -> [(String, AXRecord)] {
    guard let records = try? snapshot(application) else {
        return []
    }
    return records
        .filter { $0.key.hasPrefix(prefix) && $0.value.count == 1 }
        .map { ($0.key, $0.value[0]) }
        .sorted { lhs, rhs in orderBefore(lhs.1.order, rhs.1.order) }
}

private func orderBefore(_ lhs: [Int], _ rhs: [Int]) -> Bool {
    for index in 0 ..< min(lhs.count, rhs.count) {
        if lhs[index] != rhs[index] {
            return lhs[index] < rhs[index]
        }
    }
    return lhs.count < rhs.count
}

private func press(_ record: AXRecord) throws {
    let error = AXUIElementPerformAction(record.element, kAXPressAction as CFString)
    guard error == .success else {
        throw fail("AX press failed: \(error.rawValue)")
    }
    pump(0.35)
}

private func setValue(
    identifier: String,
    value: String,
    application: AXUIElement
) throws {
    let record = try single(identifier, application: application)
    let focus = AXUIElementSetAttributeValue(
        record.element,
        kAXFocusedAttribute as CFString,
        kCFBooleanTrue
    )
    guard focus == .success else {
        throw fail("AX focus failed for \(identifier): \(focus.rawValue)")
    }
    let changed = AXUIElementSetAttributeValue(
        record.element,
        kAXValueAttribute as CFString,
        value as CFString
    )
    guard changed == .success else {
        throw fail("AX value change failed for \(identifier): \(changed.rawValue)")
    }
    guard waitUntil(timeout: 8, {
        guard let refreshed = optionalSingle(identifier, application: application) else {
            return false
        }
        return stringAttribute(refreshed.element, kAXValueAttribute as CFString) == value
    }) else {
        throw fail("AX value did not settle for \(identifier)")
    }
}

private func statusValue(
    _ identifier: String,
    application: AXUIElement
) -> String? {
    guard let record = optionalSingle(identifier, application: application) else {
        return nil
    }
    return stringAttribute(record.element, kAXValueAttribute as CFString)
}

private func waitForStatus(
    _ identifier: String,
    application: AXUIElement,
    timeout: TimeInterval,
    _ predicate: @escaping (String) -> Bool
) throws -> String {
    var observed: String?
    guard waitUntil(timeout: timeout, {
        guard let value = statusValue(identifier, application: application) else {
            return false
        }
        observed = value
        return predicate(value)
    }), let observed else {
        throw fail("status did not settle for \(identifier)")
    }
    return observed
}

private func requireTrustedAX() throws {
    let options = [
        kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true,
    ] as CFDictionary
    guard AXIsProcessTrustedWithOptions(options) else {
        throw fail("macOS accessibility permission is required")
    }
}

private func runningPreviewApplications() -> [NSRunningApplication] {
    NSWorkspace.shared.runningApplications.filter {
        $0.bundleIdentifier == bundleIdentifier && !$0.isTerminated
    }
}

private func launch(
    appPath: String,
    configPath: String,
    requireCleanProcessSet: Bool
) throws -> NSRunningApplication {
    try requireDirectory(appPath, label: "--app-path")
    try requireRegularFile(configPath, label: "--config")
    if requireCleanProcessSet && !runningPreviewApplications().isEmpty {
        throw fail("a stale Preview process exists")
    }

    let configuration = NSWorkspace.OpenConfiguration()
    configuration.activates = true
    configuration.addsToRecentItems = false
    configuration.arguments = ["--config", configPath]
    configuration.createsNewApplicationInstance = true
    var launched: NSRunningApplication?
    var launchError: Error?
    NSWorkspace.shared.openApplication(
        at: URL(fileURLWithPath: appPath, isDirectory: true),
        configuration: configuration
    ) { application, error in
        launched = application
        launchError = error
    }
    guard waitUntil(timeout: 30, {
        launched != nil || launchError != nil
    }) else {
        throw fail("timed out launching exact Preview app")
    }
    if let launchError {
        throw fail("Preview launch failed: \(launchError.localizedDescription)")
    }
    guard let launched,
          waitUntil(timeout: 30, {
              launched.isFinishedLaunching || launched.isTerminated
          }),
          !launched.isTerminated,
          launched.bundleIdentifier == bundleIdentifier else {
        throw fail("launched application identity is invalid")
    }
    launched.activate(options: [.activateAllWindows])
    return launched
}

private func applicationElement(pid: Int32) throws -> AXUIElement {
    guard pid > 0,
          Darwin.kill(pid, 0) == 0,
          let application = NSRunningApplication(processIdentifier: pid),
          !application.isTerminated,
          application.bundleIdentifier == bundleIdentifier else {
        throw fail("requested PID is not a live Preview process")
    }
    let element = AXUIElementCreateApplication(pid)
    _ = AXUIElementSetMessagingTimeout(element, 5)
    return element
}

private func selectProfile(
    index: Int,
    filter: String,
    application: AXUIElement
) throws {
    try setValue(
        identifier: "navigator.connection-filter",
        value: filter,
        application: application
    )
    try press(try single("connection.profile.\(index)", application: application))
    _ = try single("workspace.persistence.status", application: application, timeout: 20)
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 20
    ) { value in
        value != "Loading"
    }
}

private func connectSelectedProfile(application: AXUIElement) throws {
    if optionalSingle("connection.disconnect", application: application) != nil {
        return
    }
    try press(try single("connection.connect", application: application, timeout: 15))
    _ = try single("connection.disconnect", application: application, timeout: 30)
}

private func setEditorSource(
    _ source: String,
    application: AXUIElement,
    caretAtEnd: Bool = true
) throws {
    try setValue(identifier: "editor.input", value: source, application: application)
    if caretAtEnd {
        let editor = try single("editor.input", application: application)
        try setSelectedRange(editor.element, location: source.utf16.count)
        pump(0.2)
    }
}

private func waitForExecutionToSettle(
    application: AXUIElement,
    requireResultCount: Int
) throws {
    guard waitUntil(timeout: 45, {
        let results = prefixRecords("result.output.", application: application)
        return results.count >= requireResultCount
            && optionalSingle("editor.pending", application: application) == nil
    }) else {
        throw fail("execution did not produce the expected retained result")
    }
}

private func adjustSplitter(application: AXUIElement) throws -> Double {
    let splitter = try single("workspace.splitter", application: application)
    let before = numberAttribute(splitter.element, kAXValueAttribute as CFString)
    for action in [kAXIncrementAction as CFString, kAXDecrementAction as CFString] {
        let error = AXUIElementPerformAction(splitter.element, action)
        if error == .success,
           waitUntil(timeout: 5, {
               guard let refreshed = optionalSingle("workspace.splitter", application: application),
                     let value = numberAttribute(refreshed.element, kAXValueAttribute as CFString)
               else {
                   return false
               }
               return before == nil || abs(value - (before ?? value)) > 0.000_001
           }),
           let refreshed = optionalSingle("workspace.splitter", application: application),
           let value = numberAttribute(refreshed.element, kAXValueAttribute as CFString) {
            return value
        }
    }
    throw fail("workspace splitter could not be adjusted")
}

private func readSeedObservation(_ path: String) throws -> SeedObservation {
    try requireRegularFile(path, label: "--seed-evidence")
    let observation = try JSONDecoder().decode(
        SeedObservation.self,
        from: Data(contentsOf: URL(fileURLWithPath: path))
    )
    guard observation.schema == observationSchema,
          observation.phase == "seed",
          observation.pid > 0,
          observation.splitValue != nil else {
        throw fail("seed observation is invalid")
    }
    return observation
}

private let commonAllowed: Set<String> = [
    "--phase", "--app-path", "--config", "--output", "--pid", "--seed-evidence",
]

private func runSeed(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let outputPath = try options.require("--output")
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    let launched = try launch(
        appPath: appPath,
        configPath: configPath,
        requireCleanProcessSet: true
    )
    let application = try applicationElement(pid: launched.processIdentifier)
    try selectProfile(index: 0, filter: primaryProfileFilter, application: application)
    try connectSelectedProfile(application: application)

    _ = try single("editor.tab.1", application: application, timeout: 20)
    try setValue(identifier: "editor.tab.title", value: firstTitle, application: application)
    try setEditorSource(firstSource, application: application)
    try press(try single("editor.tab.new", application: application))
    _ = try single("editor.tab.2", application: application)
    try setValue(identifier: "editor.tab.title", value: secondTitle, application: application)
    try setEditorSource(secondSource, application: application)
    try press(try single("editor.tab.move_left", application: application))
    try press(try single("editor.tab.move_right", application: application))
    try press(try single("editor.tab.move_left", application: application))

    let tabOne = try single("editor.tab.1", application: application)
    let tabTwo = try single("editor.tab.2", application: application)
    let selectedTitle = stringAttribute(
        try single("editor.tab.title", application: application).element,
        kAXValueAttribute as CFString
    )
    let tabsCreatedRenamedReordered =
        orderBefore(tabTwo.order, tabOne.order)
        && selectedTitle == secondTitle
    guard tabsCreatedRenamedReordered else {
        throw fail("editor tab create/rename/reorder did not settle")
    }

    let splitValue = try adjustSplitter(application: application)
    try press(try single("editor.execute_all", application: application))
    try waitForExecutionToSettle(application: application, requireResultCount: 2)

    try setEditorSource(failedSource, application: application)
    try press(try single("editor.execute", application: application))
    _ = waitUntil(timeout: 30) {
        optionalSingle("editor.pending", application: application) == nil
    }
    pump(2.2)
    try press(try single("result.tab.history", application: application))
    guard waitUntil(timeout: 20, {
        prefixRecords("history.entry.", application: application).count >= 2
    }) else {
        throw fail("typed success/error history was not retained")
    }
    try press(try single("result.tab.results", application: application))

    try setEditorSource(secondSource, application: application)
    try press(try single("editor.save", application: application))
    let saved = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 30
    ) { $0 == "Saved" }
    guard saved == "Saved" else {
        throw fail("Saved was not visibly reached")
    }

    try writeObservation(
        Observation(
            checkpoints: [
                "tabs_created_renamed_reordered": tabsCreatedRenamedReordered,
                "saved_visible_before_kill": true,
            ],
            phase: "seed",
            pid: launched.processIdentifier,
            schema: observationSchema,
            splitValue: splitValue
        ),
        path: outputPath
    )
}

private func runRestart(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let outputPath = try options.require("--output")
    let seed = try readSeedObservation(try options.require("--seed-evidence"))
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    let launched = try launch(
        appPath: appPath,
        configPath: configPath,
        requireCleanProcessSet: true
    )
    let application = try applicationElement(pid: launched.processIdentifier)
    try selectProfile(index: 0, filter: primaryProfileFilter, application: application)
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 30
    ) { $0 == "Saved" }

    let tabOne = try single("editor.tab.1", application: application)
    let tabTwo = try single("editor.tab.2", application: application)
    let title = stringAttribute(
        try single("editor.tab.title", application: application).element,
        kAXValueAttribute as CFString
    )
    let editor = try single("editor.input", application: application)
    let source = stringAttribute(editor.element, kAXValueAttribute as CFString)
    let caret = selectedRange(editor.element)
    let split = numberAttribute(
        try single("workspace.splitter", application: application).element,
        kAXValueAttribute as CFString
    )
    let tabsRestored =
        orderBefore(tabTwo.order, tabOne.order)
        && title == secondTitle
        && source == secondSource
        && caret?.location == secondSource.utf16.count
        && split != nil
        && abs((split ?? 0) - (seed.splitValue ?? -1)) < 0.000_001
    let resultsOmitted =
        prefixRecords("result.output.", application: application).isEmpty
        && statusValue("status.result", application: application) == "None"
    guard tabsRestored, resultsOmitted else {
        throw fail("restart did not restore tabs or omit result payloads exactly")
    }
    try connectSelectedProfile(application: application)

    try writeObservation(
        Observation(
            checkpoints: [
                "tabs_restored": tabsRestored,
                "results_omitted_after_restart": resultsOmitted,
            ],
            phase: "restart",
            pid: launched.processIdentifier,
            schema: observationSchema,
            splitValue: split
        ),
        path: outputPath
    )
}

private func runHistoryOpen(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let outputPath = try options.require("--output")
    guard let pid = Int32(try options.require("--pid")), pid > 0 else {
        throw fail("--pid must be positive")
    }
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    let application = try applicationElement(pid: pid)
    try press(try single("result.tab.history", application: application))
    try setValue(identifier: "history.search", value: historySearch, application: application)
    guard waitUntil(timeout: 20, {
        !prefixRecords("history.entry.", application: application).isEmpty
    }), let entry = prefixRecords("history.entry.", application: application).first else {
        throw fail("searchable history entry was not observed")
    }
    try press(entry.1)
    guard waitUntil(timeout: 15, {
        prefixRecords("editor.tab.", application: application)
            .filter { !$0.0.hasPrefix("editor.tab.close.") }.count >= 3
            && statusValue("status.operation", application: application)?
                .contains("Run remains explicit") == true
    }) else {
        throw fail("history did not open as a new zero-run editor")
    }
    let openedSource = stringAttribute(
        try single("editor.input", application: application).element,
        kAXValueAttribute as CFString
    )
    let historyOpenedWithoutRun =
        openedSource == secondSource
        && optionalSingle("editor.pending", application: application) == nil
        && prefixRecords("result.output.", application: application).isEmpty
    guard historyOpenedWithoutRun else {
        throw fail("history open dispatched work or changed source identity")
    }
    try writeObservation(
        Observation(
            checkpoints: ["history_opened_without_run": historyOpenedWithoutRun],
            phase: "history-open",
            pid: pid,
            schema: observationSchema,
            splitValue: nil
        ),
        path: outputPath
    )
}

private func runExplicitRun(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let outputPath = try options.require("--output")
    guard let pid = Int32(try options.require("--pid")), pid > 0 else {
        throw fail("--pid must be positive")
    }
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    let application = try applicationElement(pid: pid)
    let editor = try single("editor.input", application: application)
    try setSelectedRange(editor.element, location: secondSource.utf16.count)
    try press(try single("editor.execute", application: application))
    try waitForExecutionToSettle(application: application, requireResultCount: 1)
    let explicitRunCompleted =
        !prefixRecords("result.output.", application: application).isEmpty
        && optionalSingle("editor.pending", application: application) == nil
    guard explicitRunCompleted else {
        throw fail("explicit history rerun did not complete")
    }
    try writeObservation(
        Observation(
            checkpoints: ["explicit_run_completed": explicitRunCompleted],
            phase: "explicit-run",
            pid: pid,
            schema: observationSchema,
            splitValue: nil
        ),
        path: outputPath
    )
}

private func runSecondInstance(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let outputPath = try options.require("--output")
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    guard runningPreviewApplications().count == 1 else {
        throw fail("second-instance phase requires exactly one writer process")
    }
    let launched = try launch(
        appPath: appPath,
        configPath: configPath,
        requireCleanProcessSet: false
    )
    let application = try applicationElement(pid: launched.processIdentifier)
    try selectProfile(index: 0, filter: primaryProfileFilter, application: application)
    let readOnly = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 30
    ) { $0.contains("Read-only") }
    let secondInstanceReadOnly = readOnly.contains("Read-only")
    guard secondInstanceReadOnly else {
        throw fail("second Preview instance did not expose read-only persistence")
    }
    try writeObservation(
        Observation(
            checkpoints: ["second_instance_read_only": secondInstanceReadOnly],
            phase: "second-instance",
            pid: launched.processIdentifier,
            schema: observationSchema,
            splitValue: nil
        ),
        path: outputPath
    )
    launched.terminate()
    guard waitUntil(timeout: 15, { launched.isTerminated }) else {
        launched.forceTerminate()
        _ = waitUntil(timeout: 10, { launched.isTerminated })
        throw fail("second Preview instance did not terminate cleanly")
    }
}

private func runCorruptReopen(_ options: Options) throws {
    try options.rejectUnknown(commonAllowed)
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let outputPath = try options.require("--output")
    try requireAbsent(outputPath, label: "--output")
    try requireTrustedAX()
    let launched = try launch(
        appPath: appPath,
        configPath: configPath,
        requireCleanProcessSet: true
    )
    let application = try applicationElement(pid: launched.processIdentifier)
    try selectProfile(index: 0, filter: primaryProfileFilter, application: application)
    let corruptProfileQuarantined = waitUntil(timeout: 30) {
        optionalSingle("workspace.persistence.retry", application: application) != nil
    }
    guard corruptProfileQuarantined else {
        throw fail("corrupt profile did not expose isolated recovery")
    }

    try selectProfile(index: 1, filter: healthyProfileFilter, application: application)
    try connectSelectedProfile(application: application)
    try setEditorSource(healthySource, application: application)
    try press(try single("editor.execute", application: application))
    try waitForExecutionToSettle(application: application, requireResultCount: 1)
    let healthyProfileRemainsUsable =
        !prefixRecords("result.output.", application: application).isEmpty
        && optionalSingle("editor.pending", application: application) == nil
    guard healthyProfileRemainsUsable else {
        throw fail("healthy profile stopped working after isolated corruption")
    }
    try writeObservation(
        Observation(
            checkpoints: [
                "corrupt_profile_quarantined": corruptProfileQuarantined,
                "healthy_profile_remains_usable": healthyProfileRemainsUsable,
            ],
            phase: "corrupt-reopen",
            pid: launched.processIdentifier,
            schema: observationSchema,
            splitValue: nil
        ),
        path: outputPath
    )
}

private func usage() {
    let text = """
    Usage: native-j2-ax-driver --phase PHASE --app-path PATH --config PATH --output PATH

    Phases:
      seed
      restart --seed-evidence PATH
      history-open --pid PID
      explicit-run --pid PID
      second-instance
      corrupt-reopen
    """
    FileHandle.standardOutput.write(Data((text + "\n").utf8))
}

@main
private struct NativeJ2AXDriver {
    static func main() {
        do {
            let arguments = Array(CommandLine.arguments.dropFirst())
            if arguments == ["--help"] || arguments == ["-h"] {
                usage()
                return
            }
            let options = try Options(arguments)
            switch try options.require("--phase") {
            case "seed":
                try runSeed(options)
            case "restart":
                try runRestart(options)
            case "history-open":
                try runHistoryOpen(options)
            case "explicit-run":
                try runExplicitRun(options)
            case "second-instance":
                try runSecondInstance(options)
            case "corrupt-reopen":
                try runCorruptReopen(options)
            default:
                throw fail("unsupported --phase")
            }
        } catch {
            FileHandle.standardError.write(Data("native J2 AX driver: \(error)\n".utf8))
            Darwin.exit(1)
        }
    }
}

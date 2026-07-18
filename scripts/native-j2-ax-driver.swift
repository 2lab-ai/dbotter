import AppKit
import ApplicationServices
import Darwin
import Foundation

private let bundleIdentifier = "ai.2lab.dbotter.preview"
private let observationSchema = "dbotter.installed-j2-ax-observations.v1"
private let credentialEnvName = "DBOTTER_MYSQL_PASSWORD"
private let primaryProfileFilter = "J2 Primary"
private let healthyProfileFilter = "J2 Healthy"
private let firstTitle = "J2 Alpha"
private let secondTitle = "J2 Beta"
private let firstSource = "SELECT 41 AS j2_alpha"
private let secondSource = "SELECT 41 AS j2_first; SELECT 42 AS j2_second"
private let privateResultSource =
    "SELECT id, value FROM dbotter_j2_private_result ORDER BY id"
private let selectionSource =
    "SELECT 39 AS j2_unselected;\nSELECT 40 AS j2_selected"
private let failedSource = "SELECT * FROM dbotter_j2_missing_relation"
private let healthySource = "SELECT 84 AS j2_healthy"
private let historySearch = "j2_second"
private let exactHistoryMarker = "j2_history_exact"

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

private func boolAttribute(_ element: AXUIElement, _ attribute: CFString) -> Bool? {
    let (error, value) = copyAttribute(element, attribute)
    guard error == .success, let number = value as? NSNumber else {
        return nil
    }
    return number.boolValue
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

private func setSelectedRange(
    _ element: AXUIElement,
    location: Int,
    length: Int = 0
) throws {
    var range = CFRange(location: location, length: length)
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
    pump(0.2)
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

private func editorTabIdentifiers(application: AXUIElement) -> Set<String> {
    numericIdentifiers(prefix: "editor.tab.", application: application)
}

private func resultTabIdentifiers(application: AXUIElement) -> Set<String> {
    numericIdentifiers(prefix: "result.output.", application: application)
}

private func numericIdentifiers(
    prefix: String,
    application: AXUIElement
) -> Set<String> {
    Set(
        prefixRecords(prefix, application: application).compactMap { identifier, _ in
            let suffix = identifier.dropFirst(prefix.count)
            guard !suffix.isEmpty, suffix.allSatisfy(\.isNumber) else {
                return nil
            }
            return identifier
        }
    )
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
    guard let credential = ProcessInfo.processInfo.environment[credentialEnvName],
          !credential.isEmpty else {
        throw fail("the required Environment credential is unavailable")
    }

    let configuration = NSWorkspace.OpenConfiguration()
    configuration.activates = true
    configuration.addsToRecentItems = false
    configuration.arguments = ["--config", configPath]
    configuration.createsNewApplicationInstance = true
    configuration.environment = [credentialEnvName: credential]
    configuration.allowsRunningApplicationSubstitution = false
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

@discardableResult
private func waitForResultDelta(
    application: AXUIElement,
    before: Set<String>,
    delta: Int
) throws -> Set<String> {
    var observed = Set<String>()
    guard waitUntil(timeout: 45, {
        observed = resultTabIdentifiers(application: application)
        return observed.count == before.count + delta
            && observed.isSuperset(of: before)
            && optionalSingle("editor.pending", application: application) == nil
    }) else {
        throw fail("execution did not retain the exact result-tab delta")
    }
    return observed
}

private func historyEntryValues(application: AXUIElement) -> [(AXRecord, String)] {
    prefixRecords("history.entry.", application: application).compactMap { _, record in
        stringAttribute(record.element, kAXValueAttribute as CFString).map { (record, $0) }
    }
}

private func boundaryHistorySource(marker: String, bytes: Int) throws -> String {
    let prefix = "SELECT 1 AS \(marker) /*"
    let suffix = "*/"
    let fixedBytes = prefix.utf8.count + suffix.utf8.count
    guard bytes >= fixedBytes else {
        throw fail("history boundary source size is invalid")
    }
    let source = prefix + String(repeating: "x", count: bytes - fixedBytes) + suffix
    guard source.utf8.count == bytes else {
        throw fail("history boundary source did not reach the exact byte size")
    }
    return source
}

private func currentUTCDate() -> String {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "yyyy-MM-dd"
    return formatter.string(from: Date())
}

private func numericIdentifierSuffix(_ identifier: String) -> Int {
    Int(identifier.split(separator: ".").last ?? "0") ?? 0
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

private func exercisePersistenceOptOutAndClear(application: AXUIElement) throws -> Bool {
    try press(try single("workspace.persistence.toggle", application: application))
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 20
    ) { $0.hasPrefix("Off") }
    try press(try single("workspace.persistence.toggle", application: application))
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 20
    ) { !$0.hasPrefix("Off") && $0 != "Loading" }

    try setEditorSource("SELECT 7 AS j2_clear_probe", application: application)
    let before = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    _ = try waitForResultDelta(application: application, before: before, delta: 1)
    try press(try single("result.tab.history", application: application))
    guard waitUntil(timeout: 20, {
        !historyEntryValues(application: application).isEmpty
    }) else {
        throw fail("persistence clear probe did not enter private history")
    }
    try press(try single("result.tab.results", application: application))
    try press(try single("workspace.persistence.clear", application: application))
    try press(
        try single("workspace.persistence.clear.confirm", application: application)
    )
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 30
    ) { $0.hasPrefix("Off") }
    try press(try single("result.tab.history", application: application))
    guard waitUntil(timeout: 15, {
        historyEntryValues(application: application).isEmpty
    }) else {
        throw fail("durable clear retained a history entry")
    }
    try press(try single("result.tab.results", application: application))
    try press(try single("workspace.persistence.toggle", application: application))
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 20
    ) { !$0.hasPrefix("Off") && $0 != "Loading" }
    return true
}

private func exerciseSyntaxAndAutocomplete(application: AXUIElement) throws -> Bool {
    let refresh = try single(
        "navigator.catalog.refresh-schemas",
        application: application,
        timeout: 20
    )
    try press(refresh)
    guard waitUntil(timeout: 30, {
        optionalSingle(
            "navigator.catalog.refresh-schemas",
            application: application
        ).flatMap {
            boolAttribute($0.element, kAXEnabledAttribute as CFString)
        } == true
    }) else {
        throw fail("catalog refresh did not complete")
    }

    let source = "-- j2 syntax\nSELECT 1 FROM dbo"
    try setEditorSource(source, application: application)
    var candidate: AXRecord?
    guard waitUntil(timeout: 20, {
        candidate = prefixRecords(
            "editor.autocomplete.candidate.",
            application: application
        ).first(where: { _, record in
            stringAttribute(record.element, kAXValueAttribute as CFString) == "dbotter"
        })?.1
        return candidate != nil
    }), let candidate else {
        throw fail("bounded catalog autocomplete candidate was not observed")
    }
    try press(candidate)
    let expected = "-- j2 syntax\nSELECT 1 FROM `dbotter`"
    guard waitUntil(timeout: 10, {
        statusValue("editor.input", application: application) == expected
    }) else {
        throw fail("catalog autocomplete did not replace the exact syntax token")
    }
    return true
}

private func exerciseTabBound(application: AXUIElement) throws -> Bool {
    while editorTabIdentifiers(application: application).count < 20 {
        let before = editorTabIdentifiers(application: application)
        try press(try single("editor.tab.new", application: application))
        guard waitUntil(timeout: 10, {
            let after = editorTabIdentifiers(application: application)
            return after.count == before.count + 1 && after.isSuperset(of: before)
        }) else {
            throw fail("editor tab did not grow to the profile bound")
        }
    }
    let atBound = editorTabIdentifiers(application: application)
    try press(try single("editor.tab.new", application: application))
    pump(0.5)
    guard editorTabIdentifiers(application: application) == atBound else {
        throw fail("editor tab profile bound accepted a plus-one tab")
    }

    try press(try single("editor.save", application: application))
    _ = try waitForStatus(
        "workspace.persistence.status",
        application: application,
        timeout: 30
    ) { $0 == "Saved" }
    let removable = atBound
        .filter { $0 != "editor.tab.1" && $0 != "editor.tab.2" }
        .sorted { numericIdentifierSuffix($0) > numericIdentifierSuffix($1) }
    for identifier in removable {
        let tabIdentifier = identifier.replacingOccurrences(
            of: "editor.tab.",
            with: "editor.tab.close."
        )
        let before = editorTabIdentifiers(application: application)
        try press(try single(tabIdentifier, application: application))
        guard waitUntil(timeout: 10, {
            let after = editorTabIdentifiers(application: application)
            return after.count + 1 == before.count && !after.contains(identifier)
        }) else {
            throw fail("saved excess editor tab could not be closed")
        }
    }
    return editorTabIdentifiers(application: application)
        == Set(["editor.tab.1", "editor.tab.2"])
}

private func exerciseResultInspection(application: AXUIElement) throws -> Bool {
    try press(try single("result.sort.0", application: application))
    guard waitUntil(timeout: 10, {
        statusValue("result.sort.0", application: application)?
            .contains("Ascending") == true
    }) else {
        throw fail("result sort state was not exposed")
    }
    try setValue(identifier: "result.filter", value: "alpha", application: application)
    _ = try waitForStatus(
        "result.filter.status",
        application: application,
        timeout: 10
    ) { $0 == "1 visible of 3" }
    try press(try single("result.cell.1.1", application: application))
    NSPasteboard.general.clearContents()
    try press(try single("result.copy.cell", application: application))
    guard waitUntil(timeout: 10, {
        NSPasteboard.general.string(forType: .string) == "alpha"
    }) else {
        throw fail("selected result cell was not copied exactly")
    }
    NSPasteboard.general.clearContents()
    try press(try single("result.mode.record", application: application))
    _ = try waitForStatus(
        "result.record.status",
        application: application,
        timeout: 10
    ) { $0 == "Record 1 of 1" }
    guard statusValue("result.record.field.1", application: application) == "alpha" else {
        throw fail("record result detail did not retain the selected value")
    }
    try press(try single("result.mode.grid", application: application))
    try setValue(identifier: "result.filter", value: "", application: application)
    _ = try waitForStatus(
        "result.filter.status",
        application: application,
        timeout: 10
    ) { $0 == "3 visible of 3" }
    return true
}

private func inspectHistory(application: AXUIElement) throws -> (Bool, Bool, Bool) {
    try press(try single("result.tab.history", application: application))

    try setValue(identifier: "history.search", value: "succeeded", application: application)
    guard waitUntil(timeout: 20, {
        historyEntryValues(application: application).contains { _, value in
            value.contains("Succeeded")
                && value.contains(" ms")
                && value.contains(" returned")
                && value.contains(" affected")
        }
    }) else {
        throw fail("history success filter did not expose typed metrics")
    }
    try setValue(identifier: "history.search", value: "failed", application: application)
    guard waitUntil(timeout: 20, {
        historyEntryValues(application: application).contains { _, value in
            value.contains("Failed")
        }
    }) else {
        throw fail("history failed-status filter did not match")
    }
    try setValue(identifier: "history.search", value: currentUTCDate(), application: application)
    guard waitUntil(timeout: 20, {
        !historyEntryValues(application: application).isEmpty
    }) else {
        throw fail("history UTC date filter did not match")
    }

    try setValue(identifier: "history.search", value: exactHistoryMarker, application: application)
    var exactRetained = false
    guard waitUntil(timeout: 20, {
        exactRetained = historyEntryValues(application: application).contains { record, value in
            value.contains(exactHistoryMarker)
                && boolAttribute(record.element, kAXEnabledAttribute as CFString) == true
        }
        return exactRetained
    }) else {
        throw fail("exact 64 KiB history source was not reopenable")
    }

    try setValue(identifier: "history.search", value: "", application: application)
    var plusOneOmitted = false
    var filtersAndMetrics = false
    guard waitUntil(timeout: 20, {
        let entries = historyEntryValues(application: application)
        plusOneOmitted = entries.contains { record, value in
            value.contains("Source omitted (over 64 KiB)")
                && boolAttribute(record.element, kAXEnabledAttribute as CFString) == false
        }
        filtersAndMetrics = ["Current", "Selection", "All"].allSatisfy { target in
            entries.contains { _, value in value.contains(" · \(target) · ") }
        } && entries.contains { _, value in
            value.contains(" ms")
                && value.contains(" returned")
                && value.contains(" affected")
        }
        return plusOneOmitted && filtersAndMetrics
    }) else {
        throw fail("history omission, targets, or metrics were not visible")
    }
    try press(try single("result.tab.results", application: application))
    return (filtersAndMetrics, exactRetained, plusOneOmitted)
}

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
    let persistenceOptOutAndClear =
        try exercisePersistenceOptOutAndClear(application: application)
    try press(try single("editor.tab.new", application: application))
    _ = try single("editor.tab.2", application: application)
    let tabBoundEnforced = try exerciseTabBound(application: application)
    guard tabBoundEnforced else {
        throw fail("editor tab bound did not return to the exact two-tab fixture")
    }

    try press(try single("editor.tab.1", application: application))
    try setValue(identifier: "editor.tab.title", value: firstTitle, application: application)
    let syntaxAutocompleteExercised =
        try exerciseSyntaxAndAutocomplete(application: application)

    try setEditorSource(privateResultSource, application: application)
    let beforeCurrent = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    let afterCurrent = try waitForResultDelta(
        application: application,
        before: beforeCurrent,
        delta: 1
    )
    let currentResultTabs = afterCurrent.subtracting(beforeCurrent)
    guard currentResultTabs.count == 1 else {
        throw fail("current execution did not retain one exact result tab")
    }
    let resultInspectionCompleted =
        try exerciseResultInspection(application: application)

    try setEditorSource(selectionSource, application: application)
    let selectedStatement = "SELECT 40 AS j2_selected"
    guard let selectionStart = selectionSource.range(of: selectedStatement) else {
        throw fail("selection fixture is invalid")
    }
    let selectionLocation =
        selectionSource[..<selectionStart.lowerBound].utf16.count
    let editor = try single("editor.input", application: application)
    try setSelectedRange(
        editor.element,
        location: selectionLocation,
        length: selectedStatement.utf16.count
    )
    let beforeSelection = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    let afterSelection = try waitForResultDelta(
        application: application,
        before: beforeSelection,
        delta: 1
    )
    let selectionResultTabs = afterSelection.subtracting(beforeSelection)
    guard selectionResultTabs.count == 1,
          let currentResult = currentResultTabs.first,
          let selectionResult = selectionResultTabs.first else {
        throw fail("selection execution did not retain one exact result tab")
    }
    try press(try single(currentResult, application: application))
    _ = try waitForStatus(
        "result.filter.status",
        application: application,
        timeout: 10
    ) { $0 == "3 visible of 3" }
    try press(try single(selectionResult, application: application))
    _ = try waitForStatus(
        "result.filter.status",
        application: application,
        timeout: 10
    ) { $0 == "1 visible of 1" }

    try press(try single("editor.tab.2", application: application))
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
    let beforeAll = resultTabIdentifiers(application: application)
    try press(try single("editor.execute_all", application: application))
    _ = try waitForResultDelta(
        application: application,
        before: beforeAll,
        delta: 2
    )
    let currentSelectionAllExercised = true

    try setEditorSource(failedSource, application: application)
    try press(try single("editor.execute", application: application))
    guard waitUntil(timeout: 30, {
        optionalSingle("editor.pending", application: application) == nil
    }) else {
        throw fail("failed execution did not settle")
    }

    let exactHistorySource = try boundaryHistorySource(
        marker: exactHistoryMarker,
        bytes: 65_536
    )
    try setEditorSource(exactHistorySource, application: application)
    let beforeExact = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    _ = try waitForResultDelta(
        application: application,
        before: beforeExact,
        delta: 1
    )

    let plusOneHistorySource = try boundaryHistorySource(
        marker: "j2_history_plus_one",
        bytes: 65_537
    )
    try setEditorSource(plusOneHistorySource, application: application)
    let beforePlusOne = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    _ = try waitForResultDelta(
        application: application,
        before: beforePlusOne,
        delta: 1
    )
    let (
        historyFiltersAndMetricsVisible,
        historySourceExactRetained,
        historySourcePlusOneOmitted
    ) = try inspectHistory(application: application)

    try press(try single("editor.tab.1", application: application))
    try setEditorSource(firstSource, application: application)
    try press(try single("editor.tab.2", application: application))
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
                "current_selection_all_exercised": currentSelectionAllExercised,
                "history_filters_and_metrics_visible": historyFiltersAndMetricsVisible,
                "history_source_exact_retained": historySourceExactRetained,
                "history_source_plus_one_omitted": historySourcePlusOneOmitted,
                "persistence_opt_out_and_clear": persistenceOptOutAndClear,
                "result_inspection_completed": resultInspectionCompleted,
                "tabs_created_renamed_reordered": tabsCreatedRenamedReordered,
                "tab_bound_enforced": tabBoundEnforced,
                "syntax_autocomplete_exercised": syntaxAutocompleteExercised,
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
    try press(tabOne)
    let firstRestoredTitle = stringAttribute(
        try single("editor.tab.title", application: application).element,
        kAXValueAttribute as CFString
    )
    let firstEditor = try single("editor.input", application: application)
    let firstRestoredSource = stringAttribute(
        firstEditor.element,
        kAXValueAttribute as CFString
    )
    let firstCaret = selectedRange(firstEditor.element)
    try press(tabTwo)
    let secondRestoredTitle = stringAttribute(
        try single("editor.tab.title", application: application).element,
        kAXValueAttribute as CFString
    )
    let secondEditor = try single("editor.input", application: application)
    let secondRestoredSource = stringAttribute(
        secondEditor.element,
        kAXValueAttribute as CFString
    )
    let secondCaret = selectedRange(secondEditor.element)
    let split = numberAttribute(
        try single("workspace.splitter", application: application).element,
        kAXValueAttribute as CFString
    )
    let tabsRestored =
        orderBefore(tabTwo.order, tabOne.order)
        && firstRestoredTitle == firstTitle
        && firstRestoredSource == firstSource
        && firstCaret?.location == firstSource.utf16.count
        && secondRestoredTitle == secondTitle
        && secondRestoredSource == secondSource
        && secondCaret?.location == secondSource.utf16.count
        && split != nil
        && abs((split ?? 0) - (seed.splitValue ?? -1)) < 0.000_001
    let resultsOmitted =
        resultTabIdentifiers(application: application).isEmpty
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
    let tabIdentifiersBefore = editorTabIdentifiers(application: application)
    try press(entry.1)
    guard waitUntil(timeout: 15, {
        let tabIdentifiersAfter = editorTabIdentifiers(application: application)
        return tabIdentifiersAfter.count == tabIdentifiersBefore.count + 1
            && tabIdentifiersAfter.isSuperset(of: tabIdentifiersBefore)
            && statusValue("status.operation", application: application)?
                .contains("Run remains explicit") == true
    }) else {
        throw fail("history did not open as a new zero-run editor")
    }
    let tabIdentifiersAfter = editorTabIdentifiers(application: application)
    let openedSource = stringAttribute(
        try single("editor.input", application: application).element,
        kAXValueAttribute as CFString
    )
    let historyOpenedWithoutRun =
        tabIdentifiersAfter.count == tabIdentifiersBefore.count + 1
        && tabIdentifiersAfter.isSuperset(of: tabIdentifiersBefore)
        && openedSource == secondSource
        && optionalSingle("editor.pending", application: application) == nil
        && resultTabIdentifiers(application: application).isEmpty
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
    let before = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    _ = try waitForResultDelta(application: application, before: before, delta: 1)
    let explicitRunCompleted =
        resultTabIdentifiers(application: application).count == before.count + 1
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
    let before = resultTabIdentifiers(application: application)
    try press(try single("editor.execute", application: application))
    _ = try waitForResultDelta(application: application, before: before, delta: 1)
    let healthyProfileRemainsUsable =
        resultTabIdentifiers(application: application).count == before.count + 1
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

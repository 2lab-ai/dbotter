import AppKit
import ApplicationServices
import CryptoKit
import Darwin
import Foundation

private let expectedBundleIdentifier = "ai.2lab.dbotter.preview"
private let launchSchema = "dbotter.installed-gui-launch-evidence.v1"
private let observationSchema = "dbotter.native-ax-observations.v1"

private enum DriverFailure: Error, CustomStringConvertible {
    case message(String)

    var description: String {
        switch self {
        case let .message(message):
            return message
        }
    }
}

private struct FileIdentity: Codable, Equatable {
    let realpath: String
    let device: Int64
    let inode: UInt64
    let sha256: String
}

private struct LaunchEvidence: Codable {
    let appPath: String
    let bundleID: String
    let pid: Int32
    let pidExecutable: FileIdentity
    let schema: String
    let staleProcessDisposition: String

    enum CodingKeys: String, CodingKey {
        case appPath = "app_path"
        case bundleID = "bundle_id"
        case pid
        case pidExecutable = "pid_executable"
        case schema
        case staleProcessDisposition = "stale_process_disposition"
    }
}

private struct AXElementObservation: Encodable {
    let enabled: Bool?
    let focused: Bool?
    let identifier: String
    let order: [Int]
    let role: String?
    let title: String?
    let valuePresent: Bool
    let valueProtected: Bool

    enum CodingKeys: String, CodingKey {
        case enabled
        case focused
        case identifier
        case order
        case role
        case title
        case valuePresent = "value_present"
        case valueProtected = "value_protected"
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(enabled, forKey: .enabled)
        try container.encode(focused, forKey: .focused)
        try container.encode(identifier, forKey: .identifier)
        try container.encode(order, forKey: .order)
        try container.encode(role, forKey: .role)
        try container.encode(title, forKey: .title)
        try container.encode(valuePresent, forKey: .valuePresent)
        try container.encode(valueProtected, forKey: .valueProtected)
    }
}

private struct ClipboardObservation: Encodable {
    let afterCount: Int
    let beforeCount: Int
    let byteCount: Int
    let types: [String]

    enum CodingKeys: String, CodingKey {
        case afterCount = "after_count"
        case beforeCount = "before_count"
        case byteCount = "byte_count"
        case types
    }
}

private struct ExportObservation: Encodable {
    let basename: String
    let byteCount: UInt64
    let exists: Bool
    let mode: Int
    let regular: Bool

    enum CodingKeys: String, CodingKey {
        case basename
        case byteCount = "byte_count"
        case exists
        case mode
        case regular
    }
}

private struct InteractionObservation: Encodable {
    let axError: Int32
    let clipboard: ClipboardObservation?
    let export: ExportObservation?
    let identifier: String
    let kind: String
    let mechanism: String

    enum CodingKeys: String, CodingKey {
        case axError = "ax_error"
        case clipboard
        case export
        case identifier
        case kind
        case mechanism
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(axError, forKey: .axError)
        try container.encode(clipboard, forKey: .clipboard)
        try container.encode(export, forKey: .export)
        try container.encode(identifier, forKey: .identifier)
        try container.encode(kind, forKey: .kind)
        try container.encode(mechanism, forKey: .mechanism)
    }
}

private struct JourneyEvidence: Encodable {
    let appPath: String
    let axElements: [AXElementObservation]
    let bundleID: String
    let interactionObservations: [InteractionObservation]
    let pid: Int32
    let pidExecutable: FileIdentity
    let schema: String
    let staleProcessDisposition: String

    enum CodingKeys: String, CodingKey {
        case appPath = "app_path"
        case axElements = "ax_elements"
        case bundleID = "bundle_id"
        case interactionObservations = "interaction_observations"
        case pid
        case pidExecutable = "pid_executable"
        case schema
        case staleProcessDisposition = "stale_process_disposition"
    }
}

private struct ElementRecord {
    let element: AXUIElement
    let observation: AXElementObservation
}

private struct CommandLineOptions {
    let values: [String: String]

    init(arguments: [String]) throws {
        var values: [String: String] = [:]
        var index = 0
        while index < arguments.count {
            let flag = arguments[index]
            guard flag.hasPrefix("--"), index + 1 < arguments.count else {
                throw DriverFailure.message("invalid or incomplete argument: \(flag)")
            }
            guard values[flag] == nil else {
                throw DriverFailure.message("duplicate argument: \(flag)")
            }
            values[flag] = arguments[index + 1]
            index += 2
        }
        self.values = values
    }

    func require(_ flag: String) throws -> String {
        guard let value = values[flag], !value.isEmpty else {
            throw DriverFailure.message("\(flag) is required")
        }
        return value
    }

    func rejectUnknown(allowed: Set<String>) throws {
        let unknown = Set(values.keys).subtracting(allowed)
        guard unknown.isEmpty else {
            throw DriverFailure.message("unknown argument: \(unknown.sorted().joined(separator: ", "))")
        }
    }
}

private func usage() {
    let text = """
    Usage:
      native-ax-driver --phase launch --app-path PATH --config PATH --manifest PATH --output PATH
      native-ax-driver --phase journey --app-path PATH --config PATH --manifest PATH \\
        --pid PID --launch-evidence PATH --required-ids PATH \\
        --export-directory PATH --output PATH

    The launch phase starts the exact app path with the exact config path and writes
    no-replace process evidence. The journey phase reads native AX attributes, sends
    real keyboard and press actions to that PID, and writes raw safe observations.
    """
    FileHandle.standardOutput.write(Data((text + "\n").utf8))
}

private func failure(_ message: String) -> DriverFailure {
    DriverFailure.message(message)
}

private func errnoDescription(_ prefix: String) -> DriverFailure {
    failure("\(prefix): \(String(cString: strerror(errno)))")
}

private func fileStatus(_ path: String, follow: Bool) throws -> stat {
    var value = stat()
    let result = path.withCString { pointer in
        Darwin.fstatat(
            AT_FDCWD,
            pointer,
            &value,
            follow ? 0 : AT_SYMLINK_NOFOLLOW
        )
    }
    guard result == 0 else {
        throw errnoDescription("cannot inspect \(path)")
    }
    return value
}

private func fileType(_ status: stat) -> mode_t {
    status.st_mode & mode_t(S_IFMT)
}

private func requireRegularFile(_ path: String, label: String) throws {
    let status = try fileStatus(path, follow: false)
    guard fileType(status) == mode_t(S_IFREG) else {
        throw failure("\(label) must be a regular file and not a symlink")
    }
}

private func requireDirectory(_ path: String, label: String) throws -> stat {
    let status = try fileStatus(path, follow: false)
    guard fileType(status) == mode_t(S_IFDIR) else {
        throw failure("\(label) must be a directory and not a symlink")
    }
    return status
}

private func requireAbsent(_ path: String, label: String) throws {
    var status = stat()
    let result = path.withCString { Darwin.lstat($0, &status) }
    if result == 0 {
        throw failure("\(label) already exists")
    }
    guard errno == ENOENT else {
        throw errnoDescription("cannot inspect \(label)")
    }
}

private func canonicalPath(_ path: String) throws -> String {
    guard let resolved = path.withCString({ Darwin.realpath($0, nil) }) else {
        throw errnoDescription("cannot resolve \(path)")
    }
    defer { Darwin.free(resolved) }
    return String(cString: resolved)
}

private func sha256File(_ path: String) throws -> String {
    let handle = try FileHandle(forReadingFrom: URL(fileURLWithPath: path))
    defer { try? handle.close() }
    var hasher = SHA256()
    while true {
        let data = try handle.read(upToCount: 1_048_576) ?? Data()
        if data.isEmpty {
            break
        }
        hasher.update(data: data)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

private func executableIdentity(_ executablePath: String) throws -> FileIdentity {
    let realpath = try canonicalPath(executablePath)
    let status = try fileStatus(realpath, follow: true)
    guard fileType(status) == mode_t(S_IFREG) else {
        throw failure("process executable is not a regular file")
    }
    return FileIdentity(
        realpath: realpath,
        device: Int64(status.st_dev),
        inode: UInt64(status.st_ino),
        sha256: try sha256File(realpath)
    )
}

private func bundleExecutable(appPath: String) throws -> (bundle: Bundle, executable: String) {
    _ = try requireDirectory(appPath, label: "--app-path")
    guard let bundle = Bundle(path: appPath), bundle.bundleIdentifier == expectedBundleIdentifier else {
        throw failure("app bundle identifier mismatch")
    }
    guard let executableURL = bundle.executableURL else {
        throw failure("app bundle has no executable")
    }
    let executable = try canonicalPath(executableURL.path)
    try requireRegularFile(executable, label: "app executable")
    return (bundle, executable)
}

private func encodeJSON<T: Encodable>(_ value: T) throws -> Data {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    var data = try encoder.encode(value)
    data.append(0x0A)
    return data
}

private func writeNoReplace(_ data: Data, to outputPath: String) throws {
    try requireAbsent(outputPath, label: "--output")
    let descriptor = outputPath.withCString { pathPointer in
        Darwin.open(pathPointer, O_WRONLY | O_CREAT | O_EXCL, mode_t(S_IRUSR | S_IWUSR))
    }
    guard descriptor >= 0 else {
        throw errnoDescription("cannot create --output without replacement")
    }

    var complete = false
    defer {
        _ = Darwin.close(descriptor)
        if !complete {
            _ = outputPath.withCString { Darwin.unlink($0) }
        }
    }

    try data.withUnsafeBytes { rawBuffer in
        guard let baseAddress = rawBuffer.baseAddress else {
            return
        }
        var offset = 0
        while offset < rawBuffer.count {
            let written = Darwin.write(
                descriptor,
                baseAddress.advanced(by: offset),
                rawBuffer.count - offset
            )
            if written < 0 {
                if errno == EINTR {
                    continue
                }
                throw errnoDescription("cannot write --output")
            }
            guard written > 0 else {
                throw failure("short write while creating --output")
            }
            offset += written
        }
    }
    guard Darwin.fsync(descriptor) == 0 else {
        throw errnoDescription("cannot fsync --output")
    }
    complete = true
}

private func waitUntil(timeout: TimeInterval, condition: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(timeout)
    repeat {
        if condition() {
            return true
        }
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
    } while Date() < deadline
    return condition()
}

private func pumpRunLoop(for duration: TimeInterval) {
    let deadline = Date().addingTimeInterval(duration)
    repeat {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
    } while Date() < deadline
}

private func runLaunch(options: CommandLineOptions) throws {
    try options.rejectUnknown(allowed: ["--phase", "--app-path", "--config", "--manifest", "--output"])
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let manifestPath = try options.require("--manifest")
    let outputPath = try options.require("--output")

    try requireRegularFile(configPath, label: "--config")
    try requireRegularFile(manifestPath, label: "--manifest")
    try requireAbsent(outputPath, label: "--output")
    let app = try bundleExecutable(appPath: appPath)
    let appExecutable = try executableIdentity(app.executable)

    let workspace = NSWorkspace.shared
    let staleApplications = workspace.runningApplications.filter {
        $0.bundleIdentifier == expectedBundleIdentifier && !$0.isTerminated
    }
    guard staleApplications.isEmpty else {
        throw failure("stale preview application process exists")
    }

    let configuration = NSWorkspace.OpenConfiguration()
    configuration.activates = true
    configuration.addsToRecentItems = false
    configuration.arguments = ["--config", configPath]
    configuration.createsNewApplicationInstance = true

    var launchedApplication: NSRunningApplication?
    var launchError: Error?
    workspace.openApplication(
        at: URL(fileURLWithPath: appPath, isDirectory: true),
        configuration: configuration
    ) { application, error in
        launchedApplication = application
        launchError = error
    }

    guard waitUntil(timeout: 30, condition: { launchedApplication != nil || launchError != nil }) else {
        throw failure("timed out launching exact app path")
    }
    if let launchError {
        throw failure("exact app launch failed: \(launchError.localizedDescription)")
    }
    guard let launchedApplication else {
        throw failure("exact app launch returned no process")
    }
    guard waitUntil(timeout: 30, condition: {
        launchedApplication.isFinishedLaunching || launchedApplication.isTerminated
    }), !launchedApplication.isTerminated else {
        throw failure("launched app did not reach a running state")
    }
    guard launchedApplication.bundleIdentifier == expectedBundleIdentifier else {
        throw failure("launched process bundle identifier mismatch")
    }
    guard let launchedExecutableURL = launchedApplication.executableURL else {
        throw failure("launched process has no executable URL")
    }
    let launchedIdentity = try executableIdentity(launchedExecutableURL.path)
    guard launchedIdentity == appExecutable else {
        throw failure("launched process executable does not equal exact app executable")
    }
    let liveApplications = workspace.runningApplications.filter {
        $0.bundleIdentifier == expectedBundleIdentifier && !$0.isTerminated
    }
    guard liveApplications.count == 1,
          liveApplications[0].processIdentifier == launchedApplication.processIdentifier else {
        throw failure("preview application process set changed during launch")
    }

    let evidence = LaunchEvidence(
        appPath: appPath,
        bundleID: expectedBundleIdentifier,
        pid: launchedApplication.processIdentifier,
        pidExecutable: launchedIdentity,
        schema: launchSchema,
        staleProcessDisposition: "none"
    )
    try writeNoReplace(try encodeJSON(evidence), to: outputPath)
}

private func copyAttribute(_ element: AXUIElement, _ attribute: CFString) -> (AXError, CFTypeRef?) {
    var value: CFTypeRef?
    let error = AXUIElementCopyAttributeValue(element, attribute, &value)
    return (error, value)
}

private func stringAttribute(_ element: AXUIElement, _ attribute: CFString) -> String? {
    let (error, value) = copyAttribute(element, attribute)
    guard error == .success else {
        return nil
    }
    return value as? String
}

private func boolAttribute(_ element: AXUIElement, _ attribute: CFString) -> Bool? {
    let (error, value) = copyAttribute(element, attribute)
    guard error == .success else {
        return nil
    }
    return value as? Bool
}

private func titleAttribute(_ element: AXUIElement) -> String? {
    for attribute in [kAXTitleAttribute as CFString, kAXDescriptionAttribute as CFString] {
        if let title = stringAttribute(element, attribute), !title.isEmpty {
            return String(title.prefix(256))
        }
    }
    return nil
}

private func snapshotAXTree(
    application: AXUIElement,
    requiredIDs: Set<String>
) throws -> [String: ElementRecord] {
    var records: [String: ElementRecord] = [:]
    var visited = 0

    func visit(_ element: AXUIElement, order: [Int], depth: Int) throws {
        guard depth <= 64 else {
            throw failure("AX tree exceeds the depth bound")
        }
        visited += 1
        guard visited <= 20_000 else {
            throw failure("AX tree exceeds the element bound")
        }

        if let identifier = stringAttribute(element, kAXIdentifierAttribute as CFString),
           requiredIDs.contains(identifier) {
            guard records[identifier] == nil else {
                throw failure("duplicate AXIdentifier in one tree snapshot: \(identifier)")
            }
            let role = stringAttribute(element, kAXRoleAttribute as CFString)
            let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
            let (valueError, value) = copyAttribute(element, kAXValueAttribute as CFString)
            let valueProtected = subrole == (kAXSecureTextFieldSubrole as String)
            let observation = AXElementObservation(
                enabled: boolAttribute(element, kAXEnabledAttribute as CFString),
                focused: boolAttribute(element, kAXFocusedAttribute as CFString),
                identifier: identifier,
                order: order,
                role: role,
                title: titleAttribute(element),
                valuePresent: !valueProtected && valueError == .success && value != nil,
                valueProtected: valueProtected
            )
            records[identifier] = ElementRecord(element: element, observation: observation)
        }

        let (childrenError, childrenValue) = copyAttribute(element, kAXChildrenAttribute as CFString)
        if childrenError == .noValue || childrenError == .attributeUnsupported {
            return
        }
        guard childrenError == .success else {
            return
        }
        guard let children = childrenValue as? [AXUIElement] else {
            return
        }
        for (index, child) in children.enumerated() {
            try visit(child, order: order + [index], depth: depth + 1)
        }
    }

    try visit(application, order: [], depth: 0)
    return records
}

private func refreshRecords(
    application: AXUIElement,
    requiredIDs: Set<String>,
    accumulated: inout [String: AXElementObservation]
) throws -> [String: ElementRecord] {
    let current = try snapshotAXTree(application: application, requiredIDs: requiredIDs)
    for (identifier, record) in current {
        accumulated[identifier] = record.observation
    }
    return current
}

private func waitForElement(
    identifier: String,
    application: AXUIElement,
    requiredIDs: Set<String>,
    accumulated: inout [String: AXElementObservation],
    timeout: TimeInterval
) throws -> ElementRecord? {
    var result: ElementRecord?
    _ = waitUntil(timeout: timeout) {
        do {
            let records = try refreshRecords(
                application: application,
                requiredIDs: requiredIDs,
                accumulated: &accumulated
            )
            result = records[identifier]
            return result != nil
        } catch {
            return false
        }
    }
    return result
}

private func press(_ record: ElementRecord) -> AXError {
    AXUIElementPerformAction(record.element, kAXPressAction as CFString)
}

private func postKey(
    pid: pid_t,
    virtualKey: CGKeyCode,
    flags: CGEventFlags = []
) throws {
    let source = CGEventSource(stateID: .hidSystemState)
    guard let down = CGEvent(keyboardEventSource: source, virtualKey: virtualKey, keyDown: true),
          let up = CGEvent(keyboardEventSource: source, virtualKey: virtualKey, keyDown: false) else {
        throw failure("CGEvent keyboard construction failed")
    }
    down.flags = flags
    up.flags = flags
    down.postToPid(pid)
    up.postToPid(pid)
}

private func postText(pid: pid_t, text: String) throws {
    let source = CGEventSource(stateID: .hidSystemState)
    let units = Array(text.utf16)
    var start = 0
    while start < units.count {
        var end = min(start + 16, units.count)
        if end < units.count,
           (0xD800 ... 0xDBFF).contains(units[end - 1]),
           (0xDC00 ... 0xDFFF).contains(units[end]) {
            end -= 1
        }
        let chunk = Array(units[start ..< end])
        guard let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else {
            throw failure("CGEvent text construction failed")
        }
        chunk.withUnsafeBufferPointer { buffer in
            down.keyboardSetUnicodeString(
                stringLength: buffer.count,
                unicodeString: buffer.baseAddress
            )
            up.keyboardSetUnicodeString(
                stringLength: buffer.count,
                unicodeString: buffer.baseAddress
            )
        }
        down.postToPid(pid)
        up.postToPid(pid)
        start = end
    }
}

private func dismissTransientUI(pid: pid_t) throws {
    try postKey(pid: pid, virtualKey: 53)
    pumpRunLoop(for: 1)
}

private func revealFormIdentifiers(
    application: AXUIElement,
    requiredIDs: Set<String>,
    accumulated: inout [String: AXElementObservation],
    pid: pid_t
) throws {
    var records = try refreshRecords(
        application: application,
        requiredIDs: requiredIDs,
        accumulated: &accumulated
    )
    if let newButton = records["connection.new"] {
        _ = press(newButton)
        pumpRunLoop(for: 1)
        records = try refreshRecords(
            application: application,
            requiredIDs: requiredIDs,
            accumulated: &accumulated
        )
    }

    for identifier in ["connection.new.mysql", "connection.new.redis"] {
        if let typeButton = try (records[identifier] ?? waitForElement(
            identifier: identifier,
            application: application,
            requiredIDs: requiredIDs,
            accumulated: &accumulated,
            timeout: 2
        )) {
            _ = press(typeButton)
            pumpRunLoop(for: 1)
            _ = try refreshRecords(
                application: application,
                requiredIDs: requiredIDs,
                accumulated: &accumulated
            )
            try dismissTransientUI(pid: pid)
            pumpRunLoop(for: 1)
            records = try refreshRecords(
                application: application,
                requiredIDs: requiredIDs,
                accumulated: &accumulated
            )
            if let newButton = records["connection.new"] {
                _ = press(newButton)
                pumpRunLoop(for: 1)
                records = try refreshRecords(
                    application: application,
                    requiredIDs: requiredIDs,
                    accumulated: &accumulated
                )
            }
        }
    }
    try dismissTransientUI(pid: pid)
}

private func clipboardObservation(beforeCount: Int) -> ClipboardObservation {
    let pasteboard = NSPasteboard.general
    _ = waitUntil(timeout: 5) { pasteboard.changeCount > beforeCount }
    let afterCount = pasteboard.changeCount
    guard afterCount > beforeCount else {
        return ClipboardObservation(
            afterCount: afterCount,
            beforeCount: beforeCount,
            byteCount: 0,
            types: []
        )
    }
    let types = (pasteboard.types ?? []).map(\.rawValue).sorted()
    let byteCount: Int
    if let string = pasteboard.string(forType: .string) {
        byteCount = string.lengthOfBytes(using: .utf8)
    } else if let firstType = pasteboard.types?.first,
              let data = pasteboard.data(forType: firstType) {
        byteCount = data.count
    } else {
        byteCount = 0
    }
    return ClipboardObservation(
        afterCount: afterCount,
        beforeCount: beforeCount,
        byteCount: byteCount,
        types: types
    )
}

private func safeExportObservation(path: String, basename: String) -> ExportObservation {
    var status = stat()
    let result = path.withCString { Darwin.lstat($0, &status) }
    guard result == 0 else {
        return ExportObservation(
            basename: basename,
            byteCount: 0,
            exists: false,
            mode: 0,
            regular: false
        )
    }
    let regular = fileType(status) == mode_t(S_IFREG)
    return ExportObservation(
        basename: basename,
        byteCount: regular ? UInt64(max(status.st_size, 0)) : 0,
        exists: true,
        mode: Int(status.st_mode & 0o777),
        regular: regular
    )
}

private func axTreeContainsDialog(_ application: AXUIElement) -> Bool {
    var visited = 0
    func visit(_ element: AXUIElement, depth: Int) -> Bool {
        guard depth <= 32, visited < 10_000 else {
            return false
        }
        visited += 1
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        if role == (kAXSheetRole as String)
            || subrole == (kAXDialogSubrole as String)
            || subrole == (kAXSystemDialogSubrole as String)
        {
            return true
        }
        let (error, value) = copyAttribute(element, kAXChildrenAttribute as CFString)
        guard error == .success, let children = value as? [AXUIElement] else {
            return false
        }
        return children.contains { visit($0, depth: depth + 1) }
    }
    return visit(application, depth: 0)
}

private func driveSavePanel(
    application: AXUIElement,
    pid: pid_t,
    exportDirectory: String,
    basename: String,
    outputPath: String
) throws {
    guard waitUntil(timeout: 10, condition: { axTreeContainsDialog(application) }) else {
        return
    }

    try postKey(pid: pid, virtualKey: 5, flags: [.maskCommand, .maskShift])
    pumpRunLoop(for: 0.4)
    try postText(pid: pid, text: exportDirectory)
    try postKey(pid: pid, virtualKey: 36)
    pumpRunLoop(for: 0.8)
    try postKey(pid: pid, virtualKey: 0, flags: [.maskCommand])
    try postText(pid: pid, text: basename)
    try postKey(pid: pid, virtualKey: 36)
    _ = waitUntil(timeout: 20) {
        var status = stat()
        return outputPath.withCString { Darwin.lstat($0, &status) } == 0
    }
}

private func decodeLaunchEvidence(_ path: String) throws -> LaunchEvidence {
    try requireRegularFile(path, label: "--launch-evidence")
    let data = try Data(contentsOf: URL(fileURLWithPath: path), options: [.mappedIfSafe])
    let object = try JSONSerialization.jsonObject(with: data)
    guard let dictionary = object as? [String: Any],
          Set(dictionary.keys) == [
              "app_path", "bundle_id", "pid", "pid_executable", "schema",
              "stale_process_disposition",
          ] else {
        throw failure("launch evidence has unexpected top-level fields")
    }
    return try JSONDecoder().decode(LaunchEvidence.self, from: data)
}

private func decodeRequiredIDs(_ path: String) throws -> [String] {
    try requireRegularFile(path, label: "--required-ids")
    let data = try Data(contentsOf: URL(fileURLWithPath: path), options: [.mappedIfSafe])
    guard let values = try JSONSerialization.jsonObject(with: data) as? [String],
          !values.isEmpty,
          values.count == Set(values).count else {
        throw failure("--required-ids must be a non-empty unique JSON string array")
    }
    for value in values {
        guard value.count <= 128,
              value.range(of: "^[a-z0-9][a-z0-9._-]*$", options: .regularExpression) != nil else {
            throw failure("--required-ids contains an unsafe identifier")
        }
    }
    return values
}

private func runJourney(options: CommandLineOptions) throws {
    try options.rejectUnknown(allowed: [
        "--phase", "--app-path", "--config", "--manifest", "--pid",
        "--launch-evidence", "--required-ids", "--export-directory", "--output",
    ])
    let appPath = try options.require("--app-path")
    let configPath = try options.require("--config")
    let manifestPath = try options.require("--manifest")
    let launchEvidencePath = try options.require("--launch-evidence")
    let requiredIDsPath = try options.require("--required-ids")
    let exportDirectoryPath = try options.require("--export-directory")
    let outputPath = try options.require("--output")
    guard let parsedPID = Int32(try options.require("--pid")), parsedPID > 0 else {
        throw failure("--pid must be a positive process identifier")
    }

    try requireRegularFile(configPath, label: "--config")
    try requireRegularFile(manifestPath, label: "--manifest")
    try requireAbsent(outputPath, label: "--output")
    _ = try bundleExecutable(appPath: appPath)
    let exportStatus = try requireDirectory(exportDirectoryPath, label: "--export-directory")
    guard Int(exportStatus.st_mode & 0o777) == 0o700 else {
        throw failure("--export-directory mode must be 0700")
    }
    let exportDirectory = try canonicalPath(exportDirectoryPath)
    let initialExportEntries = try FileManager.default.contentsOfDirectory(atPath: exportDirectory)
    guard initialExportEntries.isEmpty else {
        throw failure("--export-directory must start empty")
    }

    let launch = try decodeLaunchEvidence(launchEvidencePath)
    guard launch.schema == launchSchema,
          launch.appPath == appPath,
          launch.bundleID == expectedBundleIdentifier,
          launch.pid == parsedPID,
          launch.staleProcessDisposition == "none" || launch.staleProcessDisposition == "terminated" else {
        throw failure("launch evidence does not bind the requested journey")
    }
    guard Darwin.kill(parsedPID, 0) == 0 else {
        throw failure("requested PID is not alive")
    }
    guard let runningApplication = NSRunningApplication(processIdentifier: parsedPID),
          !runningApplication.isTerminated,
          runningApplication.bundleIdentifier == expectedBundleIdentifier,
          let executableURL = runningApplication.executableURL else {
        throw failure("requested PID is not the preview application")
    }
    let currentIdentity = try executableIdentity(executableURL.path)
    guard currentIdentity == launch.pidExecutable else {
        throw failure("requested PID executable changed after launch evidence")
    }

    let trustOptions = [
        kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true,
    ] as CFDictionary
    guard AXIsProcessTrustedWithOptions(trustOptions) else {
        throw failure("macOS accessibility permission is required")
    }

    let requiredIDs = try decodeRequiredIDs(requiredIDsPath)
    let requiredIDSet = Set(requiredIDs)
    let application = AXUIElementCreateApplication(parsedPID)
    _ = AXUIElementSetMessagingTimeout(application, 5)
    var accumulated: [String: AXElementObservation] = [:]
    var interactions: [InteractionObservation] = []

    try revealFormIdentifiers(
        application: application,
        requiredIDs: requiredIDSet,
        accumulated: &accumulated,
        pid: parsedPID
    )

    guard let editorInput = try waitForElement(
        identifier: "editor.input",
        application: application,
        requiredIDs: requiredIDSet,
        accumulated: &accumulated,
        timeout: 15
    ) else {
        throw failure("editor.input AXIdentifier was not observed")
    }
    let focusError = AXUIElementSetAttributeValue(
        editorInput.element,
        kAXFocusedAttribute as CFString,
        kCFBooleanTrue
    )
    let valueError = AXUIElementSetAttributeValue(
        editorInput.element,
        kAXValueAttribute as CFString,
        "SELECT 1 AS native_ax_value" as CFString
    )
    try postKey(pid: parsedPID, virtualKey: 36, flags: .maskCommand)
    let keyboardError = focusError == .success ? valueError : focusError
    interactions.append(InteractionObservation(
        axError: keyboardError.rawValue,
        clipboard: nil,
        export: nil,
        identifier: "editor.execute",
        kind: "keyboard",
        mechanism: "CGEvent.postToPid"
    ))

    for identifier in ["result.copy.cell", "result.copy.row", "result.copy.all"] {
        guard let record = try waitForElement(
            identifier: identifier,
            application: application,
            requiredIDs: requiredIDSet,
            accumulated: &accumulated,
            timeout: 30
        ) else {
            throw failure("\(identifier) AXIdentifier was not observed")
        }
        let beforeCount = NSPasteboard.general.changeCount
        let actionError = press(record)
        let clipboard = clipboardObservation(beforeCount: beforeCount)
        interactions.append(InteractionObservation(
            axError: actionError.rawValue,
            clipboard: clipboard,
            export: nil,
            identifier: identifier,
            kind: "press",
            mechanism: "AXUIElementPerformAction"
        ))
    }

    let exportControls = [
        (identifier: "result.export.csv", basename: "dbotter-native-ax-result.csv"),
        (identifier: "result.export.tsv", basename: "dbotter-native-ax-result.tsv"),
        (identifier: "result.export.json", basename: "dbotter-native-ax-result.json"),
    ]
    for exportControl in exportControls {
        guard let record = try waitForElement(
            identifier: exportControl.identifier,
            application: application,
            requiredIDs: requiredIDSet,
            accumulated: &accumulated,
            timeout: 10
        ) else {
            throw failure("\(exportControl.identifier) AXIdentifier was not observed")
        }
        let outputURL = URL(fileURLWithPath: exportDirectory, isDirectory: true)
            .appendingPathComponent(exportControl.basename, isDirectory: false)
        let outputPath = outputURL.path
        try requireAbsent(outputPath, label: "export destination")
        let actionError = press(record)
        if actionError == .success {
            try driveSavePanel(
                application: application,
                pid: parsedPID,
                exportDirectory: exportDirectory,
                basename: exportControl.basename,
                outputPath: outputPath
            )
        }
        interactions.append(InteractionObservation(
            axError: actionError.rawValue,
            clipboard: nil,
            export: safeExportObservation(path: outputPath, basename: exportControl.basename),
            identifier: exportControl.identifier,
            kind: "press",
            mechanism: "AXUIElementPerformAction"
        ))
    }

    _ = try refreshRecords(
        application: application,
        requiredIDs: requiredIDSet,
        accumulated: &accumulated
    )
    let missingIDs = requiredIDs.filter { accumulated[$0] == nil }
    guard missingIDs.isEmpty else {
        throw failure("required AXIdentifiers were not observed: \(missingIDs.joined(separator: ", "))")
    }
    guard Darwin.kill(parsedPID, 0) == 0 else {
        throw failure("requested PID exited during the AX journey")
    }

    let elements = requiredIDs.compactMap { accumulated[$0] }
    let evidence = JourneyEvidence(
        appPath: appPath,
        axElements: elements,
        bundleID: expectedBundleIdentifier,
        interactionObservations: interactions,
        pid: parsedPID,
        pidExecutable: currentIdentity,
        schema: observationSchema,
        staleProcessDisposition: launch.staleProcessDisposition
    )
    try writeNoReplace(try encodeJSON(evidence), to: outputPath)
}

@main
private struct NativeAXDriver {
    static func main() {
        do {
            let arguments = Array(CommandLine.arguments.dropFirst())
            if arguments == ["--help"] || arguments == ["-h"] {
                usage()
                return
            }
            let options = try CommandLineOptions(arguments: arguments)
            switch try options.require("--phase") {
            case "launch":
                try runLaunch(options: options)
            case "journey":
                try runJourney(options: options)
            default:
                throw failure("--phase must be launch or journey")
            }
        } catch {
            FileHandle.standardError.write(Data("native AX driver: \(error)\n".utf8))
            Darwin.exit(1)
        }
    }
}

import AppKit
import Vision

func reply(_ line: String) {
    FileHandle.standardOutput.write(Data((line + "\n").utf8))
}

if CommandLine.arguments == [CommandLine.arguments[0], "--check-aqua"] {
    let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
    guard session["kCGSessionLoginDoneKey"] as? Bool == true,
          session["CGSSessionScreenIsLocked"] as? Bool != true else { exit(77) }
    exit(0)
}

if CommandLine.arguments.count == 2 {
    let image = URL(fileURLWithPath: CommandLine.arguments[1])
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .fast
    try VNImageRequestHandler(url: image, options: [:]).perform([request])
    for line in request.results ?? [] {
        if let text = line.topCandidates(1).first?.string { reply(text) }
    }
    exit(0)
}

if CommandLine.arguments.count >= 4 && CommandLine.arguments[1] == "sheet" {
    let args = CommandLine.arguments
    guard AXIsProcessTrusted(), let pid = pid_t(args[2]) else { exit(77) }
    var elements: [(AXUIElement, [String])] = []
    func visit(_ element: AXUIElement, _ depth: Int) {
        guard depth > 0, !elements.contains(where: { CFEqual($0.0, element) }) else { return }
        var labels: [String] = []
        for name in [kAXRoleAttribute, kAXTitleAttribute, kAXValueAttribute, kAXDescriptionAttribute] {
            var value: CFTypeRef?
            if AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success,
               let text = value as? String { labels.append(text) }
        }
        elements.append((element, labels))
        for name in [kAXWindowsAttribute, "AXSheets", kAXChildrenAttribute] {
            var value: CFTypeRef?
            if AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success,
               let children = value as? [AXUIElement] {
                for child in children { visit(child, depth - 1) }
            }
        }
    }
    visit(AXUIElementCreateApplication(pid), 6)
    let strings = elements.flatMap { $0.1 }
    guard strings.contains("Computer use"), strings.contains("Allow for this session"), strings.contains("Deny"),
          strings.contains(where: { $0.contains("for session " + args[3]) }) else {
        reply("AX roles: \(Set(strings.filter { $0.hasPrefix("AX") }).sorted()); " +
              "dialog labels: \(strings.filter { $0.contains("Computer use") || $0.contains("this session") })")
        exit(1)
    }
    reply("Computer use sheet for \(args[3]) in Desktop PID \(pid)")
    if args.count == 6 {
        guard NSRunningApplication(processIdentifier: pid)?.executableURL?.resolvingSymlinksInPath().path == args[5]
        else { exit(2) }
        let buttons = elements.filter { $0.1.contains(kAXButtonRole) && $0.1.contains(args[4]) }
        guard buttons.count == 1,
              AXUIElementPerformAction(buttons[0].0, kAXPressAction as CFString) == .success else { exit(2) }
        reply("Fixture invoked \(args[4]) in Desktop PID \(pid)")
    }
    exit(0)
}

func window(title: String, x: CGFloat, marker: String, value: String) -> (NSWindow, NSTextField) {
    let window = NSWindow(
        contentRect: NSRect(x: x, y: 140, width: 500, height: 180),
        styleMask: [.titled, .closable], backing: .buffered, defer: false
    )
    window.title = title
    let label = NSTextField(labelWithString: marker)
    label.frame = NSRect(x: 20, y: 105, width: 460, height: 45)
    label.font = .boldSystemFont(ofSize: 28)
    let field = NSTextField(string: value)
    field.frame = NSRect(x: 20, y: 40, width: 460, height: 45)
    field.font = .systemFont(ofSize: 25)
    field.isEditable = true
    field.setAccessibilityLabel("Acceptance input")
    window.contentView!.addSubview(label)
    window.contentView!.addSubview(field)
    window.orderFrontRegardless()
    window.displayIfNeeded()
    return (window, field)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let (target, input) = window(title: "Jcode AX Acceptance", x: 50,
                             marker: "JCODE ACCEPTANCE WINDOW", value: "JCODE TEXT ORIGINAL")
let (decoy, decoyInput) = window(title: "Jcode AX Decoy", x: 125,
                                 marker: "JCODE DECOY WINDOW", value: "JCODE DECOY ORIGINAL")

func presented(_ window: NSWindow) -> Bool {
    guard let id = CGWindowID(exactly: window.windowNumber), id > 0,
          let entries = CGWindowListCopyWindowInfo(.optionIncludingWindow, id) as? [[String: Any]]
    else { return false }
    return entries.contains { entry in
        guard entry[kCGWindowNumber as String] as? CGWindowID == id,
              entry[kCGWindowOwnerPID as String] as? pid_t == getpid(),
              let raw = entry[kCGWindowBounds as String] as? NSDictionary,
              let bounds = CGRect(dictionaryRepresentation: raw as CFDictionary)
        else { return false }
        let frame = window.frame
        let top = CGDisplayBounds(CGMainDisplayID()).height - frame.maxY
        return abs(bounds.minX - frame.minX) < 1 && abs(bounds.minY - top) < 1
            && abs(bounds.width - frame.width) < 1 && abs(bounds.height - frame.height) < 1
    }
}

func reportWindows() {
    if presented(target) && presented(decoy) {
        reply("\(target.windowNumber)|\(decoy.windowNumber)")
    } else {
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(50), execute: reportWindows)
    }
}
DispatchQueue.main.async(execute: reportWindows)

FileHandle.standardInput.readabilityHandler = { handle in
    let data = handle.availableData
    DispatchQueue.main.async {
        if data.isEmpty { app.terminate(nil); return }
        for command in String(decoding: data, as: UTF8.self).split(separator: "\n") {
            switch command {
            case "state": reply("\(input.stringValue)|\(decoyInput.stringValue)|\(app.isActive)")
            case "focus-input":
                app.activate(ignoringOtherApps: true)
                target.makeKeyAndOrderFront(nil)
                target.makeFirstResponder(input)
                if let editor = input.currentEditor() {
                    editor.selectedRange = NSRange(location: (input.stringValue as NSString).length, length: 0)
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(250)) {
                    reply(app.isActive && target.isKeyWindow && input.currentEditor() != nil ? "ready" : "not ready")
                }
            case "hide": app.hide(nil); reply("hidden")
            default: reply("unknown command")
            }
        }
    }
}
app.run()

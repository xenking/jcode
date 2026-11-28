import AppKit

final class Fixture: NSObject, NSTableViewDataSource {
    let table = NSTableView()
    let first: NSWindow
    let second: NSWindow
    let firstInput: NSTextField
    let secondInput: NSTextField
    let firstButton: NSButton

    override init() {
        func window(x: CGFloat, title: String) -> NSWindow {
            let window = NSWindow(contentRect: NSRect(x: x, y: 150, width: 640, height: 360),
                                  styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
            window.title = title
            window.isReleasedWhenClosed = false
            return window
        }
        func field(_ parent: NSWindow, label: String, value: String) -> NSTextField {
            let field = NSTextField(string: value)
            field.frame = NSRect(x: 30, y: 260, width: 400, height: 25)
            field.isEditable = true
            field.setAccessibilityLabel(label)
            field.setAccessibilityIdentifier(label)
            parent.contentView!.addSubview(field)
            return field
        }
        first = window(x: 170, title: "AX Fixture First")
        second = window(x: 260, title: "AX Fixture Second")
        firstInput = field(first, label: "First input", value: "first-original")
        secondInput = field(second, label: "Second input", value: "second-original")
        firstButton = NSButton(title: "Fixture button", target: nil, action: nil)
        firstButton.frame = NSRect(x: 30, y: 210, width: 180, height: 25)
        first.contentView!.addSubview(firstButton)
        let secret = NSSecureTextField(string: "fixture-secret-value")
        secret.frame = NSRect(x: 30, y: 160, width: 200, height: 25)
        first.contentView!.addSubview(secret)
        super.init()
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("items"))
        column.width = 200
        table.addTableColumn(column)
        table.headerView = nil
        table.frame = NSRect(x: 300, y: 30, width: 200, height: 100)
        table.dataSource = self
        first.contentView!.addSubview(table)
        table.reloadData()
        firstButton.target = self
        firstButton.action = #selector(press)
    }

    func numberOfRows(in tableView: NSTableView) -> Int { 2 }
    func tableView(_ tableView: NSTableView, objectValueFor column: NSTableColumn?, row: Int) -> Any? {
        ["First row", "Second row"][row]
    }

    @objc func press() { firstInput.stringValue = "pressed-first" }

    func command(_ name: String) {
        switch name {
        case "front-second":
            second.makeKeyAndOrderFront(nil)
            reply(NSApp.keyWindow == second ? "second-front" : "no-key-window")
        case "rename-first":
            firstInput.setAccessibilityLabel("Relabeled input")
            firstInput.setAccessibilityIdentifier("Relabeled input")
            reply("renamed")
        case "close-first":
            first.close()
            reply("closed")
        case "set-first":
            firstInput.stringValue = "fixture-updated"
            reply("updated")
        case "selected-row":
            reply("\(table.selectedRow)")
        case "values":
            reply("\(firstInput.stringValue)|\(secondInput.stringValue)")
        default:
            reply("unknown-command")
        }
    }
}

func reply(_ line: String) {
    FileHandle.standardOutput.write(Data((line + "\n").utf8))
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let fixture = Fixture()
if ProcessInfo.processInfo.environment["JCODE_FIXTURE_BACKGROUND"] == nil {
    fixture.second.orderFront(nil)
    fixture.first.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)
} else {
    fixture.second.close()
    fixture.first.orderFront(nil)
}
DispatchQueue.main.async {
    reply("\(fixture.first.windowNumber)|\(fixture.second.windowNumber)")
}
FileHandle.standardInput.readabilityHandler = { handle in
    let data = handle.availableData
    DispatchQueue.main.async {
        if data.isEmpty { app.terminate(nil); return }
        for command in String(decoding: data, as: UTF8.self).split(separator: "\n") {
            fixture.command(String(command))
        }
    }
}
app.run()

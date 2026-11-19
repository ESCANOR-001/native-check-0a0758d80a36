import CoreGraphics
import Foundation
import Vision

let arguments = CommandLine.arguments.dropFirst()
let hid = CGEventSource(stateID: .hidSystemState)

func key(_ code: CGKeyCode, _ flags: CGEventFlags = []) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: hid, virtualKey: code, keyDown: down)!
        event.flags = flags
        event.post(tap: .cghidEventTap)
        usleep(13_000)
    }
}

switch arguments.first {
case "click":
    let coordinates = arguments.dropFirst().map { Double($0)! }
    let point = CGPoint(x: coordinates[0], y: coordinates[1])
    for action: CGEventType in [.mouseMoved, .leftMouseDown, .leftMouseUp] {
        CGEvent(mouseEventSource: hid, mouseType: action, mouseCursorPosition: point, mouseButton: .left)!
            .post(tap: .cghidEventTap)
        usleep(30_000)
    }
case "type":
    // Physical ANSI key codes work in the secure password field too. The
    // disposable sign-in account uses only lower-case ASCII and digits.
    let codes: [Character: CGKeyCode] = [
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8,
        "v": 9, "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
        "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "9": 25, "7": 26, "8": 28, "0": 29,
        "o": 31, "u": 32, "i": 34, "p": 35, "l": 37, "j": 38, "k": 40, "n": 45, "m": 46
    ]
    guard let text = readLine(), text.allSatisfy({ codes[$0] != nil }) else { exit(1) }
    for character in text { key(codes[character]!) }
case "tab": key(48)
case "down": key(125)
case "return": key(36)
case "escape": key(53)
case "read-screen":
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.recognitionLanguages = ["en-US"]
    let url = URL(fileURLWithPath: String(arguments[arguments.index(after: arguments.startIndex)]))
    try VNImageRequestHandler(url: url).perform([request])
    let display = CGDisplayBounds(CGMainDisplayID())
    for item in request.results ?? [] {
        let rect = item.boundingBox
        guard let text = item.topCandidates(1).first?.string else { continue }
        let x = Int(rect.midX * display.width + display.origin.x)
        let y = Int((1.0 - rect.midY) * display.height + display.origin.y)
        print("\(x)\t\(y)\t\(text)")
    }
default:
    exit(2)
}

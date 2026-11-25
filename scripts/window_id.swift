// Prints the window ID of the largest on-screen window owned by a process
// (the app window, not its menu bar strip).
// usage: swift window_id.swift <pid>
import CoreGraphics
import Foundation

let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as! [[String: Any]]
let best = windows
    .filter { ($0[kCGWindowOwnerPID as String] as? Int) == pid }
    .compactMap { w -> (Int, Double)? in
        guard let id = w[kCGWindowNumber as String] as? Int,
              let b = w[kCGWindowBounds as String] as? [String: Double],
              let width = b["Width"], let height = b["Height"], width > 0, height > 100 else { return nil }
        return (id, width * height)
    }
    .max { $0.1 < $1.1 }
if let (id, _) = best {
    print(id)
} else {
    exit(1)
}

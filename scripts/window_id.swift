// Prints the window number of the "Tempo Sampler" window owned by the given
// process ID, for `screencapture -l`. Works even when the window isn't on
// screen (e.g. behind other windows or with the display locked).
import CoreGraphics
import Foundation

let pid = Int32(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]]
for w in windows where (w[kCGWindowOwnerPID as String] as? Int32) == pid {
    let bounds = w[kCGWindowBounds as String] as? [String: Any]
    let width = (bounds?["Width"] as? Double) ?? 0
    if let n = w[kCGWindowNumber as String] as? Int, width > 0 {
        print(n)
        exit(0)
    }
}
exit(1)

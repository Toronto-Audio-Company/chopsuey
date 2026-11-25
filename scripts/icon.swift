// Finder icons.
//   swift scripts/icon.swift set <icns> <path>   gives a folder a custom icon
//   swift scripts/icon.swift get <path> <png>    saves the icon Finder shows
import AppKit

let args = CommandLine.arguments
switch args.count == 4 ? args[1] : "" {
case "set":
    guard let image = NSImage(contentsOfFile: args[2]),
          NSWorkspace.shared.setIcon(image, forFile: args[3], options: []) else { exit(1) }
case "get":
    let image = NSWorkspace.shared.icon(forFile: args[2])
    image.size = NSSize(width: 256, height: 256)
    guard let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
    try! png.write(to: URL(fileURLWithPath: args[3]))
default:
    FileHandle.standardError.write("usage: icon.swift set <icns> <path> | get <path> <png>\n".data(using: .utf8)!)
    exit(2)
}

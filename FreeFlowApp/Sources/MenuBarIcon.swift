import AppKit

/// Draw the FreeFlow waveform as a monochrome macOS menu bar icon.
@MainActor
enum MenuBarIcon {
    static let image: NSImage = {
        let image = NSImage(
            size: NSSize(width: 18, height: 18),
            flipped: false
        ) { _ in
            let barHeights: [CGFloat] = [5, 9, 13, 8, 11, 6]
            let centerY: CGFloat = 9

            NSColor.black.setStroke()

            for (index, height) in barHeights.enumerated() {
                let x = 2.25 + CGFloat(index) * 2.7
                let bar = NSBezierPath()
                bar.move(to: NSPoint(x: x, y: centerY - height / 2))
                bar.line(to: NSPoint(x: x, y: centerY + height / 2))
                bar.lineWidth = 1.75
                bar.lineCapStyle = .round
                bar.stroke()
            }

            return true
        }

        image.isTemplate = true
        image.accessibilityDescription = "FreeFlow"
        return image
    }()
}

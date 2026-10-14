import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// High-performance 120fps SwiftUI Code Editor view rendering `ViewportViewModel.visibleLines`
/// with line numbers, Git diff gutter markers, syntax-colored spans, active cursors,
/// iPhone `InputAccessoryView` programming toolbar, and iPad `UIKeyCommand` hardware shortcuts.
public struct CodeEditorView: View {
    public let viewport: ViewportViewModel
    public let onDispatch: (EditorAction) -> Void

    private let lineHeight: CGFloat = 22.0
    private let gutterWidth: CGFloat = 54.0
    private let charWidth: CGFloat = 8.4

    public init(viewport: ViewportViewModel, onDispatch: @escaping (EditorAction) -> Void) {
        self.viewport = viewport
        self.onDispatch = onDispatch
    }

    public var body: some View {
        VStack(spacing: 0) {
            GeometryReader { geo in
                Canvas { context, size in
                    // 1. Editor & Gutter Backgrounds
                    context.fill(
                        Path(CGRect(origin: .zero, size: size)),
                        with: .color(Color(red: 0.05, green: 0.07, blue: 0.09))
                    )
                    context.fill(
                        Path(CGRect(x: 0, y: 0, width: gutterWidth, height: size.height)),
                        with: .color(Color(red: 0.09, green: 0.11, blue: 0.13))
                    )

                    // 2. Visible Lines + Git Gutter + Syntax Spans
                    for (idx, line) in viewport.visibleLines.enumerated() {
                        let y = CGFloat(idx) * lineHeight
                        if y > size.height { break }

                        let lineNumText = Text(String(format: "%4d", line.lineNumber + 1))
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundColor(Color(red: 0.43, green: 0.46, blue: 0.51))
                        context.draw(lineNumText, at: CGPoint(x: 24, y: y + lineHeight * 0.5))

                        if let marker = viewport.gitDiffMarkers.first(where: { $0.lineNumber == line.lineNumber }) {
                            let markerColor: Color = switch marker.status {
                            case .added: Color(red: 0.18, green: 0.63, blue: 0.26)
                            case .modified: Color(red: 0.35, green: 0.65, blue: 1.0)
                            case .deleted: Color(red: 0.97, green: 0.32, blue: 0.29)
                            case .unchanged: .clear
                            }
                            context.fill(
                                Path(CGRect(x: gutterWidth - 4, y: y, width: 3, height: lineHeight)),
                                with: .color(markerColor)
                            )
                        }

                        let attributedLine = buildAttributedLine(line)
                        context.draw(
                            Text(attributedLine).font(.system(size: 14, design: .monospaced)),
                            at: CGPoint(x: gutterWidth + 8, y: y + 2),
                            anchor: .topLeading
                        )
                    }

                    // 3. Cursors
                    for cursor in viewport.cursors where cursor.line >= viewport.firstVisibleLine {
                        let relRow = CGFloat(cursor.line - viewport.firstVisibleLine)
                        let cy = relRow * lineHeight
                        let cx = gutterWidth + 8 + CGFloat(cursor.column) * charWidth
                        let color = cursor.isPrimary
                            ? Color(red: 0.35, green: 0.65, blue: 1.0)
                            : Color(red: 0.82, green: 0.66, blue: 1.0)
                        context.fill(
                            Path(CGRect(x: cx, y: cy + 2, width: 2, height: lineHeight - 4)),
                            with: .color(color)
                        )
                    }
                }
                .gesture(
                    DragGesture(minimumDistance: 0)
                        .onEnded { value in
                            if abs(value.translation.height) < 6 && abs(value.translation.width) < 6 {
                                let rowOffset = UInt32(max(0, Int(value.location.y / lineHeight)))
                                let targetLine = viewport.firstVisibleLine + rowOffset
                                let targetCol = UInt32(max(0, Int(((value.location.x - gutterWidth - 8) / charWidth).rounded())))
                                onDispatch(
                                    .setCursorPosition(
                                        bufferId: viewport.bufferId,
                                        line: targetLine,
                                        column: targetCol,
                                        select: false
                                    )
                                )
                            } else {
                                let deltaLines = Int((-value.translation.height / lineHeight).rounded())
                                let nextFirst = UInt32(max(0, Int(viewport.firstVisibleLine) + deltaLines))
                                let visibleCount = UInt32(max(15, Int(geo.size.height / lineHeight)))
                                onDispatch(
                                    .scrollViewport(
                                        bufferId: viewport.bufferId,
                                        firstVisibleLine: nextFirst,
                                        visibleLineCount: visibleCount
                                    )
                                )
                            }
                        }
                )
            }

            // 4. Native iOS InputAccessoryView + iPad UIKeyCommand hardware bridge
            #if canImport(UIKit)
            HardwareAndAccessoryInputBridge(
                bufferId: viewport.bufferId,
                onDispatch: onDispatch
            )
            .frame(height: 0)
            #endif

            // 5. Persistent touch programming shortcut bar
            ProgrammingKeyBarView(bufferId: viewport.bufferId, onDispatch: onDispatch)
        }
    }

    private func buildAttributedLine(_ line: RenderedLine) -> AttributedString {
        var attr = AttributedString(line.text)
        attr.foregroundColor = Color(red: 0.90, green: 0.93, blue: 0.95)
        let chars = Array(line.text)
        for span in line.spans {
            let s = min(Int(span.startCol), chars.count)
            let e = min(max(s, Int(span.endCol)), chars.count)
            guard s < e else { continue }
            let startIdx = attr.characters.index(attr.startIndex, offsetBy: s)
            let endIdx = attr.characters.index(attr.startIndex, offsetBy: e)
            let rgba = span.colorRgba
            let r = Double((rgba >> 24) & 0xFF) / 255.0
            let g = Double((rgba >> 16) & 0xFF) / 255.0
            let b = Double((rgba >> 8) & 0xFF) / 255.0
            let a = Double(rgba & 0xFF) / 255.0
            attr[startIdx..<endIdx].foregroundColor = Color(.sRGB, red: r, green: g, blue: b, opacity: a)
        }
        return attr
    }
}

struct ProgrammingKeyBarView: View {
    let bufferId: UInt64
    let onDispatch: (EditorAction) -> Void

    private let keys: [(String, String)] = [
        ("Tab", "    "),
        ("{", "{"),
        ("}", "}"),
        ("(", "("),
        (")", ")"),
        (";", ";")
    ]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(keys, id: \.0) { label, insertion in
                    Button(label) {
                        onDispatch(.insertText(bufferId: bufferId, text: insertion))
                    }
                    .buttonStyle(.bordered)
                    .font(.system(size: 13, design: .monospaced))
                }
                Button("Esc") {
                    onDispatch(.setCursorPosition(bufferId: bufferId, line: 0, column: 0, select: false))
                }
                .buttonStyle(.bordered)
                Button("Undo") {
                    onDispatch(.undo(bufferId: bufferId))
                }
                .buttonStyle(.bordered)
                Button("Redo") {
                    onDispatch(.redo(bufferId: bufferId))
                }
                .buttonStyle(.bordered)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
        .background(Color(red: 0.09, green: 0.11, blue: 0.13))
    }
}

#if canImport(UIKit)
/// UIKit responder bridge providing:
/// - `inputAccessoryView` programming shortcut bar above the iPhone/iPad virtual keyboard
/// - `UIKeyCommand` hardware keyboard shortcuts (`Cmd+S`, `Cmd+Z`, `Cmd+Shift+Z`, arrow navigation) on iPad
struct HardwareAndAccessoryInputBridge: UIViewRepresentable {
    let bufferId: UInt64
    let onDispatch: (EditorAction) -> Void

    func makeUIView(context: Context) -> KeyCommandResponderView {
        let view = KeyCommandResponderView()
        view.bufferId = bufferId
        view.onDispatch = onDispatch
        return view
    }

    func updateUIView(_ uiView: KeyCommandResponderView, context: Context) {
        uiView.bufferId = bufferId
        uiView.onDispatch = onDispatch
    }
}

final class KeyCommandResponderView: UIView {
    var bufferId: UInt64 = 1
    var onDispatch: ((EditorAction) -> Void)?

    override var canBecomeFirstResponder: Bool { true }

    override var inputAccessoryView: UIView? {
        let toolbar = UIToolbar(frame: CGRect(x: 0, y: 0, width: UIScreen.main.bounds.width, height: 44))
        toolbar.barStyle = .black
        let titles = ["Tab", "{", "}", "(", ")", ";", "Undo", "Redo"]
        var items: [UIBarButtonItem] = []
        for title in titles {
            let action = UIAction(title: title) { [weak self] _ in
                guard let self else { return }
                switch title {
                case "Tab": self.onDispatch?(.insertText(bufferId: self.bufferId, text: "    "))
                case "Undo": self.onDispatch?(.undo(bufferId: self.bufferId))
                case "Redo": self.onDispatch?(.redo(bufferId: self.bufferId))
                default: self.onDispatch?(.insertText(bufferId: self.bufferId, text: title))
                }
            }
            items.append(UIBarButtonItem( primaryAction: action ))
        }
        toolbar.items = items
        return toolbar
    }

    override var keyCommands: [UIKeyCommand]? {
        [
            UIKeyCommand(title: "Save File", action: #selector(handleSave), input: "s", modifierFlags: .command),
            UIKeyCommand(title: "Undo", action: #selector(handleUndo), input: "z", modifierFlags: .command),
            UIKeyCommand(title: "Redo", action: #selector(handleRedo), input: "z", modifierFlags: [.command, .shift]),
            UIKeyCommand(title: "Trigger Completion", action: #selector(handleCompletion), input: " ", modifierFlags: .control),
            UIKeyCommand(title: "Move Up", action: #selector(handleUp), input: UIKeyCommand.inputUpArrow, modifierFlags: []),
            UIKeyCommand(title: "Move Down", action: #selector(handleDown), input: UIKeyCommand.inputDownArrow, modifierFlags: []),
            UIKeyCommand(title: "Move Left", action: #selector(handleLeft), input: UIKeyCommand.inputLeftArrow, modifierFlags: []),
            UIKeyCommand(title: "Move Right", action: #selector(handleRight), input: UIKeyCommand.inputRightArrow, modifierFlags: [])
        ]
    }

    @objc private func handleSave() {
        onDispatch?(.saveFile(bufferId: bufferId))
    }

    @objc private func handleUndo() {
        onDispatch?(.undo(bufferId: bufferId))
    }

    @objc private func handleRedo() {
        onDispatch?(.redo(bufferId: bufferId))
    }

    @objc private func handleCompletion() {
        onDispatch?(.triggerLspCompletion(bufferId: bufferId))
    }

    @objc private func handleUp() {
        onDispatch?(.moveCursor(bufferId: bufferId, direction: .up, select: false))
    }

    @objc private func handleDown() {
        onDispatch?(.moveCursor(bufferId: bufferId, direction: .down, select: false))
    }

    @objc private func handleLeft() {
        onDispatch?(.moveCursor(bufferId: bufferId, direction: .left, select: false))
    }

    @objc private func handleRight() {
        onDispatch?(.moveCursor(bufferId: bufferId, direction: .right, select: false))
    }
}
#endif

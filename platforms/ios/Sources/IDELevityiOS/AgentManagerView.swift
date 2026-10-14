import SwiftUI

/// SwiftUI view for managing parallel AI Agent tasks and inspecting Unified Diff previews on iPhone & iPad.
public struct AgentManagerView: View {
    public let viewport: ViewportViewModel
    public let onSubmitPrompt: (String) -> Void
    public let onAcceptTask: (UInt64) -> Void
    public let onRejectTask: (UInt64) -> Void

    @State private var promptInput: String = ""

    public init(
        viewport: ViewportViewModel,
        onSubmitPrompt: @escaping (String) -> Void,
        onAcceptTask: @escaping (UInt64) -> Void,
        onRejectTask: @escaping (UInt64) -> Void
    ) {
        self.viewport = viewport
        self.onSubmitPrompt = onSubmitPrompt
        self.onAcceptTask = onAcceptTask
        self.onRejectTask = onRejectTask
    }

    public var body: some View {
        VStack(spacing: 12) {
            HStack {
                TextField("AI Agent prompt (e.g. old_name -> new_name)...", text: $promptInput)
                    .textFieldStyle(.roundedBorder)
                Button("Dispatch") {
                    let trimmed = promptInput.trimmingCharacters(in: .whitespacesAndNewlines)
                    if !trimmed.isEmpty {
                        onSubmitPrompt(trimmed)
                        promptInput = ""
                    }
                }
                .buttonStyle(.borderedProminent)
            }
            .padding(.horizontal)

            List(viewport.agentTasks, id: \.taskId) { task in
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        Text("Task #\(task.taskId): \(task.prompt)")
                            .font(.headline)
                        Spacer()
                        Text(task.verificationPassed ? "✓ Verified" : task.state)
                            .font(.caption)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 2)
                            .background(Color.blue.opacity(0.2))
                            .clipShape(Capsule())
                    }

                    if let diff = task.unifiedDiffPreview {
                        ScrollView(.horizontal, showsIndicators: true) {
                            VStack(alignment: .leading, spacing: 2) {
                                ForEach(Array(diff.components(separatedBy: "\n").enumerated()), id: \.offset) { _, line in
                                    Text(line)
                                        .font(.system(size: 12, design: .monospaced))
                                        .foregroundColor(diffLineColor(line))
                                }
                            }
                            .padding(8)
                        }
                        .background(Color(red: 0.09, green: 0.11, blue: 0.13))
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                    }

                    HStack(spacing: 12) {
                        Button("Accept (Chấp nhận)") {
                            onAcceptTask(task.taskId)
                        }
                        .buttonStyle(.borderedProminent)
                        .tint(.green)

                        Button("Reject (Từ chối)") {
                            onRejectTask(task.taskId)
                        }
                        .buttonStyle(.bordered)
                        .tint(.red)
                    }
                }
                .padding(.vertical, 4)
            }
        }
    }

    private func diffLineColor(_ line: String) -> Color {
        if line.hasPrefix("+") && !line.hasPrefix("+++") {
            return .green
        } else if line.hasPrefix("-") && !line.hasPrefix("---") {
            return .red
        } else if line.hasPrefix("@@") {
            return .cyan
        } else {
            return .primary
        }
    }
}

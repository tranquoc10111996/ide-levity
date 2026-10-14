import Foundation
import SwiftUI

/// Main Observable state coordinator bridging SwiftUI on iPhone & iPad with Rust `FfiWorkspace`.
@MainActor
public final class WorkspaceViewModel: ObservableObject {
    private let workspace: FfiWorkspace
    @Published public var viewport: ViewportViewModel
    @Published public var workspaceFiles: [String] = []
    @Published public var gitBranch: String = "main"
    @Published public var selectedScreen: IOSScreenMode = .editor

    public enum IOSScreenMode: String, CaseIterable, Identifiable {
        case editor = "Editor"
        case agentManager = "Agents"
        public var id: String { rawValue }
    }

    public init(rootPath: String = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first?.path ?? ".") {
        let ws = FfiWorkspace(rootDir: rootPath)
        self.workspace = ws
        self.viewport = (try? ws.activeViewport()) ?? ViewportViewModel(
            bufferId: 1,
            filePath: "welcome.rs",
            isDirty: false,
            totalLines: 1,
            firstVisibleLine: 0,
            visibleLines: [],
            cursors: [],
            selections: [],
            diagnostics: [],
            gitDiffMarkers: [],
            agentTasks: []
        )
        self.workspaceFiles = ws.listWorkspaceFiles()
        self.gitBranch = ws.currentGitBranch() ?? "main"
    }

    public func dispatch(_ action: EditorAction) {
        if let updated = try? workspace.dispatch(action: action) {
            self.viewport = updated
            self.workspaceFiles = workspace.listWorkspaceFiles()
            self.gitBranch = workspace.currentGitBranch() ?? "main"
        }
    }

    public func acceptAgentTask(_ taskId: UInt64) {
        if let updated = try? workspace.acceptAgentTask(taskId: taskId, bufferId: viewport.bufferId) {
            self.viewport = updated
        }
    }

    public func rejectAgentTask(_ taskId: UInt64) {
        if let updated = try? workspace.rejectAgentTask(taskId: taskId, bufferId: viewport.bufferId) {
            self.viewport = updated
        }
    }
}

/// Adaptive root view:
/// - **iPad (`horizontalSizeClass == .regular`)**: `NavigationSplitView` with persistent file explorer sidebar.
/// - **iPhone (`horizontalSizeClass == .compact`)**: `NavigationStack` with compact file sheet & bottom `InputAccessoryView`.
public struct IDELevityRootView: View {
    @StateObject private var model = WorkspaceViewModel()
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @State private var showingFileSheet = false

    public init() {}

    public var body: some View {
        Group {
            if horizontalSizeClass == .regular {
                // iPad Layout: NavigationSplitView
                NavigationSplitView {
                    FileExplorerListView(
                        files: model.workspaceFiles,
                        activeFilePath: model.viewport.filePath,
                        gitBranch: model.gitBranch,
                        onSelectFile: { path in
                            model.dispatch(.openFile(path: path))
                        }
                    )
                    .navigationTitle("Explorer")
                } detail: {
                    detailContent
                }
            } else {
                // iPhone Layout: NavigationStack
                NavigationStack {
                    detailContent
                        .toolbar {
                            ToolbarItem(placement: .topBarLeading) {
                                Button("Files") {
                                    showingFileSheet = true
                                }
                            }
                        }
                        .sheet(isPresented: $showingFileSheet) {
                            NavigationStack {
                                FileExplorerListView(
                                    files: model.workspaceFiles,
                                    activeFilePath: model.viewport.filePath,
                                    gitBranch: model.gitBranch,
                                    onSelectFile: { path in
                                        model.dispatch(.openFile(path: path))
                                        showingFileSheet = false
                                    }
                                )
                                .navigationTitle("Files (\(model.gitBranch))")
                            }
                        }
                }
            }
        }
        .preferredColorScheme(.dark)
    }

    @ViewBuilder
    private var detailContent: some View {
        VStack(spacing: 0) {
            Picker("Mode", selection: $model.selectedScreen) {
                ForEach(WorkspaceViewModel.IOSScreenMode.allCases) { mode in
                    Text(mode.rawValue).tag(mode)
                }
            }
            .pickerStyle(.segmented)
            .padding(8)

            switch model.selectedScreen {
            case .editor:
                CodeEditorView(
                    viewport: model.viewport,
                    onDispatch: { action in
                        model.dispatch(action)
                    }
                )
            case .agentManager:
                AgentManagerView(
                    viewport: model.viewport,
                    onSubmitPrompt: { prompt in
                        model.dispatch(.submitAgentPrompt(prompt: prompt))
                    },
                    onAcceptTask: { id in
                        model.acceptAgentTask(id)
                    },
                    onRejectTask: { id in
                        model.rejectAgentTask(id)
                    }
                )
            }
        }
        .navigationTitle(model.viewport.filePath + (model.viewport.isDirty ? " ●" : ""))
    }
}

struct FileExplorerListView: View {
    let files: [String]
    let activeFilePath: String
    let gitBranch: String
    let onSelectFile: (String) -> Void

    var body: some View {
        List(files, id: \.self) { file in
            Button {
                onSelectFile(file)
            } label: {
                HStack {
                    Image(systemName: "doc.text")
                    Text(file)
                        .font(.system(.subheadline, design: .monospaced))
                    Spacer()
                    if file == activeFilePath {
                        Image(systemName: "checkmark.circle.fill")
                            .foregroundStyle(.blue)
                    }
                }
            }
        }
    }
}

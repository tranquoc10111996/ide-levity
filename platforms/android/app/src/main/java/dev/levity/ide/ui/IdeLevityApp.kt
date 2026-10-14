package dev.levity.ide.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.NavigationDrawerItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.rememberDrawerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import uniffi.ide_ffi.EditorAction
import uniffi.ide_ffi.FfiWorkspace
import uniffi.ide_ffi.ViewportViewModel

enum class AndroidScreenMode {
    EDITOR,
    AGENT_MANAGER
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun IdeLevityApp(
    workspace: FfiWorkspace,
    modifier: Modifier = Modifier
) {
    val drawerState = rememberDrawerState(initialValue = DrawerValue.Closed)
    val scope = rememberCoroutineScope()

    var screenMode by remember { mutableStateOf(AndroidScreenMode.EDITOR) }
    var viewport: ViewportViewModel by remember {
        mutableStateOf(workspace.activeViewport())
    }
    val files: List<String> = remember(viewport.filePath) {
        workspace.listWorkspaceFiles()
    }
    val gitBranch: String = remember(viewport.filePath) {
        workspace.currentGitBranch() ?: "main"
    }

    val dispatchAction: (EditorAction) -> Unit = { action ->
        viewport = workspace.dispatch(action)
    }

    ModalNavigationDrawer(
        drawerState = drawerState,
        drawerContent = {
            ModalDrawerSheet {
                Text(
                    text = "EXPLORER ($gitBranch)",
                    modifier = Modifier.padding(16.dp)
                )
                HorizontalDivider()
                LazyColumn(modifier = Modifier.fillMaxWidth()) {
                    items(files) { filePath ->
                        NavigationDrawerItem(
                            label = { Text(text = filePath) },
                            selected = filePath == viewport.filePath,
                            onClick = {
                                dispatchAction(EditorAction.OpenFile(path = filePath))
                                scope.launch { drawerState.close() }
                            }
                        )
                    }
                }
            }
        },
        modifier = modifier.fillMaxSize()
    ) {
        Scaffold(
            topBar = {
                TopAppBar(
                    title = {
                        val dirtyMark = if (viewport.isDirty) " ●" else ""
                        Text(text = "${viewport.filePath}$dirtyMark")
                    },
                    navigationIcon = {
                        TextButton(onClick = {
                            scope.launch {
                                if (drawerState.isClosed) drawerState.open() else drawerState.close()
                            }
                        }) {
                            Text("Files")
                        }
                    },
                    actions = {
                        Row {
                            TextButton(onClick = { screenMode = AndroidScreenMode.EDITOR }) {
                                Text("Editor")
                            }
                            Spacer(modifier = Modifier.width(4.dp))
                            TextButton(onClick = { screenMode = AndroidScreenMode.AGENT_MANAGER }) {
                                Text("Agents (${viewport.agentTasks.size})")
                            }
                        }
                    }
                )
            }
        ) { innerPadding ->
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(innerPadding)
            ) {
                when (screenMode) {
                    AndroidScreenMode.EDITOR -> {
                        CodeEditorComposable(
                            viewport = viewport,
                            onDispatch = dispatchAction,
                            modifier = Modifier.fillMaxSize()
                        )
                    }
                    AndroidScreenMode.AGENT_MANAGER -> {
                        AgentManagerScreen(
                            viewport = viewport,
                            onSubmitPrompt = { prompt ->
                                dispatchAction(EditorAction.SubmitAgentPrompt(prompt = prompt))
                            },
                            onAcceptTask = { taskId ->
                                viewport = workspace.acceptAgentTask(taskId, viewport.bufferId)
                            },
                            onRejectTask = { taskId ->
                                viewport = workspace.rejectAgentTask(taskId, viewport.bufferId)
                            },
                            modifier = Modifier.fillMaxSize()
                        )
                    }
                }
            }
        }
    }
}

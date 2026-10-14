package dev.levity.ide.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.PrimaryScrollableTabRow
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.rememberDrawerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.levity.ide.IdeUiState
import dev.levity.ide.IdeViewModel
import dev.levity.ide.ScreenMode
import kotlinx.coroutines.launch
import uniffi.ide_ffi.DirEntryInfo
import uniffi.ide_ffi.EditorAction

/** A pending dialog that asks the user for a name or a confirmation. */
private sealed interface PendingDialog {
    data object SaveAs : PendingDialog
    data object NewFile : PendingDialog
    data object NewFolder : PendingDialog
    data class Rename(val entry: DirEntryInfo) : PendingDialog
    data class Delete(val entry: DirEntryInfo) : PendingDialog
}

/** Root composable: explorer drawer, tabs, top bar, and the Editor / Agent Manager screens. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun IdeLevityApp(viewModel: IdeViewModel, modifier: Modifier = Modifier) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    val drawerState = rememberDrawerState(initialValue = DrawerValue.Closed)
    val scope = rememberCoroutineScope()
    val snackbarHostState = remember { SnackbarHostState() }
    val clipboardManager = LocalClipboardManager.current
    var dialog by remember { mutableStateOf<PendingDialog?>(null) }
    val viewport = state.viewport
    val activeBuffer = state.activeBuffer

    LaunchedEffect(state.errorMessage) {
        val message = state.errorMessage ?: return@LaunchedEffect
        snackbarHostState.showSnackbar(message)
        viewModel.consumeError()
    }

    val onClipboard: (ClipboardCommand) -> Unit = { command ->
        when (command) {
            ClipboardCommand.COPY, ClipboardCommand.CUT -> scope.launch {
                val payload = viewModel.copySelection(cut = command == ClipboardCommand.CUT)
                if (payload != null && payload.text.isNotEmpty()) {
                    clipboardManager.setText(AnnotatedString(payload.text))
                }
            }
            ClipboardCommand.PASTE -> clipboardManager.getText()?.text?.let(viewModel::paste)
        }
    }

    val save: () -> Unit = {
        if (viewport != null) {
            if (activeBuffer != null && activeBuffer.path == null) {
                dialog = PendingDialog.SaveAs
            } else {
                viewModel.dispatch(EditorAction.SaveFile(bufferId = viewport.bufferId))
            }
        }
    }

    val explorerActions = remember(viewModel) {
        object : ExplorerActions {
            override fun openFile(path: String) {
                viewModel.dispatch(EditorAction.OpenFile(path = path))
                scope.launch { drawerState.close() }
            }
            override fun toggleDirectory(path: String) = viewModel.toggleDirectory(path)
            override fun requestNewFile() { dialog = PendingDialog.NewFile }
            override fun requestNewFolder() { dialog = PendingDialog.NewFolder }
            override fun requestRename(entry: DirEntryInfo) { dialog = PendingDialog.Rename(entry) }
            override fun requestDelete(entry: DirEntryInfo) { dialog = PendingDialog.Delete(entry) }
        }
    }

    ModalNavigationDrawer(
        drawerState = drawerState,
        drawerContent = {
            ModalDrawerSheet {
                ExplorerPanel(
                    rootDir = state.rootDir,
                    gitBranch = state.gitBranch,
                    listings = state.directoryListings,
                    expanded = state.expandedDirectories,
                    activePath = activeBuffer?.path,
                    actions = explorerActions,
                )
            }
        },
        modifier = modifier.fillMaxSize(),
    ) {
        Scaffold(
            snackbarHost = { SnackbarHost(snackbarHostState) },
            topBar = {
                Column {
                    TopAppBar(
                        title = {
                            val title = activeBuffer?.title ?: "IDE Levity"
                            val dirty = if (activeBuffer?.isDirty == true) " \u2022" else ""
                            Text(text = title + dirty, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        },
                        navigationIcon = {
                            TextButton(onClick = {
                                scope.launch { if (drawerState.isClosed) drawerState.open() else drawerState.close() }
                            }) { Text("Files") }
                        },
                        actions = {
                            TextButton(onClick = viewModel::newUntitledBuffer) { Text("New") }
                            TextButton(onClick = save, enabled = activeBuffer?.isDirty == true) { Text("Save") }
                            val editorSelected = state.screenMode == ScreenMode.EDITOR
                            TextButton(onClick = {
                                viewModel.setScreenMode(if (editorSelected) ScreenMode.AGENT_MANAGER else ScreenMode.EDITOR)
                            }) {
                                Text(if (editorSelected) "Agents (${viewport?.agentTasks?.size ?: 0})" else "Editor")
                            }
                        },
                    )
                    if (state.screenMode == ScreenMode.EDITOR && state.openBuffers.isNotEmpty()) {
                        EditorTabs(state = state, onSelect = viewModel::selectBuffer, onClose = viewModel::closeBuffer)
                    }
                }
            },
        ) { innerPadding ->
            Box(modifier = Modifier.fillMaxSize().padding(innerPadding)) {
                if (viewport == null) {
                    CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
                    return@Box
                }
                when (state.screenMode) {
                    ScreenMode.EDITOR -> CodeEditorComposable(
                        viewport = viewport,
                        onDispatch = { action ->
                            if (action is EditorAction.SaveFile) save() else viewModel.dispatch(action)
                        },
                        onClipboard = onClipboard,
                        modifier = Modifier.fillMaxSize(),
                    )
                    ScreenMode.AGENT_MANAGER -> AgentManagerScreen(
                        viewport = viewport,
                        onSubmitPrompt = { prompt -> viewModel.dispatch(EditorAction.SubmitAgentPrompt(prompt = prompt)) },
                        onAcceptTask = viewModel::acceptAgentTask,
                        onRejectTask = viewModel::rejectAgentTask,
                        modifier = Modifier.fillMaxSize(),
                    )
                }
            }
        }
    }

    when (val pending = dialog) {
        null -> Unit
        PendingDialog.SaveAs -> NameDialog(
            title = "Save as",
            label = "Path relative to workspace",
            initial = activeBuffer?.title?.takeIf { it.contains('.') } ?: "untitled.txt",
            confirmLabel = "Save",
            onDismiss = { dialog = null },
            onConfirm = { name -> dialog = null; viewModel.saveActiveBufferAs(name) },
        )
        PendingDialog.NewFile -> NameDialog(
            title = "New file",
            label = "Path relative to workspace",
            initial = "",
            confirmLabel = "Create",
            onDismiss = { dialog = null },
            onConfirm = { name ->
                dialog = null
                viewModel.createFile(name)
                scope.launch { drawerState.close() }
            },
        )
        PendingDialog.NewFolder -> NameDialog(
            title = "New folder",
            label = "Path relative to workspace",
            initial = "",
            confirmLabel = "Create",
            onDismiss = { dialog = null },
            onConfirm = { name -> dialog = null; viewModel.createDirectory(name) },
        )
        is PendingDialog.Rename -> NameDialog(
            title = "Rename ${pending.entry.name}",
            label = "New name",
            initial = pending.entry.name,
            confirmLabel = "Rename",
            onDismiss = { dialog = null },
            onConfirm = { name -> dialog = null; viewModel.renamePath(pending.entry.path, name) },
        )
        is PendingDialog.Delete -> AlertDialog(
            onDismissRequest = { dialog = null },
            title = { Text("Delete ${pending.entry.name}?") },
            text = { Text(if (pending.entry.isDir) "The folder and all of its contents will be deleted." else "The file will be deleted.") },
            confirmButton = {
                TextButton(onClick = { dialog = null; viewModel.deletePath(pending.entry.path) }) {
                    Text("Delete", color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = { TextButton(onClick = { dialog = null }) { Text("Cancel") } },
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun EditorTabs(state: IdeUiState, onSelect: (ULong) -> Unit, onClose: (ULong) -> Unit) {
    val selectedIndex = state.openBuffers.indexOfFirst { it.bufferId == state.viewport?.bufferId }.coerceAtLeast(0)
    PrimaryScrollableTabRow(selectedTabIndex = selectedIndex, edgePadding = 8.dp) {
        state.openBuffers.forEachIndexed { index, tab ->
            Tab(
                selected = index == selectedIndex,
                onClick = { onSelect(tab.bufferId) },
                text = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(
                            text = tab.title + if (tab.isDirty) " \u2022" else "",
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        if (state.openBuffers.size > 1) {
                            TextButton(onClick = { onClose(tab.bufferId) }) { Text("\u00D7") }
                        }
                    }
                },
            )
        }
    }
}

@Composable
private fun NameDialog(
    title: String,
    label: String,
    initial: String,
    confirmLabel: String,
    onDismiss: () -> Unit,
    onConfirm: (String) -> Unit,
) {
    var value by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(value = value, onValueChange = { value = it }, label = { Text(label) }, singleLine = true)
        },
        confirmButton = {
            TextButton(onClick = { onConfirm(value) }, enabled = value.isNotBlank()) { Text(confirmLabel) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

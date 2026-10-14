package dev.levity.ide

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.ExecutorCoroutineDispatcher
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.ide_ffi.ClipboardPayload
import uniffi.ide_ffi.CursorDirection
import uniffi.ide_ffi.DirEntryInfo
import uniffi.ide_ffi.EditorAction
import uniffi.ide_ffi.FfiException
import uniffi.ide_ffi.FfiWorkspace
import uniffi.ide_ffi.OpenBufferInfo
import uniffi.ide_ffi.ViewportViewModel
import java.io.File
import java.util.concurrent.Executors

/** Which top level screen is shown (Editor or Agent Manager, mirroring the desktop dual view). */
enum class ScreenMode { EDITOR, AGENT_MANAGER }

/** Immutable UI state rendered by Compose. */
data class IdeUiState(
    val viewport: ViewportViewModel? = null,
    val openBuffers: List<OpenBufferInfo> = emptyList(),
    val rootDir: String = "",
    /** Explorer listings keyed by absolute directory path ([rootDir] for the root level). */
    val directoryListings: Map<String, List<DirEntryInfo>> = emptyMap(),
    val expandedDirectories: Set<String> = emptySet(),
    val gitBranch: String? = null,
    val screenMode: ScreenMode = ScreenMode.EDITOR,
    val errorMessage: String? = null,
) {
    /** Tab info of the active buffer, if any. */
    val activeBuffer: OpenBufferInfo?
        get() = viewport?.let { vm -> openBuffers.firstOrNull { it.bufferId == vm.bufferId } }
}

/**
 * Owns the Rust [FfiWorkspace] for the lifetime of the app UI (survives configuration changes)
 * and implements the unidirectional data flow: Compose sends [EditorAction]s, the Rust core
 * returns a fresh [ViewportViewModel], which is published through [state].
 *
 * Every FFI call runs on one dedicated background thread. This keeps the main thread free
 * (file IO, parsing and git work happen inside the core) and guarantees that actions are
 * applied in exactly the order the user produced them.
 */
class IdeViewModel(application: Application) : AndroidViewModel(application) {
    private val ffiThread: ExecutorCoroutineDispatcher =
        Executors.newSingleThreadExecutor { runnable ->
            Thread(runnable, "ide-ffi").apply { isDaemon = true }
        }.asCoroutineDispatcher()

    private val workspace = CompletableDeferred<FfiWorkspace>()

    /** Only read and written on [ffiThread]. */
    private var ownedWorkspace: FfiWorkspace? = null

    /** Text of the last whole-line copy, so pasting it inserts a full line like desktop editors. */
    @Volatile
    private var lastWholeLineCopy: String? = null

    private val _state = MutableStateFlow(IdeUiState())

    /** Current UI state. */
    val state: StateFlow<IdeUiState> = _state.asStateFlow()

    init {
        viewModelScope.launch(ffiThread) {
            try {
                val root = File(application.filesDir, "workspace").apply { mkdirs() }
                val ws = FfiWorkspace(root.absolutePath)
                ownedWorkspace = ws
                workspace.complete(ws)
                val rootDir = ws.rootDir()
                val viewport = ws.activeViewport()
                _state.update {
                    it.copy(
                        viewport = viewport,
                        openBuffers = ws.openBuffers(),
                        rootDir = rootDir,
                        directoryListings = mapOf(rootDir to ws.listDirectory("")),
                        gitBranch = ws.currentGitBranch(),
                    )
                }
            } catch (e: FfiException) {
                reportError(e)
            } catch (e: LinkageError) {
                // libide_ffi.so or libjnidispatch.so missing for this ABI.
                _state.update { it.copy(errorMessage = "Native core failed to load: ${e.message}") }
            } catch (e: RuntimeException) {
                // UniFFI contract/checksum mismatch between the bindings and the native library.
                _state.update { it.copy(errorMessage = "Native core initialization failed: ${e.message}") }
            }
        }
    }

    /** Dispatches an editor action to the Rust core. */
    fun dispatch(action: EditorAction) {
        runOnCore { ws -> publish(ws, ws.dispatch(action), refreshExplorer = action is EditorAction.SaveFile) }
    }

    /** Activates an open tab. */
    fun selectBuffer(bufferId: ULong) = runOnCore { ws -> publish(ws, ws.setActiveBuffer(bufferId)) }

    /** Closes a tab (the core always keeps at least one buffer open). */
    fun closeBuffer(bufferId: ULong) = runOnCore { ws -> publish(ws, ws.closeBuffer(bufferId)) }

    /** Opens a new empty untitled buffer. */
    fun newUntitledBuffer() = runOnCore { ws -> publish(ws, ws.newUntitledBuffer()) }

    /** Saves the active buffer under [relativePath] inside the workspace root (Save As). */
    fun saveActiveBufferAs(relativePath: String) = runOnCore { ws ->
        val bufferId = _state.value.viewport?.bufferId ?: return@runOnCore
        publish(ws, ws.saveBufferAs(bufferId, resolve(ws, relativePath)), refreshExplorer = true)
    }

    /** Creates (and opens) a new file at [relativePath] inside the workspace root. */
    fun createFile(relativePath: String) = runOnCore { ws ->
        publish(ws, ws.createFile(resolve(ws, relativePath)), refreshExplorer = true)
    }

    /** Creates a new directory at [relativePath] inside the workspace root. */
    fun createDirectory(relativePath: String) = runOnCore { ws ->
        ws.createDir(resolve(ws, relativePath))
        refreshExplorer(ws)
    }

    /** Renames [path] (absolute) to [newName] in the same directory. */
    fun renamePath(path: String, newName: String) = runOnCore { ws ->
        val target = File(File(path).parentFile, newName.trim()).absolutePath
        ws.renamePath(path, target)
        publish(ws, ws.activeViewport(), refreshExplorer = true)
    }

    /** Deletes [path] (absolute file or directory). */
    fun deletePath(path: String) = runOnCore { ws ->
        ws.deletePath(path)
        _state.update { it.copy(expandedDirectories = it.expandedDirectories - path) }
        publish(ws, ws.activeViewport(), refreshExplorer = true)
    }

    /** Expands or collapses an explorer directory, loading its entries on first expansion. */
    fun toggleDirectory(path: String) = runOnCore { ws ->
        if (path in _state.value.expandedDirectories) {
            _state.update { it.copy(expandedDirectories = it.expandedDirectories - path) }
        } else {
            val entries = ws.listDirectory(path)
            _state.update {
                it.copy(
                    expandedDirectories = it.expandedDirectories + path,
                    directoryListings = it.directoryListings + (path to entries),
                )
            }
        }
    }

    /**
     * Returns the text to put on the clipboard for Copy (or Cut when [cut] is true, which also
     * removes the selection or current line). Runs after all previously queued actions.
     */
    suspend fun copySelection(cut: Boolean): ClipboardPayload? {
        val ws = workspace.await()
        return withContext(ffiThread) {
            val bufferId = _state.value.viewport?.bufferId ?: return@withContext null
            try {
                val payload = ws.clipboardText(bufferId)
                lastWholeLineCopy = if (payload.isWholeLine) payload.text else null
                if (cut) publish(ws, ws.dispatch(EditorAction.DeleteSelectionOrLine(bufferId)))
                payload
            } catch (e: FfiException) {
                reportError(e)
                null
            }
        }
    }

    /** Inserts clipboard [text]; a whole-line copy is pasted above the current line. */
    fun paste(text: String) = runOnCore { ws ->
        val bufferId = _state.value.viewport?.bufferId ?: return@runOnCore
        if (text.isNotEmpty() && text == lastWholeLineCopy) {
            ws.dispatch(EditorAction.CollapseCursors(bufferId))
            ws.dispatch(EditorAction.MoveCursor(bufferId, CursorDirection.LINE_START, false))
        }
        publish(ws, ws.dispatch(EditorAction.InsertText(bufferId, text)))
    }

    /** Applies the patch produced by agent task [taskId] to the active buffer. */
    fun acceptAgentTask(taskId: ULong) = runOnCore { ws ->
        val bufferId = _state.value.viewport?.bufferId ?: return@runOnCore
        publish(ws, ws.acceptAgentTask(taskId, bufferId))
    }

    /** Discards the patch produced by agent task [taskId]. */
    fun rejectAgentTask(taskId: ULong) = runOnCore { ws ->
        val bufferId = _state.value.viewport?.bufferId ?: return@runOnCore
        publish(ws, ws.rejectAgentTask(taskId, bufferId))
    }

    /** Switches between the Editor and Agent Manager screens. */
    fun setScreenMode(mode: ScreenMode) {
        _state.update { it.copy(screenMode = mode) }
    }

    /** Clears the currently displayed error after the UI has shown it. */
    fun consumeError() {
        _state.update { it.copy(errorMessage = null) }
    }

    private fun resolve(ws: FfiWorkspace, relativePath: String): String =
        File(ws.rootDir(), relativePath.trim().trimStart('/')).absolutePath

    /** Publishes a new viewport plus the cheap derived data (tabs). Runs on [ffiThread]. */
    private fun publish(ws: FfiWorkspace, viewport: ViewportViewModel, refreshExplorer: Boolean = false) {
        val tabs = ws.openBuffers()
        _state.update { it.copy(viewport = viewport, openBuffers = tabs) }
        if (refreshExplorer) refreshExplorer(ws)
    }

    /** Reloads every loaded explorer level, dropping directories that no longer exist. */
    private fun refreshExplorer(ws: FfiWorkspace) {
        val current = _state.value
        val listings = LinkedHashMap<String, List<DirEntryInfo>>()
        listings[current.rootDir] = ws.listDirectory("")
        for (dir in current.expandedDirectories) {
            if (File(dir).isDirectory) listings[dir] = ws.listDirectory(dir)
        }
        _state.update {
            it.copy(
                directoryListings = listings,
                expandedDirectories = it.expandedDirectories.filterTo(HashSet()) { dir -> dir in listings },
                gitBranch = ws.currentGitBranch(),
            )
        }
    }

    private fun runOnCore(block: (FfiWorkspace) -> Unit) {
        viewModelScope.launch(ffiThread) {
            val ws = workspace.await()
            try {
                block(ws)
            } catch (e: FfiException) {
                reportError(e)
            }
        }
    }

    private fun reportError(e: FfiException) {
        val message = when (e) {
            is FfiException.Workspace -> e.message
        }
        _state.update { it.copy(errorMessage = message) }
    }

    override fun onCleared() {
        super.onCleared()
        // The executor is FIFO: the Rust object is destroyed after every queued action has run,
        // then the thread exits once the queue is drained (close() performs a graceful shutdown).
        ffiThread.executor.execute { ownedWorkspace?.close() }
        ffiThread.close()
    }
}

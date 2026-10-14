package dev.levity.ide.ffi

import dev.levity.ide.core.TextColumns
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import uniffi.ide_ffi.CursorDirection
import uniffi.ide_ffi.EditorAction
import uniffi.ide_ffi.FfiException
import uniffi.ide_ffi.FfiWorkspace
import java.io.File
import java.nio.file.Files

/**
 * End-to-end test of the generated UniFFI Kotlin bindings against the real Rust core, compiled
 * for the host and loaded through desktop JNA (`jna.library.path` is set by Gradle to the Cargo
 * target directory). This exercises exactly the binding code that ships in the APK.
 */
class FfiWorkspaceJvmTest {
    private lateinit var root: File
    private lateinit var workspace: FfiWorkspace

    @Before
    fun setUp() {
        val libDir = System.getProperty("jna.library.path")
        val libName = System.mapLibraryName("ide_ffi")
        check(libDir != null && File(libDir, libName).isFile) {
            "Host $libName not found in jna.library.path=$libDir (run scripts/build-rust-android.sh)"
        }
        root = Files.createTempDirectory("ide-levity-ffi").toFile()
        File(root, "hello.rs").writeText("fn main() {}\n")
        workspace = FfiWorkspace(root.absolutePath)
    }

    @After
    fun tearDown() {
        workspace.close()
        root.deleteRecursively()
    }

    private fun openHello() = workspace.dispatch(EditorAction.OpenFile(File(root, "hello.rs").absolutePath))

    @Test
    fun listsWorkspaceFiles() {
        val files = workspace.listWorkspaceFiles()
        assertTrue("files=$files", files.any { it.endsWith("hello.rs") })
    }

    @Test
    fun openEditUndoRedoSaveRoundTrip() {
        val opened = openHello()
        assertEquals("fn main() {}", opened.visibleLines.first().text)
        assertFalse(opened.isDirty)

        val edited = workspace.dispatch(EditorAction.InsertText(opened.bufferId, "// Chào 😀\n"))
        assertTrue(edited.isDirty)
        assertEquals("// Chào 😀", edited.visibleLines[0].text)
        assertEquals("fn main() {}", edited.visibleLines[1].text)

        workspace.dispatch(EditorAction.MoveCursor(opened.bufferId, CursorDirection.UP, false))
        val atEnd = workspace.dispatch(EditorAction.MoveCursor(opened.bufferId, CursorDirection.LINE_END, false))
        val cursor = atEnd.cursors.single { it.isPrimary }
        val line = atEnd.visibleLines[0].text
        assertEquals(0u, cursor.line)
        // 9 Rust chars, 10 UTF-16 units: the emoji is a surrogate pair on the Kotlin side.
        assertEquals(9u, cursor.column)
        assertEquals(line.length, TextColumns.charColumnToUtf16(line, cursor.column.toInt()))

        val undone = workspace.dispatch(EditorAction.Undo(opened.bufferId))
        assertEquals("fn main() {}", undone.visibleLines[0].text)
        val redone = workspace.dispatch(EditorAction.Redo(opened.bufferId))
        assertEquals("// Chào 😀", redone.visibleLines[0].text)

        val saved = workspace.dispatch(EditorAction.SaveFile(opened.bufferId))
        assertFalse(saved.isDirty)
        assertEquals("// Chào 😀\nfn main() {}\n", File(root, "hello.rs").readText())
    }

    @Test
    fun highlightSpansStayInsideLineBounds() {
        val opened = openHello()
        val first = opened.visibleLines.first()
        assertTrue("expected syntax spans for Rust source", first.spans.isNotEmpty())
        val charCount = first.text.codePointCount(0, first.text.length).toUInt()
        for (span in first.spans) {
            assertTrue(span.startCol < span.endCol)
            assertTrue(span.endCol <= charCount)
        }
    }

    @Test
    fun unknownBufferSurfacesTypedError() {
        try {
            workspace.dispatch(EditorAction.SaveFile(bufferId = 9_999u))
            fail("expected FfiException")
        } catch (e: FfiException.Workspace) {
            assertTrue(e.message.isNotBlank())
        }
    }
}

package dev.levity.ide

import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * On-device smoke test: launches the real app (native `libide_ffi.so` loaded through JNA),
 * waits for the Rust core to publish the first viewport, types through the soft keyboard input
 * path and checks that the core marks the buffer dirty and that Undo works.
 */
@OptIn(ExperimentalTestApi::class)
@RunWith(AndroidJUnit4::class)
class EditorSmokeTest {
    @get:Rule
    val compose = createAndroidComposeRule<MainActivity>()

    @Test
    fun typingMarksBufferDirtyAndUndoRestoresIt() {
        compose.waitUntilAtLeastOneExists(hasTestTag("editor-canvas"), timeoutMillis = 15_000)

        compose.onNodeWithTag("editor-canvas").performClick()
        compose.onNodeWithTag("editor-input").performTextInput("let x = 1;")
        compose.waitUntilAtLeastOneExists(hasText("\u2022", substring = true), timeoutMillis = 5_000)

        compose.onNodeWithText("Undo").performClick()
        compose.waitUntil(timeoutMillis = 5_000) {
            compose.onAllNodes(hasText("\u2022", substring = true)).fetchSemanticsNodes().isEmpty()
        }
    }

    @Test
    fun agentManagerScreenIsReachable() {
        compose.waitUntilAtLeastOneExists(hasTestTag("editor-canvas"), timeoutMillis = 15_000)
        compose.onNodeWithText("Agents", substring = true).performClick()
        compose.onNodeWithText("Dispatch").assertExists()
    }
}

package dev.levity.ide.core

import dev.levity.ide.core.ImeInputDiff.SENTINEL
import org.junit.Assert.assertEquals
import org.junit.Test

class ImeInputDiffTest {
    @Test
    fun typingAppendsText() {
        assertEquals(listOf(ImeEdit.Insert("a")), ImeInputDiff.diff(SENTINEL, SENTINEL + "a"))
    }

    @Test
    fun backspaceOnIdleFieldDeletesOneCharacter() {
        assertEquals(listOf(ImeEdit.DeleteBackward(1)), ImeInputDiff.diff(SENTINEL, ""))
    }

    @Test
    fun unchangedTextProducesNoEdits() {
        assertEquals(emptyList<ImeEdit>(), ImeInputDiff.diff(SENTINEL + "abc", SENTINEL + "abc"))
    }

    @Test
    fun compositionCorrectionDeletesAndReinserts() {
        // Autocorrect replaces the composing word "teh" by "the ".
        val edits = ImeInputDiff.diff(SENTINEL + "teh", SENTINEL + "the ")
        assertEquals(listOf(ImeEdit.DeleteBackward(2), ImeEdit.Insert("he ")), edits)
    }

    @Test
    fun vietnameseTelexComposition() {
        // Telex input turns "a" + "a" into "â" by replacing the composed character.
        val edits = ImeInputDiff.diff(SENTINEL + "ca", SENTINEL + "câ")
        assertEquals(listOf(ImeEdit.DeleteBackward(1), ImeEdit.Insert("â")), edits)
    }

    @Test
    fun emojiDeletionCountsRustCharactersNotUtf16Units() {
        val edits = ImeInputDiff.diff(SENTINEL + "😀", SENTINEL)
        assertEquals(listOf(ImeEdit.DeleteBackward(1)), edits)
    }

    @Test
    fun replacingOneEmojiWithAnotherSharingHighSurrogateDoesNotSplitPair() {
        // U+1F600 and U+1F601 share the same high surrogate.
        val edits = ImeInputDiff.diff(SENTINEL + "😀", SENTINEL + "😁")
        assertEquals(listOf(ImeEdit.DeleteBackward(1), ImeEdit.Insert("😁")), edits)
    }

    @Test
    fun newlineIsInsertedVerbatim() {
        assertEquals(listOf(ImeEdit.Insert("\n")), ImeInputDiff.diff(SENTINEL, SENTINEL + "\n"))
    }
}

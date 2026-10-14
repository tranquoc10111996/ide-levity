package dev.levity.ide.core

/** A primitive edit produced by the soft keyboard, expressed in Rust `char` units. */
sealed interface ImeEdit {
    /** Delete [count] characters before the cursor (maps to repeated `DeleteBackward`). */
    data class DeleteBackward(val count: Int) : ImeEdit

    /** Insert [text] at the cursor (maps to `InsertText`). */
    data class Insert(val text: String) : ImeEdit
}

/**
 * Translates soft keyboard (IME) changes of a hidden text field into editor actions.
 *
 * The hidden field always starts from [SENTINEL] so that a backspace on an otherwise empty field
 * is still observable as a deletion. While the IME is composing (suggestions, autocorrect) the
 * field keeps its content and every change is diffed against the previous value, so replacing a
 * composed word turns into "delete N, insert M". Once composition ends, the field is reset to the
 * sentinel by the caller.
 */
object ImeInputDiff {
    /** Content of the hidden input field when idle. A zero width space is invisible to the user. */
    const val SENTINEL: String = "\u200B"

    /** Computes the edits that transform [old] into [new]. */
    fun diff(old: String, new: String): List<ImeEdit> {
        if (old == new) return emptyList()
        var prefix = 0
        val max = minOf(old.length, new.length)
        while (prefix < max && old[prefix] == new[prefix]) prefix++
        // Do not split a surrogate pair between the kept prefix and the edited suffix.
        if (prefix > 0 && Character.isHighSurrogate(old[prefix - 1])) prefix--

        val removed = old.substring(prefix)
        val inserted = new.substring(prefix)
        val edits = ArrayList<ImeEdit>(2)
        val deleteCount = removed.codePointCount(0, removed.length)
        if (deleteCount > 0) edits += ImeEdit.DeleteBackward(deleteCount)
        if (inserted.isNotEmpty()) edits += ImeEdit.Insert(inserted)
        return edits
    }
}

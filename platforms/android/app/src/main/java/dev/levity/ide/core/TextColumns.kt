package dev.levity.ide.core

/**
 * Column conversions between the Rust core and Kotlin strings.
 *
 * The Rust core (`ropey`) addresses columns in Unicode scalar values (`char`), while Kotlin
 * `String` indices are UTF-16 code units. Characters outside the Basic Multilingual Plane
 * (emoji, some CJK) occupy two UTF-16 units but a single Rust column, so every column that
 * crosses the FFI boundary must be converted.
 */
object TextColumns {
    /** Converts a Rust char column into a UTF-16 index into [text], clamped to `text.length`. */
    fun charColumnToUtf16(text: String, column: Int): Int {
        if (column <= 0) return 0
        var index = 0
        var col = 0
        while (index < text.length && col < column) {
            index += Character.charCount(text.codePointAt(index))
            col++
        }
        return index
    }

    /** Converts a UTF-16 index into [text] into a Rust char column. */
    fun utf16ToCharColumn(text: String, utf16Index: Int): Int {
        var end = utf16Index.coerceIn(0, text.length)
        // Never split a surrogate pair: a position inside a pair maps to the start of that character.
        if (end in 1 until text.length && Character.isLowSurrogate(text[end]) &&
            Character.isHighSurrogate(text[end - 1])
        ) {
            end -= 1
        }
        return text.codePointCount(0, end)
    }
}

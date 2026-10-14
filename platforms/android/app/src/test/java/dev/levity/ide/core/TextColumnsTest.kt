package dev.levity.ide.core

import org.junit.Assert.assertEquals
import org.junit.Test

class TextColumnsTest {
    @Test
    fun asciiColumnsAreIdentity() {
        val text = "fn main() {}"
        for (col in 0..text.length) {
            assertEquals(col, TextColumns.charColumnToUtf16(text, col))
            assertEquals(col, TextColumns.utf16ToCharColumn(text, col))
        }
    }

    @Test
    fun vietnameseBmpCharactersAreSingleUnits() {
        val text = "Chào mừng"
        assertEquals(4, TextColumns.charColumnToUtf16(text, 4))
        assertEquals(9, TextColumns.utf16ToCharColumn(text, text.length))
    }

    @Test
    fun emojiOccupiesTwoUtf16UnitsButOneRustColumn() {
        val text = "a😀b"
        assertEquals(4, text.length)
        assertEquals(0, TextColumns.charColumnToUtf16(text, 0))
        assertEquals(1, TextColumns.charColumnToUtf16(text, 1))
        assertEquals(3, TextColumns.charColumnToUtf16(text, 2))
        assertEquals(4, TextColumns.charColumnToUtf16(text, 3))
        assertEquals(2, TextColumns.utf16ToCharColumn(text, 3))
        assertEquals(3, TextColumns.utf16ToCharColumn(text, 4))
    }

    @Test
    fun positionInsideSurrogatePairMapsToCharacterStart() {
        val text = "a😀b"
        assertEquals(1, TextColumns.utf16ToCharColumn(text, 2))
    }

    @Test
    fun outOfRangeValuesAreClamped() {
        val text = "abc"
        assertEquals(3, TextColumns.charColumnToUtf16(text, 99))
        assertEquals(0, TextColumns.charColumnToUtf16(text, -5))
        assertEquals(3, TextColumns.utf16ToCharColumn(text, 99))
        assertEquals(0, TextColumns.utf16ToCharColumn(text, -1))
    }
}

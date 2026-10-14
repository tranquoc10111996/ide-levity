package dev.levity.ide.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.ide_ffi.DiffMarkerKind
import uniffi.ide_ffi.EditorAction
import uniffi.ide_ffi.RenderedLine
import uniffi.ide_ffi.ViewportViewModel
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * High-performance 120fps Code Editor surface using Jetpack Compose `Canvas` + `TextMeasurer`
 * to render `ViewportViewModel.visibleLines`, Git diff gutter markers, cursors, selections,
 * and a dedicated programming shortcut bar above the virtual keyboard.
 */
@Composable
fun CodeEditorComposable(
    viewport: ViewportViewModel,
    onDispatch: (EditorAction) -> Unit,
    modifier: Modifier = Modifier
) {
    val textMeasurer = rememberTextMeasurer()
    val lineHeightPx = 52f
    val gutterWidthPx = 132f
    val approxCharWidthPx = 22f

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(Color(0xFF0D1117))
            .imePadding()
    ) {
        Canvas(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .pointerInput(viewport.bufferId, viewport.firstVisibleLine) {
                    detectTapGestures { tapOffset ->
                        val clickedLineOffset = (tapOffset.y / lineHeightPx).toInt().coerceAtLeast(0)
                        val targetLine = viewport.firstVisibleLine + clickedLineOffset.toUInt()
                        val targetCol = max(
                            0,
                            ((tapOffset.x - gutterWidthPx) / approxCharWidthPx).roundToInt()
                        ).toUInt()
                        onDispatch(
                            EditorAction.SetCursorPosition(
                                bufferId = viewport.bufferId,
                                line = targetLine,
                                column = targetCol,
                                select = false
                            )
                        )
                    }
                }
                .pointerInput(viewport.bufferId, viewport.firstVisibleLine) {
                    detectVerticalDragGestures { _, dragAmount ->
                        val lineDelta = (-dragAmount / lineHeightPx).roundToInt()
                        if (lineDelta != 0) {
                            val currentFirst = viewport.firstVisibleLine.toInt()
                            val nextFirst = max(0, currentFirst + lineDelta).toUInt()
                            val visibleCount = max(15, (size.height / lineHeightPx).toInt()).toUInt()
                            onDispatch(
                                EditorAction.ScrollViewport(
                                    bufferId = viewport.bufferId,
                                    firstVisibleLine = nextFirst,
                                    visibleLineCount = visibleCount
                                )
                            )
                        }
                    }
                }
        ) {
            // 1. Gutter background
            drawRect(
                color = Color(0xFF161B22),
                topLeft = Offset.Zero,
                size = Size(gutterWidthPx, size.height)
            )

            // 2. Render only visible_lines for 120fps viewport virtualization
            viewport.visibleLines.forEachIndexed { idx, renderedLine ->
                val y = idx * lineHeightPx
                if (y <= size.height) {
                    // Line number
                    val lineNumLayout = textMeasurer.measure(
                        text = AnnotatedString((renderedLine.lineNumber + 1u).toString().padStart(4, ' ')),
                        style = TextStyle(
                            color = Color(0xFF6E7681),
                            fontSize = 13.sp,
                            fontFamily = FontFamily.Monospace
                        )
                    )
                    drawText(
                        textLayoutResult = lineNumLayout,
                        topLeft = Offset(16f, y + 6f)
                    )

                    // Git diff marker in gutter
                    val diffMarker = viewport.gitDiffMarkers.firstOrNull {
                        it.lineNumber == renderedLine.lineNumber
                    }
                    if (diffMarker != null) {
                        val markerColor = when (diffMarker.status) {
                            DiffMarkerKind.ADDED -> Color(0xFF2EA043)
                            DiffMarkerKind.MODIFIED -> Color(0xFF58A6FF)
                            DiffMarkerKind.DELETED -> Color(0xFFF85149)
                            DiffMarkerKind.UNCHANGED -> Color.Transparent
                        }
                        drawRect(
                            color = markerColor,
                            topLeft = Offset(gutterWidthPx - 10f, y),
                            size = Size(6f, lineHeightPx)
                        )
                    }

                    // Syntax-highlighted line text
                    val annotatedLine = buildSyntaxAnnotatedString(renderedLine)
                    val codeLayout = textMeasurer.measure(
                        text = annotatedLine,
                        style = TextStyle(
                            color = Color(0xFFE6EDF3),
                            fontSize = 14.sp,
                            fontFamily = FontFamily.Monospace
                        )
                    )
                    drawText(
                        textLayoutResult = codeLayout,
                        topLeft = Offset(gutterWidthPx + 18f, y + 4f)
                    )
                }
            }

            // 3. Draw active cursors
            viewport.cursors.forEach { cursor ->
                if (cursor.line >= viewport.firstVisibleLine) {
                    val relRow = (cursor.line - viewport.firstVisibleLine).toInt()
                    val cy = relRow * lineHeightPx
                    val cx = gutterWidthPx + 18f + cursor.column.toFloat() * approxCharWidthPx
                    if (cy <= size.height) {
                        drawRect(
                            color = if (cursor.isPrimary) Color(0xFF58A6FF) else Color(0xFFD2A8FF),
                            topLeft = Offset(cx, cy + 4f),
                            size = Size(4f, lineHeightPx - 8f)
                        )
                    }
                }
            }
        }

        // 4. Programming Shortcut Toolbar docked above the virtual keyboard
        ProgrammingShortcutToolbar(
            bufferId = viewport.bufferId,
            onDispatch = onDispatch
        )
    }
}

@Composable
fun ProgrammingShortcutToolbar(
    bufferId: ULong,
    onDispatch: (EditorAction) -> Unit,
    modifier: Modifier = Modifier
) {
    Surface(
        color = Color(0xFF161B22),
        tonalElevation = 4.dp,
        modifier = modifier.fillMaxWidth()
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 8.dp, vertical = 4.dp),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            ShortcutKeyButton("Tab") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = "    "))
            }
            ShortcutKeyButton("{") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = "{"))
            }
            ShortcutKeyButton("}") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = "}"))
            }
            ShortcutKeyButton("(") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = "("))
            }
            ShortcutKeyButton(")") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = ")"))
            }
            ShortcutKeyButton(";") {
                onDispatch(EditorAction.InsertText(bufferId = bufferId, text = ";"))
            }
            ShortcutKeyButton("Esc") {
                onDispatch(
                    EditorAction.SetCursorPosition(
                        bufferId = bufferId,
                        line = 0u,
                        column = 0u,
                        select = false
                    )
                )
            }
            ShortcutKeyButton("Undo") {
                onDispatch(EditorAction.Undo(bufferId = bufferId))
            }
            ShortcutKeyButton("Redo") {
                onDispatch(EditorAction.Redo(bufferId = bufferId))
            }
        }
    }
}

@Composable
private fun ShortcutKeyButton(
    label: String,
    onClick: () -> Unit
) {
    OutlinedButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
        modifier = Modifier.height(36.dp)
    ) {
        Text(
            text = label,
            fontFamily = FontFamily.Monospace,
            fontSize = 13.sp
        )
    }
}

private fun buildSyntaxAnnotatedString(line: RenderedLine): AnnotatedString {
    return buildAnnotatedString {
        append(line.text)
        val len = line.text.length
        for (span in line.spans) {
            val start = span.startCol.toInt().coerceIn(0, len)
            val end = span.endCol.toInt().coerceIn(start, len)
            if (start < end) {
                val rgba = span.colorRgba
                val r = ((rgba shr 24) and 0xFFu).toInt()
                val g = ((rgba shr 16) and 0xFFu).toInt()
                val b = ((rgba shr 8) and 0xFFu).toInt()
                val a = (rgba and 0xFFu).toInt()
                val styleBits = span.fontStyle.toInt()
                val weight = if ((styleBits and 1) != 0) FontWeight.Bold else FontWeight.Normal
                val italic = if ((styleBits and 2) != 0) FontStyle.Italic else FontStyle.Normal
                addStyle(
                    style = SpanStyle(
                        color = Color(r, g, b, a),
                        fontWeight = weight,
                        fontStyle = italic
                    ),
                    start = start,
                    end = end
                )
            }
        }
    }
}

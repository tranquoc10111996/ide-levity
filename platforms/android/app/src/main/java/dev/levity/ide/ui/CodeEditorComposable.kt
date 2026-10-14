package dev.levity.ide.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEvent
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.isCtrlPressed
import androidx.compose.ui.input.key.isMetaPressed
import androidx.compose.ui.input.key.isShiftPressed
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.levity.ide.core.ImeEdit
import dev.levity.ide.core.ImeInputDiff
import dev.levity.ide.core.TextColumns
import uniffi.ide_ffi.CursorDirection
import uniffi.ide_ffi.DiagnosticSeverityLevel
import uniffi.ide_ffi.DiffMarkerKind
import uniffi.ide_ffi.EditorAction
import uniffi.ide_ffi.RenderedLine
import uniffi.ide_ffi.ViewportViewModel
import kotlin.math.max
import kotlin.math.roundToInt

private val EditorBackground = Color(0xFF0D1117)
private val GutterBackground = Color(0xFF161B22)
private val LineNumberColor = Color(0xFF6E7681)
private val DefaultTextColor = Color(0xFFE6EDF3)
private val SelectionColor = Color(0x5538BDF8)
private val PrimaryCursorColor = Color(0xFF58A6FF)
private val SecondaryCursorColor = Color(0xFFD2A8FF)

/** Clipboard operations that need the platform clipboard and are handled by the app shell. */
enum class ClipboardCommand { COPY, CUT, PASTE }

/**
 * Code editor surface: a Compose `Canvas` + `TextMeasurer` renderer of
 * `ViewportViewModel.visibleLines` (line numbers, Git gutter, syntax spans, selections,
 * diagnostics, cursors), soft keyboard and hardware keyboard input, and a programming shortcut
 * bar docked above the virtual keyboard.
 *
 * All columns coming from Rust are `char` columns and are converted with [TextColumns].
 */
@Composable
fun CodeEditorComposable(
    viewport: ViewportViewModel,
    onDispatch: (EditorAction) -> Unit,
    onClipboard: (ClipboardCommand) -> Unit,
    modifier: Modifier = Modifier,
) {
    val density = LocalDensity.current
    val textMeasurer = rememberTextMeasurer()
    val codeStyle = remember {
        TextStyle(color = DefaultTextColor, fontSize = 14.sp, fontFamily = FontFamily.Monospace)
    }
    val numberStyle = remember {
        TextStyle(color = LineNumberColor, fontSize = 13.sp, fontFamily = FontFamily.Monospace)
    }
    val lineHeightPx = with(density) { 22.sp.toPx() }
    val gutterPaddingPx = with(density) { 10.dp.toPx() }
    val codeLeftPadPx = with(density) { 8.dp.toPx() }
    val digits = max(3, viewport.totalLines.toString().length)
    val numberWidthPx = remember(digits, density) {
        textMeasurer.measure("0".repeat(digits), numberStyle).size.width.toFloat()
    }
    val gutterWidthPx = numberWidthPx + gutterPaddingPx * 2 + with(density) { 4.dp.toPx() }
    val codeStartX = gutterWidthPx + codeLeftPadPx

    // Layouts of the visible lines, reused for drawing and for hit testing taps.
    val lineLayouts: List<TextLayoutResult> = remember(viewport.visibleLines, density) {
        viewport.visibleLines.map { line ->
            textMeasurer.measure(buildSyntaxAnnotatedString(line), codeStyle, softWrap = false)
        }
    }
    val currentViewport by rememberUpdatedState(viewport)
    val currentDispatch by rememberUpdatedState(onDispatch)
    val currentClipboard by rememberUpdatedState(onClipboard)

    val focusRequester = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    var imeValue by remember {
        mutableStateOf(TextFieldValue(ImeInputDiff.SENTINEL, TextRange(ImeInputDiff.SENTINEL.length)))
    }
    var canvasHeightPx by remember { mutableStateOf(0) }

    // Keep the core's viewport size in sync with the on-screen canvas height.
    LaunchedEffect(canvasHeightPx, viewport.bufferId) {
        if (canvasHeightPx > 0) {
            val count = max(1, (canvasHeightPx / lineHeightPx).toInt() + 1)
            currentDispatch(
                EditorAction.ScrollViewport(
                    bufferId = currentViewport.bufferId,
                    firstVisibleLine = currentViewport.firstVisibleLine,
                    visibleLineCount = count.toUInt(),
                )
            )
        }
    }

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(EditorBackground)
            .imePadding()
    ) {
        Box(modifier = Modifier.weight(1f).fillMaxWidth()) {
            // Hidden input target for the soft keyboard and hardware keyboard.
            BasicTextField(
                value = imeValue,
                onValueChange = { newValue ->
                    val edits = ImeInputDiff.diff(imeValue.text, newValue.text)
                    for (edit in edits) {
                        val bufferId = currentViewport.bufferId
                        when (edit) {
                            is ImeEdit.DeleteBackward -> repeat(edit.count) {
                                currentDispatch(EditorAction.DeleteBackward(bufferId = bufferId))
                            }
                            // A typed Enter goes through InsertNewline so the core applies auto-indent.
                            is ImeEdit.Insert -> currentDispatch(
                                if (edit.text == "\n") EditorAction.InsertNewline(bufferId = bufferId)
                                else EditorAction.InsertText(bufferId = bufferId, text = edit.text)
                            )
                        }
                    }
                    imeValue = if (newValue.composition == null) {
                        TextFieldValue(ImeInputDiff.SENTINEL, TextRange(ImeInputDiff.SENTINEL.length))
                    } else {
                        newValue
                    }
                },
                keyboardOptions = KeyboardOptions(
                    capitalization = KeyboardCapitalization.None,
                    autoCorrectEnabled = false,
                    keyboardType = KeyboardType.Ascii,
                    imeAction = ImeAction.None,
                ),
                modifier = Modifier
                    .size(1.dp)
                    .alpha(0f)
                    .focusRequester(focusRequester)
                    .testTag("editor-input")
                    .onPreviewKeyEvent { event ->
                        handleHardwareKey(event, currentViewport.bufferId, currentDispatch, currentClipboard)
                    }
            )

            Canvas(
                modifier = Modifier
                    .fillMaxSize()
                    .testTag("editor-canvas")
                    .semantics { contentDescription = "Code editor ${viewport.filePath}" }
                    .onSizeChanged { canvasHeightPx = it.height }
                    .pointerInput(lineLayouts, codeStartX, lineHeightPx) {
                        detectTapGestures { tap ->
                            focusRequester.requestFocus()
                            keyboard?.show()
                            val vm = currentViewport
                            val row = (tap.y / lineHeightPx).toInt().coerceAtLeast(0)
                            val layout = lineLayouts.getOrNull(row)
                            val line = vm.visibleLines.getOrNull(row)
                            val column = if (layout != null && line != null) {
                                val offset = layout.getOffsetForPosition(Offset(tap.x - codeStartX, layout.size.height / 2f))
                                TextColumns.utf16ToCharColumn(line.text, offset)
                            } else {
                                0
                            }
                            val targetLine = if (line != null) line.lineNumber else
                                (vm.totalLines.toInt() - 1).coerceAtLeast(0).toUInt()
                            currentDispatch(
                                EditorAction.SetCursorPosition(
                                    bufferId = vm.bufferId,
                                    line = targetLine,
                                    column = column.toUInt(),
                                    select = false,
                                )
                            )
                        }
                    }
                    .pointerInput(lineHeightPx) {
                        var accumulated = 0f
                        detectVerticalDragGestures(onDragEnd = { accumulated = 0f }) { change, dragAmount ->
                            change.consume()
                            accumulated += dragAmount
                            val lineDelta = (-accumulated / lineHeightPx).toInt()
                            if (lineDelta != 0) {
                                accumulated += lineDelta * lineHeightPx
                                val vm = currentViewport
                                val maxFirst = (vm.totalLines.toInt() - 1).coerceAtLeast(0)
                                val nextFirst = (vm.firstVisibleLine.toInt() + lineDelta).coerceIn(0, maxFirst)
                                val count = max(1, (size.height / lineHeightPx).toInt() + 1)
                                currentDispatch(
                                    EditorAction.ScrollViewport(
                                        bufferId = vm.bufferId,
                                        firstVisibleLine = nextFirst.toUInt(),
                                        visibleLineCount = count.toUInt(),
                                    )
                                )
                            }
                        }
                    }
            ) {
                drawRect(color = GutterBackground, topLeft = Offset.Zero, size = Size(gutterWidthPx, size.height))

                val first = viewport.firstVisibleLine.toLong()
                fun rowOf(line: UInt): Int = (line.toLong() - first).toInt()
                fun xOf(row: Int, column: UInt): Float {
                    val layout = lineLayouts.getOrNull(row) ?: return codeStartX
                    val text = viewport.visibleLines[row].text
                    val index = TextColumns.charColumnToUtf16(text, column.toInt())
                    return codeStartX + layout.getHorizontalPosition(index, usePrimaryDirection = true)
                }

                // Selections (drawn under the text).
                for (sel in viewport.selections) {
                    for (line in sel.startLine..sel.endLine) {
                        val row = rowOf(line)
                        if (row < 0 || row >= lineLayouts.size) continue
                        val startX = if (line == sel.startLine) xOf(row, sel.startCol) else codeStartX
                        val endX = if (line == sel.endLine) xOf(row, sel.endCol) else
                            codeStartX + lineLayouts[row].size.width + lineHeightPx / 3f
                        if (endX > startX) {
                            drawRect(SelectionColor, Offset(startX, row * lineHeightPx), Size(endX - startX, lineHeightPx))
                        }
                    }
                }

                viewport.visibleLines.forEachIndexed { row, renderedLine ->
                    val y = row * lineHeightPx
                    if (y > size.height) return@forEachIndexed
                    val numberLayout = textMeasurer.measure(
                        (renderedLine.lineNumber + 1u).toString().padStart(digits, ' '),
                        numberStyle,
                    )
                    drawText(
                        textLayoutResult = numberLayout,
                        topLeft = Offset(gutterPaddingPx, y + (lineHeightPx - numberLayout.size.height) / 2f),
                    )

                    viewport.gitDiffMarkers.firstOrNull { it.lineNumber == renderedLine.lineNumber }?.let { marker ->
                        val markerColor = when (marker.status) {
                            DiffMarkerKind.ADDED -> Color(0xFF2EA043)
                            DiffMarkerKind.MODIFIED -> Color(0xFF58A6FF)
                            DiffMarkerKind.DELETED -> Color(0xFFF85149)
                            DiffMarkerKind.UNCHANGED -> null
                        }
                        if (markerColor != null) {
                            drawRect(markerColor, Offset(gutterWidthPx - 4.dp.toPx(), y), Size(3.dp.toPx(), lineHeightPx))
                        }
                    }

                    val layout = lineLayouts[row]
                    drawText(textLayoutResult = layout, topLeft = Offset(codeStartX, y + (lineHeightPx - layout.size.height) / 2f))
                }

                // Diagnostics: squiggly underline under the affected range.
                for (diag in viewport.diagnostics) {
                    val row = rowOf(diag.line)
                    if (row < 0 || row >= lineLayouts.size) continue
                    val color = when (diag.severity) {
                        DiagnosticSeverityLevel.ERROR -> Color(0xFFF85149)
                        DiagnosticSeverityLevel.WARNING -> Color(0xFFD29922)
                        DiagnosticSeverityLevel.INFORMATION -> Color(0xFF58A6FF)
                        DiagnosticSeverityLevel.HINT -> Color(0xFF8B949E)
                    }
                    val startX = xOf(row, diag.startCol)
                    val endX = max(xOf(row, diag.endCol), startX + 6.dp.toPx())
                    val baseY = (row + 1) * lineHeightPx - 2.dp.toPx()
                    val amplitude = 1.5.dp.toPx()
                    val step = 3.dp.toPx()
                    val path = Path().apply {
                        moveTo(startX, baseY)
                        var x = startX
                        var up = true
                        while (x < endX) {
                            x += step
                            lineTo(x, if (up) baseY - amplitude else baseY + amplitude)
                            up = !up
                        }
                    }
                    drawPath(path, color, style = Stroke(width = 1.dp.toPx()))
                }

                for (cursor in viewport.cursors) {
                    val row = rowOf(cursor.line)
                    if (row < 0 || row * lineHeightPx > size.height) continue
                    drawRect(
                        color = if (cursor.isPrimary) PrimaryCursorColor else SecondaryCursorColor,
                        topLeft = Offset(xOf(row, cursor.column), row * lineHeightPx + 2.dp.toPx()),
                        size = Size(2.dp.toPx(), lineHeightPx - 4.dp.toPx()),
                    )
                }
            }
        }

        ProgrammingShortcutToolbar(viewport = viewport, onDispatch = onDispatch, onClipboard = onClipboard)
    }
}

/** Maps hardware keyboard shortcuts (tablets, Chromebooks, Bluetooth keyboards) to actions. */
private fun handleHardwareKey(
    event: KeyEvent,
    bufferId: ULong,
    dispatch: (EditorAction) -> Unit,
    clipboard: (ClipboardCommand) -> Unit,
): Boolean {
    if (event.type != KeyEventType.KeyDown) return false
    val shift = event.isShiftPressed
    val command = event.isCtrlPressed || event.isMetaPressed
    fun move(direction: CursorDirection) = dispatch(EditorAction.MoveCursor(bufferId, direction, shift))
    when {
        command && event.key == Key.Z && shift -> dispatch(EditorAction.Redo(bufferId))
        command && event.key == Key.Z -> dispatch(EditorAction.Undo(bufferId))
        command && event.key == Key.Y -> dispatch(EditorAction.Redo(bufferId))
        command && event.key == Key.S -> dispatch(EditorAction.SaveFile(bufferId))
        command && event.key == Key.A -> dispatch(EditorAction.SelectAll(bufferId))
        command && event.key == Key.D -> dispatch(EditorAction.AddNextOccurrence(bufferId))
        command && event.key == Key.Slash -> dispatch(EditorAction.ToggleLineComment(bufferId))
        command && shift && event.key == Key.K -> dispatch(EditorAction.DeleteSelectionOrLine(bufferId))
        command && event.key == Key.C -> clipboard(ClipboardCommand.COPY)
        command && event.key == Key.X -> clipboard(ClipboardCommand.CUT)
        command && event.key == Key.V -> clipboard(ClipboardCommand.PASTE)
        command && event.key == Key.Spacebar -> dispatch(EditorAction.TriggerLspCompletion(bufferId))
        command && event.key == Key.Backspace -> dispatch(EditorAction.DeleteWordBackward(bufferId))
        command && event.key == Key.Delete -> dispatch(EditorAction.DeleteWordForward(bufferId))
        command && event.key == Key.DirectionLeft -> move(CursorDirection.WORD_LEFT)
        command && event.key == Key.DirectionRight -> move(CursorDirection.WORD_RIGHT)
        command && event.key == Key.MoveHome -> move(CursorDirection.DOCUMENT_START)
        command && event.key == Key.MoveEnd -> move(CursorDirection.DOCUMENT_END)
        event.key == Key.DirectionUp -> move(CursorDirection.UP)
        event.key == Key.DirectionDown -> move(CursorDirection.DOWN)
        event.key == Key.DirectionLeft -> move(CursorDirection.LEFT)
        event.key == Key.DirectionRight -> move(CursorDirection.RIGHT)
        event.key == Key.MoveHome -> move(CursorDirection.LINE_START)
        event.key == Key.MoveEnd -> move(CursorDirection.LINE_END)
        event.key == Key.PageUp -> dispatch(EditorAction.MovePage(bufferId, -1, shift))
        event.key == Key.PageDown -> dispatch(EditorAction.MovePage(bufferId, 1, shift))
        event.key == Key.Backspace -> dispatch(EditorAction.DeleteBackward(bufferId))
        event.key == Key.Delete -> dispatch(EditorAction.DeleteForward(bufferId))
        event.key == Key.Enter || event.key == Key.NumPadEnter -> dispatch(EditorAction.InsertNewline(bufferId))
        event.key == Key.Tab && shift -> dispatch(EditorAction.Outdent(bufferId))
        event.key == Key.Tab -> dispatch(EditorAction.Indent(bufferId))
        event.key == Key.Escape -> dispatch(EditorAction.CollapseCursors(bufferId))
        else -> return false
    }
    return true
}

/** Shortcut keys that are hard to reach on a phone keyboard, docked above the IME. */
@Composable
fun ProgrammingShortcutToolbar(
    viewport: ViewportViewModel,
    onDispatch: (EditorAction) -> Unit,
    onClipboard: (ClipboardCommand) -> Unit,
    modifier: Modifier = Modifier,
) {
    val bufferId = viewport.bufferId
    Surface(color = GutterBackground, tonalElevation = 4.dp, modifier = modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 8.dp, vertical = 4.dp),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ShortcutKeyButton("Tab") { onDispatch(EditorAction.Indent(bufferId)) }
            ShortcutKeyButton("\u21E4") { onDispatch(EditorAction.Outdent(bufferId)) }
            for (symbol in listOf("{", "}", "(", ")", "[", "]", ";", "\"", "<", ">", "=")) {
                ShortcutKeyButton(symbol) { onDispatch(EditorAction.InsertText(bufferId, symbol)) }
            }
            ShortcutKeyButton("\u2190") { onDispatch(EditorAction.MoveCursor(bufferId, CursorDirection.LEFT, false)) }
            ShortcutKeyButton("\u2192") { onDispatch(EditorAction.MoveCursor(bufferId, CursorDirection.RIGHT, false)) }
            ShortcutKeyButton("\u2191") { onDispatch(EditorAction.MoveCursor(bufferId, CursorDirection.UP, false)) }
            ShortcutKeyButton("\u2193") { onDispatch(EditorAction.MoveCursor(bufferId, CursorDirection.DOWN, false)) }
            ShortcutKeyButton("Esc") { onDispatch(EditorAction.CollapseCursors(bufferId)) }
            ShortcutKeyButton("//") { onDispatch(EditorAction.ToggleLineComment(bufferId)) }
            ShortcutKeyButton("Copy") { onClipboard(ClipboardCommand.COPY) }
            ShortcutKeyButton("Cut") { onClipboard(ClipboardCommand.CUT) }
            ShortcutKeyButton("Paste") { onClipboard(ClipboardCommand.PASTE) }
            ShortcutKeyButton("Undo") { onDispatch(EditorAction.Undo(bufferId)) }
            ShortcutKeyButton("Redo") { onDispatch(EditorAction.Redo(bufferId)) }
        }
    }
}

@Composable
private fun ShortcutKeyButton(label: String, onClick: () -> Unit) {
    OutlinedButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
        modifier = Modifier.height(36.dp),
    ) {
        Text(text = label, fontFamily = FontFamily.Monospace, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Clip)
    }
}

/** Builds a styled line from Rust highlight spans (char columns, RGBA colors, bold=1/italic=2 bits). */
internal fun buildSyntaxAnnotatedString(line: RenderedLine): AnnotatedString = buildAnnotatedString {
    append(line.text)
    for (span in line.spans) {
        val start = TextColumns.charColumnToUtf16(line.text, span.startCol.toInt())
        val end = TextColumns.charColumnToUtf16(line.text, span.endCol.toInt())
        if (start >= end) continue
        val rgba = span.colorRgba
        val styleBits = span.fontStyle.toInt()
        addStyle(
            style = SpanStyle(
                color = Color(
                    red = ((rgba shr 24) and 0xFFu).toInt(),
                    green = ((rgba shr 16) and 0xFFu).toInt(),
                    blue = ((rgba shr 8) and 0xFFu).toInt(),
                    alpha = (rgba and 0xFFu).toInt(),
                ),
                fontWeight = if ((styleBits and 1) != 0) FontWeight.Bold else FontWeight.Normal,
                fontStyle = if ((styleBits and 2) != 0) FontStyle.Italic else FontStyle.Normal,
            ),
            start = start,
            end = end,
        )
    }
}

package dev.levity.ide.ui

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import uniffi.ide_ffi.DirEntryInfo

/** One visible row of the flattened explorer tree. */
private data class ExplorerRow(val entry: DirEntryInfo, val depth: Int)

/** Flattens the loaded directory listings into visible rows (expanded directories only). */
private fun flatten(
    rootDir: String,
    listings: Map<String, List<DirEntryInfo>>,
    expanded: Set<String>,
): List<ExplorerRow> {
    val rows = ArrayList<ExplorerRow>()
    fun visit(dir: String, depth: Int) {
        for (entry in listings[dir].orEmpty()) {
            rows += ExplorerRow(entry, depth)
            if (entry.isDir && entry.path in expanded) visit(entry.path, depth + 1)
        }
    }
    visit(rootDir, 0)
    return rows
}

/** Operations the explorer can request from the workspace. */
interface ExplorerActions {
    fun openFile(path: String)
    fun toggleDirectory(path: String)
    fun requestNewFile()
    fun requestNewFolder()
    fun requestRename(entry: DirEntryInfo)
    fun requestDelete(entry: DirEntryInfo)
}

/** Lazily loaded workspace tree (folders first, one level fetched per expansion). */
@Composable
fun ExplorerPanel(
    rootDir: String,
    gitBranch: String?,
    listings: Map<String, List<DirEntryInfo>>,
    expanded: Set<String>,
    activePath: String?,
    actions: ExplorerActions,
    modifier: Modifier = Modifier,
) {
    val rows = remember(rootDir, listings, expanded) { flatten(rootDir, listings, expanded) }
    Column(modifier = modifier) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp, top = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "EXPLORER" + (gitBranch?.let { "  \u2387 $it" } ?: ""),
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.weight(1f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            TextButton(onClick = actions::requestNewFile) { Text("+File") }
            TextButton(onClick = actions::requestNewFolder) { Text("+Dir") }
        }
        HorizontalDivider()
        if (rows.isEmpty()) {
            Text(
                text = "Workspace is empty. Create a file to get started.",
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.padding(16.dp),
            )
        }
        LazyColumn(modifier = Modifier.fillMaxWidth()) {
            items(rows, key = { it.entry.path }) { row ->
                ExplorerRowItem(
                    row = row,
                    isExpanded = row.entry.path in expanded,
                    isActive = row.entry.path == activePath,
                    actions = actions,
                )
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ExplorerRowItem(row: ExplorerRow, isExpanded: Boolean, isActive: Boolean, actions: ExplorerActions) {
    var menuOpen by remember { mutableStateOf(false) }
    val entry = row.entry
    val color = when {
        isActive -> MaterialTheme.colorScheme.primary
        entry.isIgnored -> Color(0xFF6E7681)
        else -> MaterialTheme.colorScheme.onSurface
    }
    Box {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .combinedClickable(
                    onClick = { if (entry.isDir) actions.toggleDirectory(entry.path) else actions.openFile(entry.path) },
                    onLongClick = { menuOpen = true },
                    onLongClickLabel = "File actions",
                )
                .padding(start = (16 + row.depth * 14).dp, top = 10.dp, bottom = 10.dp, end = 16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = if (entry.isDir) (if (isExpanded) "\u25BE" else "\u25B8") else " ",
                fontFamily = FontFamily.Monospace,
                color = color,
            )
            Spacer(Modifier.width(8.dp))
            Text(text = entry.name, color = color, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
            DropdownMenuItem(text = { Text("Rename") }, onClick = { menuOpen = false; actions.requestRename(entry) })
            DropdownMenuItem(text = { Text("Delete") }, onClick = { menuOpen = false; actions.requestDelete(entry) })
        }
    }
}

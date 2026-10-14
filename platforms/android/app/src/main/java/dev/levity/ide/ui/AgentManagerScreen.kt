package dev.levity.ide.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.ide_ffi.AgentTaskStatus
import uniffi.ide_ffi.ViewportViewModel

@Composable
fun AgentManagerScreen(
    viewport: ViewportViewModel,
    onSubmitPrompt: (String) -> Unit,
    onAcceptTask: (ULong) -> Unit,
    onRejectTask: (ULong) -> Unit,
    modifier: Modifier = Modifier
) {
    var promptText by remember { mutableStateOf("") }

    Column(
        modifier = modifier
            .fillMaxSize()
            .background(Color(0xFF0D1117))
            .padding(16.dp)
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            OutlinedTextField(
                value = promptText,
                onValueChange = { promptText = it },
                label = { Text("AI Agent Prompt (e.g. old -> new)") },
                modifier = Modifier.weight(1f)
            )
            Button(
                onClick = {
                    if (promptText.isNotBlank()) {
                        onSubmitPrompt(promptText)
                        promptText = ""
                    }
                },
                modifier = Modifier.padding(top = 8.dp)
            ) {
                Text("Dispatch")
            }
        }

        Spacer(modifier = Modifier.height(16.dp))

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            items(viewport.agentTasks) { task ->
                AgentTaskCard(
                    task = task,
                    onAccept = { onAcceptTask(task.taskId) },
                    onReject = { onRejectTask(task.taskId) }
                )
            }
        }
    }
}

@Composable
private fun AgentTaskCard(
    task: AgentTaskStatus,
    onAccept: () -> Unit,
    onReject: () -> Unit
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp)) {
            val verifiedLabel = if (task.verificationPassed) "Verified" else "Pending"
            Text(
                text = "Task #${task.taskId}: ${task.prompt} [${task.state} | $verifiedLabel]",
                fontSize = 15.sp
            )
            Spacer(modifier = Modifier.height(8.dp))

            val diff = task.unifiedDiffPreview
            if (diff != null) {
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .background(Color(0xFF161B22))
                        .padding(8.dp)
                ) {
                    diff.lines().forEach { line ->
                        val lineColor = when {
                            line.startsWith("+") && !line.startsWith("+++") -> Color(0xFF3FB950)
                            line.startsWith("-") && !line.startsWith("---") -> Color(0xFFF85149)
                            line.startsWith("@@") -> Color(0xFF58A6FF)
                            else -> Color(0xFFC9D1D9)
                        }
                        Text(
                            text = line,
                            color = lineColor,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 12.sp
                        )
                    }
                }
            }

            Spacer(modifier = Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onAccept) {
                    Text("Accept (Chấp nhận)")
                }
                OutlinedButton(onClick = onReject) {
                    Text("Reject (Từ chối)")
                }
            }
        }
    }
}

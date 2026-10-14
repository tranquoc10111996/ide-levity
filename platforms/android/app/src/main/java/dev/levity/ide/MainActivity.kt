package dev.levity.ide

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import dev.levity.ide.ui.IdeLevityApp
import uniffi.ide_ffi.FfiWorkspace

class MainActivity : ComponentActivity() {
    private lateinit var workspace: FfiWorkspace

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val workspacePath = filesDir.absolutePath
        workspace = FfiWorkspace(workspacePath)

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                IdeLevityApp(workspace = workspace)
            }
        }
    }

    override fun onDestroy() {
        if (::workspace.isInitialized) {
            workspace.destroy()
        }
        super.onDestroy()
    }
}

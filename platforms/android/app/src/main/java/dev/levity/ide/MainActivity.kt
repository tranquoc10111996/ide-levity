package dev.levity.ide

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import dev.levity.ide.ui.IdeLevityApp

/** Single activity hosting the Compose UI. The Rust workspace lives in [IdeViewModel]. */
class MainActivity : ComponentActivity() {
    private val viewModel: IdeViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                IdeLevityApp(viewModel = viewModel)
            }
        }
    }
}

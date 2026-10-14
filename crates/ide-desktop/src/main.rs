//! Entrypoint binary for the cross-platform `ide-desktop` application.
//! - Launches the hardware-accelerated `iced` GUI window when a display server (`DISPLAY` / `WAYLAND_DISPLAY` / macOS / Windows) is available.
//! - Provides `--interactive` (`-i`) terminal editor mode with 24-bit TrueColor syntax highlighting for SSH / headless environments.
//! - Provides `--headless` for non-interactive E2E verification.

use iced::{Task, Theme};
use ide_core::{DiffMarkerKind, EditorAction, RenderedLine};
use ide_desktop::{CommandPaletteMode, DesktopMessage, DesktopViewMode, IdeDesktopApp};
use std::io::{self, BufRead, Write};

pub fn main() -> iced::Result {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ide-desktop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("IDE Levity Desktop ({})", env!("CARGO_PKG_VERSION"));
        println!("Usage: ide-desktop [WORKSPACE_OR_FILE_PATH] [--interactive | -i] [--headless]");
        println!();
        println!("Options:");
        println!("  -i, --interactive  Run interactive terminal editor (TrueColor syntax highlighting)");
        println!("      --headless     Run non-interactive E2E showcase and exit");
        println!("  -h, --help         Show this help message");
        println!("  -V, --version      Show version");
        return Ok(());
    }

    let target_path = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or(".");

    let force_headless = args.iter().any(|a| a == "--headless");
    let force_interactive = args.iter().any(|a| a == "--interactive" || a == "-i");
    let has_display = !cfg!(target_os = "linux")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some();

    if force_interactive {
        run_interactive_cli(target_path);
        return Ok(());
    }

    if force_headless || !has_display {
        run_headless_showcase(target_path);
        return Ok(());
    }

    let root = target_path.to_string();
    let start_in_agent_view = args.iter().any(|a| a == "--agent-view");
    let start_with_palette = args.iter().any(|a| a == "--palette");
    iced::application(
        IdeDesktopApp::title,
        IdeDesktopApp::update,
        IdeDesktopApp::view,
    )
    .subscription(IdeDesktopApp::subscription)
    .theme(|_| Theme::Dark)
    .window_size((1360.0, 860.0))
    .run_with(move || {
        let mut app = IdeDesktopApp::new(&root);
        if start_in_agent_view {
            let _ = app.update(DesktopMessage::AgentPromptChanged(
                "120 -> 240".to_string(),
            ));
            let _ = app.update(DesktopMessage::SubmitAgentPrompt);
            let _ = app.update(DesktopMessage::SwitchViewMode(
                DesktopViewMode::AgentManager,
            ));
        }
        if start_with_palette {
            let _ = app.update(DesktopMessage::ToggleCommandPalette(
                CommandPaletteMode::CommandRunner,
            ));
        }
        (app, Task::none())
    })
}

/// Renders a `RenderedLine` using 24-bit TrueColor ANSI escape sequences from `HighlightSpan`.
fn format_line_truecolor(line: &RenderedLine, diff_marker: Option<DiffMarkerKind>) -> String {
    let gutter_symbol = match diff_marker {
        Some(DiffMarkerKind::Added) => "\x1b[38;2;46;160;67m+\x1b[0m",
        Some(DiffMarkerKind::Modified) => "\x1b[38;2;88;166;255m~\x1b[0m",
        Some(DiffMarkerKind::Deleted) => "\x1b[38;2;248;81;73m-\x1b[0m",
        _ => " ",
    };

    let mut out = format!(
        "\x1b[38;2;110;118;129m{:>4}\x1b[0m {} \x1b[38;2;110;118;129m│\x1b[0m ",
        line.line_number + 1,
        gutter_symbol
    );

    if line.spans.is_empty() {
        out.push_str(&line.text);
        return out;
    }

    let chars: Vec<char> = line.text.chars().collect();
    let mut cursor = 0usize;
    for span in &line.spans {
        let s = (span.start_col as usize).min(chars.len());
        let e = (span.end_col as usize).min(chars.len());
        if cursor < s {
            let plain: String = chars[cursor..s].iter().collect();
            out.push_str(&plain);
        }
        if s < e {
            let r = (span.color_rgba >> 24) & 0xFF;
            let g = (span.color_rgba >> 16) & 0xFF;
            let b = (span.color_rgba >> 8) & 0xFF;
            let seg: String = chars[s..e].iter().collect();
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{seg}\x1b[0m"));
            cursor = e;
        }
    }
    if cursor < chars.len() {
        let tail: String = chars[cursor..].iter().collect();
        out.push_str(&tail);
    }
    out
}

fn print_viewport(app: &IdeDesktopApp) {
    let dirty = if app.viewport.is_dirty { " [modified]" } else { "" };
    println!(
        "\n\x1b[1;38;2;88;166;255m─── {} (branch: {}){} ───\x1b[0m",
        app.viewport.file_path, app.git_branch, dirty
    );
    for line in &app.viewport.visible_lines {
        let marker = app
            .viewport
            .git_diff_markers
            .iter()
            .find(|m| m.line_number == line.line_number)
            .map(|m| m.status);
        println!("{}", format_line_truecolor(line, marker));
    }
    if let Some(c) = app.viewport.cursors.first() {
        println!(
            "\x1b[38;2;139;148;158mCursor: Ln {}, Col {} | Total Lines: {} | Diagnostics: {} | Agent Tasks: {}\x1b[0m",
            c.line + 1,
            c.column + 1,
            app.viewport.total_lines,
            app.viewport.diagnostics.len(),
            app.viewport.agent_tasks.len()
        );
    }
}

fn run_interactive_cli(initial_path: &str) {
    let path_meta = std::fs::metadata(initial_path).ok();
    let root_dir = if path_meta.as_ref().is_some_and(|m| m.is_file()) {
        std::path::Path::new(initial_path)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
    } else {
        initial_path
    };

    let mut app = IdeDesktopApp::new(root_dir);
    if path_meta.is_some_and(|m| m.is_file()) {
        let _ = app.update(DesktopMessage::OpenFileFromSidebar(
            initial_path.to_string(),
        ));
    }

    println!("\x1b[1;32mIDE Levity Interactive Mode\x1b[0m - Type `help` for commands, `quit` to exit.");
    print_viewport(&app);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("\x1b[1;36mide-levity>\x1b[0m ");
        let _ = stdout.flush();

        let mut line = String::new();
        let Ok(n) = stdin.lock().read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (cmd, rest) = match trimmed.split_once(' ') {
            Some((c, r)) => (c, r.trim()),
            None => (trimmed, ""),
        };

        match cmd {
            "q" | "quit" | "exit" => break,
            "h" | "help" => {
                println!("Commands:");
                println!("  files                   List files in workspace");
                println!("  open <path>             Open a file in the editor");
                println!("  view                    Print current viewport with syntax highlighting");
                println!("  goto <line> <col>       Move cursor to 1-indexed line and column");
                println!("  insert <text>           Insert text at current cursor");
                println!("  undo | redo             Undo or redo last transaction");
                println!("  save                    Save current buffer to disk");
                println!("  agent <prompt>          Submit AI Agent task (e.g. `agent old -> new`)");
                println!("  accept <id> | reject <id> Accept or reject an Agent task patch");
                println!("  quit                    Exit IDE Levity");
            }
            "files" => {
                for f in &app.workspace_files {
                    println!("  {f}");
                }
            }
            "open" if !rest.is_empty() => {
                let _ = app.update(DesktopMessage::OpenFileFromSidebar(rest.to_string()));
                print_viewport(&app);
            }
            "view" => {
                print_viewport(&app);
            }
            "goto" => {
                let mut parts = rest.split_whitespace();
                let l = parts
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                let c = parts
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                let _ = app.update(DesktopMessage::DispatchEditorAction(
                    EditorAction::SetCursorPosition {
                        buffer_id: app.viewport.buffer_id,
                        line: l,
                        column: c,
                        select: false,
                    },
                ));
                print_viewport(&app);
            }
            "insert" if !rest.is_empty() => {
                let text = rest.replace("\\n", "\n");
                let _ = app.update(DesktopMessage::DispatchEditorAction(
                    EditorAction::InsertText {
                        buffer_id: app.viewport.buffer_id,
                        text,
                    },
                ));
                print_viewport(&app);
            }
            "undo" => {
                let _ = app.update(DesktopMessage::DispatchEditorAction(EditorAction::Undo {
                    buffer_id: app.viewport.buffer_id,
                }));
                print_viewport(&app);
            }
            "redo" => {
                let _ = app.update(DesktopMessage::DispatchEditorAction(EditorAction::Redo {
                    buffer_id: app.viewport.buffer_id,
                }));
                print_viewport(&app);
            }
            "save" => {
                let _ = app.update(DesktopMessage::DispatchEditorAction(
                    EditorAction::SaveFile {
                        buffer_id: app.viewport.buffer_id,
                    },
                ));
                println!("Saved `{}`.", app.viewport.file_path);
            }
            "agent" if !rest.is_empty() => {
                let _ = app.update(DesktopMessage::AgentPromptChanged(rest.to_string()));
                let _ = app.update(DesktopMessage::SubmitAgentPrompt);
                if let Some(t) = app.viewport.agent_tasks.last() {
                    println!("Created Agent Task #{}:", t.task_id);
                    if let Some(diff) = &t.unified_diff_preview {
                        print!("{diff}");
                    }
                }
            }
            "accept" => {
                if let Ok(id) = rest.parse::<u64>() {
                    let _ = app.update(DesktopMessage::AcceptAgentTask(id));
                    print_viewport(&app);
                }
            }
            "reject" => {
                if let Ok(id) = rest.parse::<u64>() {
                    let _ = app.update(DesktopMessage::RejectAgentTask(id));
                    print_viewport(&app);
                }
            }
            _ => {
                println!("Unknown command `{cmd}`. Type `help` for available commands.");
            }
        }
    }
}

fn run_headless_showcase(workspace_path: &str) {
    let mut app = IdeDesktopApp::new(workspace_path);
    println!("================================================================");
    println!("  {} (Headless Runtime Mode)", app.title());
    println!("================================================================");
    println!(
        "Workspace Files Discovered: {} files (Git branch: {})",
        app.workspace_files.len(),
        app.git_branch
    );

    let buf_id = app.viewport.buffer_id;
    let _ = app.update(DesktopMessage::DispatchEditorAction(
        EditorAction::SetCursorPosition {
            buffer_id: buf_id,
            line: 1,
            column: 4,
            select: false,
        },
    ));
    let _ = app.update(DesktopMessage::DispatchEditorAction(
        EditorAction::InsertText {
            buffer_id: buf_id,
            text: "let status = \"Hệ thống đã sẵn sàng hoạt động!\";\n    ".to_string(),
        },
    ));

    print_viewport(&app);

    let _ = app.update(DesktopMessage::SwitchViewMode(DesktopViewMode::AgentManager));
    let _ = app.update(DesktopMessage::AgentPromptChanged(
        "sẵn sàng hoạt động -> chạy hoàn hảo 100%".to_string(),
    ));
    let _ = app.update(DesktopMessage::SubmitAgentPrompt);

    if let Some(task) = app.viewport.agent_tasks.last() {
        let task_id = task.task_id;
        println!(
            "\n[AGENT MANAGER TASK #{} ({})]:",
            task_id, task.prompt
        );
        if let Some(diff) = &task.unified_diff_preview {
            for dl in diff.lines() {
                println!("  {dl}");
            }
        }
        let _ = app.update(DesktopMessage::AcceptAgentTask(task_id));
        println!("  -> Patch accepted and verified!");
    }

    let _ = app.update(DesktopMessage::ToggleCommandPalette(
        CommandPaletteMode::CommandRunner,
    ));
    println!(
        "\n[COMMAND PALETTE]: {} commands available",
        app.filtered_palette_commands().len()
    );
    let _ = app.update(DesktopMessage::TerminalCommandChanged(
        "cargo test --workspace".to_string(),
    ));
    let _ = app.update(DesktopMessage::ExecuteTerminalCommand);
    println!(
        "[TERMINAL EMULATOR]: {:?}",
        app.terminal.lock().visible_lines()
    );
    println!("================================================================");
}

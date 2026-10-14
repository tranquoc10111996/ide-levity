//! End-to-end tests against real shells on real PTYs.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ide_terminal::{
    encode_key, KeyInput, Rgb, SelectionKind, TermKey, TermModeFlags, TerminalEvent,
    TerminalOptions, TerminalPalette, TerminalSession, TerminalSnapshot, ViewportPoint,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Spawns a predictable bash (no rc files, fixed prompt).
fn bash(cols: u16, rows: u16) -> TerminalSession {
    let env = HashMap::from([("PS1".to_owned(), "$ ".to_owned())]);
    TerminalSession::spawn(TerminalOptions {
        shell: Some("/bin/bash".to_owned()),
        args: vec!["--norc".to_owned(), "--noprofile".to_owned()],
        env,
        cols,
        rows,
        ..TerminalOptions::default()
    })
    .expect("spawn bash")
}

/// Polls snapshots until `pred` holds, panicking with the screen content on timeout.
fn wait_until(
    session: &TerminalSession,
    what: &str,
    pred: impl Fn(&TerminalSnapshot) -> bool,
) -> TerminalSnapshot {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let snap = session.snapshot();
        if pred(&snap) {
            return snap;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}; screen:\n{}", snap.text());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_text(session: &TerminalSession, needle: &str) -> TerminalSnapshot {
    wait_until(session, needle, |s| s.text().contains(needle))
}

fn wait_for_prompt(session: &TerminalSession) {
    wait_until(session, "prompt", |s| {
        s.text().lines().any(|l| l.starts_with('$'))
    });
}

fn find_row(snap: &TerminalSnapshot, exact: &str) -> Option<usize> {
    (0..snap.rows).find(|&r| snap.row_text(r) == exact)
}

fn wait_event(
    rx: &Receiver<TerminalEvent>,
    what: &str,
    pred: impl Fn(&TerminalEvent) -> bool,
) -> TerminalEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(event) if pred(&event) => return event,
            Ok(_) => {}
            Err(err) => panic!("no {what} event: {err}"),
        }
    }
}

/// Returns `true` if the process no longer exists (fully reaped, not a zombie).
fn is_gone(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

/// Returns `true` if the process is dead (gone or a zombie owned by someone else).
fn is_dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Finds a child process of `parent` with the given command name.
fn find_child(parent: u32, comm: &str) -> Option<u32> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((head, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let name = head.split_once('(').map(|(_, n)| n).unwrap_or_default();
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok());
        if name == comm && ppid == Some(parent) {
            return Some(pid);
        }
    }
    None
}

#[test]
fn echo_arithmetic_appears_in_snapshot() {
    let session = bash(80, 24);
    session.write_str("echo LEVITY_$((40+2))\r");
    let snap = wait_for_text(&session, "LEVITY_42");
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
    assert!(find_row(&snap, "LEVITY_42").is_some());
}

#[test]
fn default_shell_spawns_and_runs_commands() {
    let mut session =
        TerminalSession::spawn(TerminalOptions::default()).expect("spawn default shell");
    session.write_str("echo LEVITY_\"\"OK\r");
    wait_for_text(&session, "LEVITY_OK");
    session.shutdown();
    assert!(session.has_exited());
}

#[test]
fn environment_and_working_directory_are_applied() {
    let dir = std::env::temp_dir().join(format!("ide-terminal-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let session = TerminalSession::spawn(TerminalOptions {
        shell: Some("/bin/sh".to_owned()),
        working_dir: Some(dir.clone()),
        env: HashMap::from([("LEVITY_VAR".to_owned(), "xyz".to_owned())]),
        ..TerminalOptions::default()
    })
    .unwrap();
    session.write_str("echo \"V=$LEVITY_VAR T=$TERM C=$COLORTERM\"; echo \"D=$(pwd)\"\r");
    wait_for_text(&session, "V=xyz T=xterm-256color C=truecolor");
    wait_for_text(&session, &format!("D={}", dir.display()));
    drop(session);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sgr_colors_resolve_to_palette() {
    let session = bash(80, 24);
    let palette = TerminalPalette::default();
    session.write_str(
        "printf '\\033[31mRED\\033[0m\\n\\033[38;2;10;20;30mTRUE\\033[0m\\n\\033[38;5;196mIDX\\033[0m\\n\\033[1;31mBRED\\033[0m\\n\\033[7mINV\\033[0m\\n'\r",
    );
    let snap = wait_until(&session, "colored rows", |s| find_row(s, "INV").is_some());

    let red = find_row(&snap, "RED").expect("RED row");
    for col in 0..3 {
        assert_eq!(snap.cells[red][col].fg, palette.ansi[1], "RED col {col}");
        assert_eq!(snap.cells[red][col].bg, palette.background);
    }
    let truecolor = find_row(&snap, "TRUE").unwrap();
    assert_eq!(snap.cells[truecolor][0].fg, Rgb::new(10, 20, 30));
    let indexed = find_row(&snap, "IDX").unwrap();
    assert_eq!(snap.cells[indexed][0].fg, Rgb::new(255, 0, 0));
    let bold = find_row(&snap, "BRED").unwrap();
    assert!(snap.cells[bold][0].bold);
    assert_eq!(snap.cells[bold][0].fg, palette.ansi[9]);
    let inverse = find_row(&snap, "INV").unwrap();
    assert_eq!(snap.cells[inverse][0].fg, palette.background);
    assert_eq!(snap.cells[inverse][0].bg, palette.foreground);
    assert_eq!(snap.background, Rgb::from_hex(0x0D1117));
    assert_eq!(snap.foreground, Rgb::from_hex(0xE6EDF3));
}

#[test]
fn resize_updates_grid_and_pty_size() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.resize(100, 30, 8, 16);
    let snap = session.snapshot();
    assert_eq!((snap.cols, snap.rows), (100, 30));
    session.write_str("stty size; echo COLS=$(tput cols 2>/dev/null || echo none)\r");
    wait_for_text(&session, "30 100");
    let snap = wait_for_text(&session, "COLS=");
    let text = snap.text();
    assert!(
        text.contains("COLS=100") || text.contains("COLS=none"),
        "{text}"
    );
}

#[test]
fn ctrl_c_interrupts_foreground_job() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.write_str("sleep 100\r");
    let pid = session.pid().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while find_child(pid, "sleep").is_none() {
        assert!(Instant::now() < deadline, "sleep never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = Instant::now();
    assert!(session.send_key(KeyInput::ctrl(TermKey::Char('c'))));
    session.write_str("echo AFTER_$((1+1))\r");
    wait_for_text(&session, "AFTER_2");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(find_child(pid, "sleep").is_none());
}

#[test]
fn exit_code_is_reported() {
    let mut session = bash(80, 24);
    let events = session.take_events().unwrap();
    wait_for_prompt(&session);
    session.write_str("exit 3\r");
    let event = wait_event(&events, "exit", |e| matches!(e, TerminalEvent::Exited(_)));
    assert_eq!(event, TerminalEvent::Exited(Some(3)));
    assert!(session.has_exited());
    assert_eq!(session.exit_code(), Some(3));
    let pid = session.pid().unwrap();
    session.shutdown();
    assert!(is_gone(pid), "shell {pid} not reaped");
}

#[test]
fn killed_shell_reports_no_exit_code() {
    let mut session = bash(80, 24);
    let events = session.take_events().unwrap();
    wait_for_prompt(&session);
    session.write_str("kill -9 $$\r");
    let event = wait_event(&events, "exit", |e| matches!(e, TerminalEvent::Exited(_)));
    assert_eq!(event, TerminalEvent::Exited(None));
}

#[test]
fn cat_echoes_interactive_input() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.write_str("cat\r");
    std::thread::sleep(Duration::from_millis(100));
    session.write_str("hello-cat\r");
    // Once from the tty echo, once from cat itself.
    wait_until(&session, "two hello-cat lines", |s| {
        s.text().matches("hello-cat").count() >= 2
    });
    session.write_bytes(
        &encode_key(KeyInput::ctrl(TermKey::Char('d')), TermModeFlags::default()).unwrap(),
    );
    session.write_str("echo CAT_$((2*3))\r");
    wait_for_text(&session, "CAT_6");
}

#[test]
fn shutdown_reaps_shell_and_foreground_job() {
    let mut session = bash(80, 24);
    wait_for_prompt(&session);
    let shell = session.pid().unwrap();
    session.write_str("sleep 1000\r");
    let deadline = Instant::now() + TIMEOUT;
    let sleep = loop {
        if let Some(pid) = find_child(shell, "sleep") {
            break pid;
        }
        assert!(Instant::now() < deadline, "sleep never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    let started = Instant::now();
    session.shutdown();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        is_gone(shell),
        "shell {shell} is still present (zombie or alive)"
    );
    let deadline = Instant::now() + TIMEOUT;
    while !is_dead(sleep) {
        assert!(
            Instant::now() < deadline,
            "foreground job {sleep} survived shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(session.has_exited());
    // Idempotent and inputs after shutdown are harmless.
    session.shutdown();
    session.write_str("echo ignored\r");
    let _ = session.snapshot();
}

#[test]
fn shutdown_escalates_when_shell_ignores_sighup() {
    let mut session = bash(80, 24);
    wait_for_prompt(&session);
    let shell = session.pid().unwrap();
    session.write_str("trap '' HUP; echo TRAPPED\r");
    wait_for_text(&session, "TRAPPED");
    session.shutdown();
    assert!(is_gone(shell), "shell {shell} survived");
}

#[test]
fn drop_reaps_shell_in_background() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    let shell = session.pid().unwrap();
    drop(session);
    let deadline = Instant::now() + TIMEOUT;
    while !is_gone(shell) {
        assert!(
            Instant::now() < deadline,
            "shell {shell} not reaped after drop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn title_bell_and_wakeup_events() {
    let mut session = bash(80, 24);
    let events = session.take_events().unwrap();
    wait_event(&events, "wakeup", |e| *e == TerminalEvent::Wakeup);
    session.snapshot();
    session.write_str("printf '\\033]0;LevityTitle\\007'; printf '\\a'\r");
    wait_event(&events, "title", |e| {
        *e == TerminalEvent::Title("LevityTitle".into())
    });
    wait_event(&events, "bell", |e| *e == TerminalEvent::Bell);
    assert_eq!(session.title(), "LevityTitle");
    assert_eq!(session.snapshot().title, "LevityTitle");
}

#[test]
fn osc52_copy_reaches_clipboard_event() {
    let mut session = bash(80, 24);
    let events = session.take_events().unwrap();
    // "hello" in base64.
    session.write_str("printf '\\033]52;c;aGVsbG8=\\007'\r");
    let event = wait_event(&events, "clipboard", |e| {
        matches!(e, TerminalEvent::ClipboardStore(_))
    });
    assert_eq!(event, TerminalEvent::ClipboardStore("hello".into()));
}

#[test]
fn terminal_queries_are_answered_through_the_pty() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    // OSC 11 background query: the reply is written back to the PTY by the session.
    session.write_str(
        "stty raw -echo; printf '\\033]11;?\\007'; R=$(head -c 24 | tr '\\033\\007' 'EB'); stty sane; echo \"REPLY=$R\"\r",
    );
    wait_for_text(&session, "REPLY=E]11;rgb:0d0d/1111/1717B");
    // DSR cursor position report.
    session.write_str("stty raw -echo; printf '\\033[6n'; R=$(head -c 1 | tr '\\033' 'E'; IFS= read -r -d R x; echo \"$x\"); stty sane; echo \"CPR=$R\"\r");
    wait_until(&session, "cursor report", |s| {
        s.text().lines().any(|l| l.starts_with("CPR=E["))
    });
}

#[test]
fn bracketed_paste_and_modes() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.write_str("stty -echo; printf '\\033[?2004h\\033[?1h'; cat -v\r");
    wait_until(&session, "bracketed mode", |s| {
        s.mode.bracketed_paste && s.mode.app_cursor
    });
    session.paste("a\nb\x1bX");
    session.write_str("\r");
    wait_for_text(&session, "^[[200~a");
    wait_for_text(&session, "bX^[[201~");
}

#[test]
fn plain_paste_converts_newlines() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    // Bash disables bracketed paste while running a command, so `cat` gets a plain paste.
    session.write_str("stty -echo; cat\r");
    wait_until(&session, "bracketed paste off", |s| !s.mode.bracketed_paste);
    std::thread::sleep(Duration::from_millis(100));
    session.paste("first-line\nsecond-line\r\nthird-line\n");
    let snap = wait_until(&session, "pasted lines", |s| {
        find_row(s, "third-line").is_some()
    });
    assert!(find_row(&snap, "first-line").is_some());
    assert!(find_row(&snap, "second-line").is_some());
}

#[test]
fn readline_prompt_receives_bracketed_paste() {
    let session = bash(80, 24);
    // Bash 5.1+ readline turns bracketed paste on at the prompt: pasted newlines must not
    // execute the command until Enter is pressed.
    let has_bracketed = wait_until(&session, "prompt", |s| s.text().starts_with('$'))
        .mode
        .bracketed_paste;
    session.paste("echo PASTE_$((5+5))\n");
    if has_bracketed {
        std::thread::sleep(Duration::from_millis(200));
        assert!(!session.snapshot().text().contains("PASTE_10"));
    }
    session.send_key(KeyInput::plain(TermKey::Enter));
    wait_for_text(&session, "PASTE_10");
}

#[test]
fn scrollback_scrolls_and_returns_to_bottom() {
    let session = bash(40, 10);
    wait_for_prompt(&session);
    session.write_str("seq 1 200\r");
    wait_until(&session, "seq output", |s| find_row(s, "200").is_some());
    session.scroll(50);
    let snap = session.snapshot();
    assert_eq!(snap.display_offset, 50);
    assert!(snap.history_size >= 190);
    assert!(find_row(&snap, "200").is_none());
    assert!(snap.cursor.is_none(), "cursor is below the viewport");
    session.scroll_to_bottom();
    let snap = session.snapshot();
    assert_eq!(snap.display_offset, 0);
    assert!(find_row(&snap, "200").is_some());
    // Typing jumps back to the bottom.
    session.scroll(20);
    session.write_str(" ");
    assert_eq!(session.snapshot().display_offset, 0);
    // The wheel scrolls the history in the primary screen.
    session.wheel(3, ViewportPoint::new(0, 0));
    assert_eq!(session.snapshot().display_offset, 3);
}

#[test]
fn selection_copies_text() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.write_str("echo 'alpha beta gamma'\r");
    let snap = wait_until(&session, "output row", |s| {
        find_row(s, "alpha beta gamma").is_some()
    });
    let row = find_row(&snap, "alpha beta gamma").unwrap();

    session.select(
        ViewportPoint::new(row, 6),
        ViewportPoint::new(row, 9),
        SelectionKind::Simple,
    );
    assert_eq!(session.selection_text().as_deref(), Some("beta"));
    let snap = session.snapshot();
    assert!(snap.cells[row][6].selected && snap.cells[row][9].selected);
    assert!(!snap.cells[row][5].selected && !snap.cells[row][10].selected);
    assert_eq!(
        snap.cells[row][7].bg,
        TerminalPalette::default().selection_background
    );

    // Reversed points select the same range.
    session.select(
        ViewportPoint::new(row, 9),
        ViewportPoint::new(row, 6),
        SelectionKind::Simple,
    );
    assert_eq!(session.selection_text().as_deref(), Some("beta"));

    session.select(
        ViewportPoint::new(row, 12),
        ViewportPoint::new(row, 12),
        SelectionKind::Semantic,
    );
    assert_eq!(session.selection_text().as_deref(), Some("gamma"));

    session.select(
        ViewportPoint::new(row, 3),
        ViewportPoint::new(row, 3),
        SelectionKind::Lines,
    );
    assert_eq!(
        session.selection_text().as_deref(),
        Some("alpha beta gamma\n")
    );

    session.clear_selection();
    assert!(!session.has_selection());
    assert_eq!(session.selection_text(), None);
}

#[test]
fn unicode_and_wide_characters() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    session.write_str("printf 'Ti\\xe1\\xba\\xbfng Vi\\xe1\\xbb\\x87t \\xe4\\xb8\\xad\\xe6\\x96\\x87 \\xf0\\x9f\\x98\\x80\\n'\r");
    let snap = wait_until(&session, "unicode row", |s| {
        find_row(s, "Tiếng Việt 中文 😀").is_some()
    });
    let row = find_row(&snap, "Tiếng Việt 中文 😀").unwrap();
    let cells = &snap.cells[row];
    assert_eq!(cells[11].ch, '中');
    assert!(cells[11].wide);
    assert!(cells[12].wide_spacer);
    assert_eq!(cells[13].ch, '文');
    assert_eq!(cells[16].ch, '😀');
}

#[test]
fn alt_screen_wheel_sends_arrow_keys() {
    let session = bash(80, 24);
    wait_for_prompt(&session);
    // Enter the alternate screen with alternate-scroll, then read three keys.
    session.write_str("printf '\\033[?1049h\\033[?1007h'; stty raw -echo; K=$(head -c 3 | tr '\\033' 'E'); stty sane; printf '\\033[?1049l'; echo \"KEY=$K\"\r");
    wait_until(&session, "alt screen", |s| {
        s.mode.alt_screen && s.mode.alternate_scroll
    });
    session.wheel(1, ViewportPoint::new(5, 5));
    wait_for_text(&session, "KEY=E[A");
}

#[test]
fn snapshot_is_cheap_for_large_grids() {
    let session = bash(120, 40);
    session.write_str("for i in $(seq 1 60); do printf '\\033[3%dm%s\\033[0m\\n' $((i%8)) \"line $i with some text to fill the columns\"; done\r");
    wait_until(&session, "output", |s| s.text().contains("line 60"));
    let started = Instant::now();
    let iterations = 200;
    for _ in 0..iterations {
        let snap = session.snapshot();
        assert_eq!(snap.cells.len(), 40);
    }
    let per_frame = started.elapsed() / iterations;
    // Generous bound (debug build, shared CI machines); release builds are far faster.
    assert!(
        per_frame < Duration::from_millis(8),
        "snapshot took {per_frame:?}"
    );
}

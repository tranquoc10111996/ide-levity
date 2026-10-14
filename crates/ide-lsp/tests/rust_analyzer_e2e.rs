use ide_lsp::{
    LanguageServerManager, LspEvent, ManagerOptions, OpenOutcome, ServerAvailability, ServerState,
};
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn real_rust_analyzer_diagnostics_hover_and_definition() {
    let options = ManagerOptions {
        completion_timeout: Duration::from_secs(30),
        request_timeout: Duration::from_secs(30),
        formatting_timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let (manager, mut events) = LanguageServerManager::new(options).expect("create manager");

    if !matches!(manager.availability("rust"), ServerAvailability::Available { .. }) {
        eprintln!("rust-analyzer not installed on PATH; skipping real E2E test");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "ra_e2e_fixture"
version = "0.1.0"
edition = "2021"
"#,
    )
    .expect("write Cargo.toml");

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).expect("create src");
    let lib_rs = src_dir.join("lib.rs");
    let code = r#"/// Adds two numbers together.
pub fn compute_sum(a: u32, b: u32) -> u32 {
    a + b
}

pub fn caller() -> u32 {
    let bad: u32 = "not_a_number";
    compute_sum(10, 20) + bad
}
"#;
    fs::write(&lib_rs, code).expect("write lib.rs");

    let outcome = manager.did_open(&lib_rs, "rust", 1, code);
    assert!(
        matches!(outcome, OpenOutcome::Attached(_)),
        "expected Attached outcome, got {outcome:?}"
    );

    // Wait up to 45 seconds for rust-analyzer to finish indexing and publish diagnostics for lib.rs
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut got_type_error_diag = false;
    let mut saw_ready = false;

    while Instant::now() < deadline && (!got_type_error_diag || !saw_ready) {
        while let Ok(ev) = events.try_recv() {
            match ev {
                LspEvent::Diagnostics { path, items, .. }
                    if path.ends_with("lib.rs") && !items.is_empty() =>
                {
                    if items.iter().any(|d| {
                        d.message.contains("mismatched types")
                            || d.message.contains("expected `u32`")
                    }) {
                        got_type_error_diag = true;
                    }
                }
                LspEvent::ServerStatus {
                    state: ServerState::Ready,
                    ..
                } => {
                    saw_ready = true;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        got_type_error_diag,
        "expected rust-analyzer to publish mismatched types diagnostic for lib.rs"
    );

    // Test go-to-definition on `compute_sum` call at line 7 (0-indexed), col 6
    let defs = manager
        .definition(&lib_rs, 7, 6)
        .wait()
        .expect("definition request");
    assert!(
        !defs.is_empty(),
        "expected definition location for compute_sum"
    );
    assert!(defs[0].path.ends_with("lib.rs"));
    assert_eq!(defs[0].range.start.line, 1);

    // Test hover on `compute_sum` call at line 7, col 6
    let hover = manager
        .hover(&lib_rs, 7, 6)
        .wait()
        .expect("hover request")
        .expect("hover text present");
    assert!(
        hover.contains("compute_sum") || hover.contains("Adds two numbers"),
        "unexpected hover text: {hover}"
    );

    manager.shutdown_all();
}

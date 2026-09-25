use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_bodysnatcher")
}

/// A convert with an undetectable source must fail (non-zero exit).
/// Kills the "main -> ExitCode::default()" and "convert -> Ok(())" mutants,
/// both of which would make a failing convert exit 0.
#[test]
fn convert_error_exits_nonzero() {
    let out = Command::new(bin())
        .args(["convert", "/nonexistent/input.jsonl", "--to", "omp"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "convert of an unknown source should fail"
    );
}

/// TUI mode with no sessions in the current dir must exit non-zero before
/// touching the terminal. Kills the "run -> Ok(())" mutant, which would exit 0.
#[test]
fn tui_with_no_sessions_exits_nonzero() {
    let dir = std::env::temp_dir().join(format!("bs-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(bin()).current_dir(&dir).output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        !out.status.success(),
        "TUI with no sessions should exit non-zero"
    );
}

/// `--dry-run` prints the destination but must not create it.
#[test]
fn convert_dry_run_writes_nothing() {
    let dir = std::env::temp_dir().join(format!("bs-dry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("s.jsonl");
    std::fs::write(
        &src,
        r#"{"type":"session","version":3,"id":"s","cwd":"/w"}
{"type":"message","id":"a","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"#,
    )
    .unwrap();
    let out_dir = dir.join("out");
    let out = Command::new(bin())
        .args([
            "convert",
            src.to_str().unwrap(),
            "--from",
            "pi",
            "--to",
            "claude",
        ])
        .args(["--sessions-dir", out_dir.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    let exists = out_dir.exists();
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(".jsonl"));
    assert!(!exists, "dry run created the output dir");
}

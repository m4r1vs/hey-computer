//! The command line as a voice front end runs it: no terminal, so every
//! refusal of an `ask` must also be a notification.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hc-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `hey-computer ask ARGS` with `stdin`, a notifier that records its
/// calls, and a socket nobody listens on. Returns the exit code, stderr and
/// the recorded notifications.
fn ask(name: &str, args: &[&str], stdin: &[u8]) -> (Option<i32>, String, String) {
    let dir = scratch(name);
    let calls = dir.join("notify.log");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[present]\nnotifier = [\"/bin/sh\", \"-c\", \"echo \\\"$*\\\" >> '{}'; echo 7\", \"sh\"]\n",
            calls.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .arg("--socket")
        .arg(dir.join("nobody.sock"))
        .arg("ask")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    // The client stops reading at the limit; the rest may not be taken.
    let _ = input.write_all(stdin);
    drop(input);
    let output = child.wait_with_output().unwrap();
    let notified = std::fs::read_to_string(&calls).unwrap_or_default();
    std::fs::remove_dir_all(&dir).unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        notified,
    )
}

#[test]
fn a_request_too_long_for_the_daemon_is_refused_with_a_notification() {
    // Between the daemon's 256 KiB and the old 1 MiB stdin limit this used
    // to fail on stderr only.
    let (code, stderr, notified) = ask("long", &[], &vec![b'a'; 400_000]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("262144"), "{stderr}");
    assert_eq!(notified.lines().count(), 1, "{notified}");
    assert!(notified.contains("did not take the request"), "{notified}");

    // Just under the limit as text, but over it once escaped as JSON.
    let quotes = vec![b'"'; 200_000];
    let (code, stderr, notified) = ask("escaped", &[], &quotes);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("more than"), "{stderr}");
    assert_eq!(notified.lines().count(), 1, "{notified}");
}

#[test]
fn a_daemon_that_is_not_running_is_a_notification_but_an_empty_request_is_not() {
    let (code, stderr, notified) = ask("down", &["what", "time", "is", "it"], b"");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("not running"), "{stderr}");
    assert!(
        notified.contains("The assistant is not running"),
        "{notified}"
    );

    // With --print the caller reads stderr; a missing daemon is still shown.
    let (code, _, notified) = ask("down-print", &["--print", "hello"], b"");
    assert_eq!(code, Some(1));
    assert_eq!(notified.lines().count(), 1, "{notified}");

    let (code, stderr, notified) = ask("empty", &[], b"  \n");
    assert_eq!(code, Some(2), "{stderr}");
    assert!(notified.is_empty(), "{notified}");
}

#[test]
fn a_viewer_outlives_the_daemon_even_when_it_writes_to_stderr() {
    // A viewer's stderr was a pipe into the daemon: its first write after a
    // restart of the daemon killed it with SIGPIPE, and its window with it.
    let dir = scratch("outlives");
    let mark = dir.join("still-alive");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[present]\nnotifications = false\nviewer = [\"/bin/sh\", \"-c\", \"sleep 2; echo late >&2; sleep 0.5; echo ok > '{}'\", \"sh\"]\n",
            mark.display()
        ),
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .args(["debug", "present", "# A long answer"])
        .env("XDG_RUNTIME_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!mark.exists(), "the viewer is still running");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !mark.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(mark.exists(), "the viewer died after the daemon left");
    // Nothing but the answer is left behind.
    let left: Vec<_> = std::fs::read_dir(dir.join("hey-computer"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.len() == 1 && left[0].ends_with(".md"), "{left:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn last_reports_missing_answer_and_notifies_only_for_copy_and_open() {
    let dir = scratch("last-missing");
    let calls = dir.join("notify.log");
    let state = dir.join("state");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = \"{}\"\n[present]\nnotifier = [\"/bin/sh\", \"-c\", \"echo \\\"$*\\\" >> '{}'; echo 7\", \"sh\"]\n",
            state.display(),
            calls.display()
        ),
    )
    .unwrap();

    // Plain `last` exits 1 without popping up a notification.
    let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .arg("last")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no answer recorded yet"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap_or_default()
            .is_empty()
    );

    // `--copy` and `--open` exit 1 and notify the user.
    for flag in ["--copy", "--open"] {
        let _ = std::fs::remove_file(&calls);
        let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
            .arg("--config")
            .arg(&config)
            .args(["last", flag])
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let notified = std::fs::read_to_string(&calls).unwrap_or_default();
        assert_eq!(notified.lines().count(), 1, "{flag}: {notified}");
        assert!(
            notified.contains("No previous answer"),
            "{flag}: {notified}"
        );
    }

    // `--copy` and `--open` together are refused by clap (exit 2).
    let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .args(["last", "--copy", "--open"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn last_prints_copies_and_opens_the_saved_answer() {
    let dir = scratch("last-retrieve");
    let calls = dir.join("notify.log");
    let state = dir.join("state");
    let copied = dir.join("copied.txt");
    let opened = dir.join("opened.md");
    let config = dir.join("config.toml");

    hey_computer::store::save_last(
        &state,
        &hey_computer::store::LastAnswer {
            version: hey_computer::store::SCHEMA_VERSION,
            id: 42,
            timestamp: 1_700_000_000,
            request: "show raw".to_string(),
            text: "hi <script>alert(1)</script>".to_string(),
            denied: vec![],
            attachments: vec![],
        },
    )
    .unwrap();

    std::fs::write(
        &config,
        format!(
            "state_dir = \"{}\"\n\
             [present]\n\
             notifier = [\"/bin/sh\", \"-c\", \"echo \\\"$*\\\" >> '{}'; echo 7\", \"sh\"]\n\
             clipboard = [\"/bin/sh\", \"-c\", \"cat > '{}'; (sleep 3) & exit 0\", \"sh\"]\n\
             viewer = [\"/bin/sh\", \"-c\", \"cp \\\"$1\\\" '{}'\", \"sh\", \"{{file}}\"]\n",
            state.display(),
            calls.display(),
            copied.display(),
            opened.display()
        ),
    )
    .unwrap();

    // Stdout mode prints raw text and works without XDG_RUNTIME_DIR.
    let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .arg("last")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi <script>alert(1)</script>\n"
    );

    // `--copy` writes exact raw bytes without hanging on forked child.
    let started = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .args(["last", "--copy"])
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(
        std::fs::read_to_string(&copied).unwrap(),
        "hi <script>alert(1)</script>"
    );
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap()
            .contains("Copied last answer")
    );

    // `--open` disarms HTML and opens the viewer even for a short answer.
    let out = Command::new(env!("CARGO_BIN_EXE_hey-computer"))
        .arg("--config")
        .arg(&config)
        .args(["last", "--open"])
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rendered = std::fs::read_to_string(&opened).unwrap();
    assert!(
        rendered.contains("&lt;script>alert(1)&lt;/script>"),
        "{rendered}"
    );
    assert!(
        std::fs::read_to_string(&calls)
            .unwrap()
            .contains("Answer opened")
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

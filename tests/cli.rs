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

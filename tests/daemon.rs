//! The daemon end to end, with the scripted mock agent in place of agy.
//!
//! Each test starts the daemon in-process on its own socket, with a recording
//! presenter, and the agent command pointing at `hey-computer debug
//! mock-agent`. The mock logs what it saw, which is how a test checks what the
//! daemon sent it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hey_computer::config::{Config, Context as ContextConfig};
use hey_computer::context::ContextProvider;
use hey_computer::daemon;
use hey_computer::ipc::{self, Client};
use hey_computer::present::{Presentation, Presenter, Recorder};
use hey_computer::protocol::{AgentState, FailReason, InputSource, Reply, Request};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const SCRIPT: &str = r##"{
  "turns": [
    {"match": "plain", "steps": [{"chunk": "Hello "}, {"sleep_ms": 20}, {"chunk": "from "}, {"chunk": "the mock."}]},
    {"match": "shell", "steps": [{"permission": {"command": "hyprctl version"}}, {"chunk": "Hyprland 0.55.0"}]},
    {"match": "root", "steps": [{"permission": {"command": "sudo -n true"}}, {"chunk": "never sent"}]},
    {"match": "turn", "steps": ["turn_info"]},
    {"match": "slow", "steps": [{"chunk": "thinking"}, "hang"]},
    {"match": "crash", "steps": [{"chunk": "partial"}, {"exit": 3}]},
    {"match": "garbage", "steps": [{"raw": "this is { not json"}, {"raw": "[1, 2]"}, {"huge": 34000000}, {"chunk": "still fine"}]},
    {"match": "silent", "steps": []},
    {"match": "long", "steps": [{"chunk": "# Title\n\nA table:\n\n| a | b |\n|---|---|\n| 1 | 2 |\n"}]}
  ],
  "default": ["echo"]
}"##;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    dir: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    recorder: Arc<Recorder>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Harness {
    async fn start(script: &str, tweak: impl FnOnce(&mut Config)) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Short: a socket path must fit in 108 bytes.
        let dir = std::env::temp_dir().join(format!("hc-t{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("script.json");
        std::fs::write(&script_path, script).unwrap();
        let log = dir.join("mock.log");
        let socket = dir.join("s.sock");

        let mut config = Config {
            state_dir: dir.join("state"),
            ..Config::default()
        };
        config.agent.command = vec![
            env!("CARGO_BIN_EXE_hey-computer").to_string(),
            "debug".to_string(),
            "mock-agent".to_string(),
            script_path.to_string_lossy().into_owned(),
            "--log".to_string(),
            log.to_string_lossy().into_owned(),
        ];
        config.agent.inactivity_timeout_secs = 5.0;
        config.agent.restart_delay_secs = 0.1;
        config.conversation.follow_up_secs = 30.0;
        tweak(&mut config);

        let recorder = Recorder::new();
        let presenter: Arc<dyn Presenter> = recorder.clone();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn({
            let socket = socket.clone();
            async move {
                daemon::serve(config, &socket, presenter, async {
                    let _ = stopped.await;
                })
                .await
            }
        });
        let harness = Self {
            dir,
            socket,
            log,
            recorder,
            stop: Some(stop),
            task: Some(task),
        };
        harness
            .eventually("the socket", || harness.socket.exists())
            .await;
        harness
    }

    /// Start with the default script and wait for the warm agent's spare
    /// session, so tests do not race the start.
    async fn warm() -> Self {
        let harness = Self::start(SCRIPT, |_| {}).await;
        harness.wait_log("new mock-1").await;
        harness
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn log_count(&self, needle: &str) -> usize {
        self.log_lines()
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    async fn wait_log(&self, needle: &str) {
        self.eventually(needle, || self.log_count(needle) > 0).await;
    }

    async fn eventually(&self, what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; mock log:\n{}",
                self.log_lines().join("\n")
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Ask and wait for the answer, as `ask --print` does.
    async fn ask(&self, text: &str, new: bool) -> Reply {
        let (id, mut client) = ipc::ask(&self.socket, text, true, new).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(20), client.next(None))
            .await
            .expect("an answer in time")
            .unwrap();
        match &reply {
            Reply::Answer { id: got, .. } | Reply::Failed { id: got, .. } => assert_eq!(*got, id),
            other => panic!("unexpected reply {other:?}"),
        }
        reply
    }

    /// Ask in the background; returns the client still waiting.
    async fn ask_waiting(&self, text: &str) -> (u64, Client) {
        ipc::ask(&self.socket, text, true, false).await.unwrap()
    }

    async fn status(&self) -> hey_computer::protocol::Status {
        match ipc::request(&self.socket, &Request::Status).await.unwrap() {
            Reply::Status(status) => status,
            other => panic!("{other:?}"),
        }
    }

    /// Stop the daemon as SIGTERM does; returns how long that took.
    async fn shut_down(&mut self) -> Duration {
        let started = Instant::now();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .expect("the daemon stops")
                .unwrap()
                .unwrap();
        }
        started.elapsed()
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the daemon stops")
                .unwrap()
                .unwrap();
        }
        assert!(!self.socket.exists(), "the socket is removed on shutdown");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn answer(reply: &Reply) -> &str {
    match reply {
        Reply::Answer { text, .. } => text,
        other => panic!("expected an answer, got {other:?}"),
    }
}

fn failure(reply: &Reply) -> (FailReason, &str) {
    match reply {
        Reply::Failed {
            reason, message, ..
        } => (*reason, message),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_answer_is_assembled_from_streamed_chunks() {
    let h = Harness::warm().await;
    let reply = h.ask("plain question", false).await;
    assert_eq!(answer(&reply), "Hello from the mock.");
    // The session was configured as the config says, and switched out of the
    // mock's "auto" mode into "default".
    for line in [
        "set model=gemini-4-hydrogen",
        "set thought_level=low",
        "set mode=default",
        "prompt mock-1 plain question",
    ] {
        assert_eq!(h.log_count(line), 1, "{line}");
    }
    // A waiting client gets the answer and nothing is shown.
    assert!(h.recorder.items().is_empty());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_wait_the_daemon_presents_progress_then_the_answer() {
    let h = Harness::warm().await;
    let (id, _client) = ipc::ask(&h.socket, "plain", false, false).await.unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| items.len() >= 2)
        .await
        .expect("two presentations");
    assert_eq!(
        items,
        vec![
            Presentation::Progress {
                id,
                request: "plain".to_string(),
                profile: None,
                thinking_label: None,
            },
            Presentation::Answer {
                id,
                text: "Hello from the mock.".to_string(),
                denied: vec![],
                profile: None,
                widgets: vec![],
                icons: vec![],
            },
        ]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_command_is_allowed() {
    let h = Harness::warm().await;
    let reply = h.ask("run a shell command", false).await;
    assert_eq!(answer(&reply), "Hyprland 0.55.0");
    assert_eq!(h.log_count("permission allow_once hyprctl version"), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_privileged_command_is_refused_and_the_daemon_says_why() {
    let h = Harness::warm().await;
    let reply = h.ask("do something as root", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::Denied);
    assert!(message.contains("sudo -n true"), "{message}");
    assert!(message.contains("deny list"), "{message}");
    assert_eq!(h.log_count("permission reject_once sudo -n true"), 1);
    // The agent stays up and the conversation goes on.
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_custom_denied_command_uses_a_generic_refusal_message() {
    let script = r#"{
        "turns": [
            {"match": "power off", "steps": [{"permission": {"command": "shutdown now"}}, {"chunk": "never"}]}
        ],
        "default": ["echo"]
    }"#;
    let h = Harness::start(script, |c| {
        c.permissions.deny.push("shutdown".to_string());
    })
    .await;
    h.wait_log("new mock-1").await;

    let reply = h.ask("power off", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::Denied);
    assert!(message.contains("shutdown now"), "{message}");
    assert!(message.contains("deny list"), "{message}");
    assert!(!message.contains("root"), "{message}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_ups_continue_inside_the_window_and_not_after() {
    let h = Harness::start(SCRIPT, |c| c.conversation.follow_up_secs = 0.8).await;
    h.wait_log("new mock-1").await;
    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-1");
    assert_eq!(answer(&h.ask("turn", false).await), "turn 2 in mock-1");
    let status = h.status().await;
    assert!(status.follow_up_secs.is_some_and(|s| s > 0.0), "{status:?}");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-2");
    h.wait_log("close mock-1").await;

    // --new starts afresh even inside the window.
    assert_eq!(answer(&h.ask("turn", true).await), "turn 1 in mock-3");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_request_cancels_the_running_one() {
    let h = Harness::warm().await;
    let (first, mut waiting) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;

    let second = h.ask("plain", false).await;
    // The mock sends a late chunk on every cancel; it must not reach the
    // next answer in the same session. With the queued request always taken
    // first, this fails every time when neither the cancel nor the start of
    // the next request reads it away.
    assert_eq!(answer(&second), "Hello from the mock.");

    let first_reply = waiting.next(Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!(
        first_reply,
        Reply::Failed {
            id: first,
            reason: FailReason::Superseded,
            message: "A newer request replaced this one.".to_string()
        }
    );
    assert_eq!(h.log_count("cancel mock-1"), 1);
    // The cancelled turn finished before the new one started.
    let lines = h.log_lines();
    let cancelled = lines.iter().position(|l| l == "turn cancelled").unwrap();
    let next = lines
        .iter()
        .position(|l| l == "prompt mock-1 plain")
        .unwrap();
    assert!(cancelled < next, "{lines:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_the_running_request() {
    let h = Harness::warm().await;
    let (id, mut waiting) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;
    assert_eq!(h.status().await.running, Some(id));

    let reply = ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Cancelled { id: Some(id) });
    let reply = waiting.next(Some(Duration::from_secs(5))).await.unwrap();
    let (reason, _) = failure(&reply);
    assert_eq!(reason, FailReason::Cancelled);

    // Nothing left to cancel.
    let reply = ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Cancelled { id: None });
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_client_that_hangs_up_cancels_its_request() {
    let h = Harness::warm().await;
    let (_, waiting) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;
    drop(waiting);
    h.wait_log("cancel mock-1").await;
    h.eventually("the request to end", || h.log_count("turn cancelled") == 1)
        .await;
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_agent_is_reported_and_restarted() {
    let h = Harness::warm().await;
    let reply = h.ask("crash now", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::AgentDown);
    assert!(message.contains("exit status: 3"), "{message}");

    // Warm start brings it back on its own, with a fresh spare session.
    h.eventually("a restart", || h.log_count("start") == 2)
        .await;
    h.eventually("ready", || h.log_count("new mock-1") == 2)
        .await;
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_that_goes_quiet_is_timed_out() {
    let h = Harness::start(SCRIPT, |c| c.agent.inactivity_timeout_secs = 0.5).await;
    h.wait_log("new mock-1").await;
    // Open a follow-up window on mock-1 first so we can confirm the timeout clears it.
    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-1");
    assert!(h.status().await.follow_up_secs.is_some());

    let started = Instant::now();
    let reply = h.ask("slow", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::Timeout);
    assert!(message.contains("sent nothing"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(h.log_count("cancel mock-1"), 1);
    // The timed-out session is closed and the follow-up window cleared.
    assert_eq!(h.status().await.follow_up_secs, None);
    h.wait_log("close mock-1").await;
    // The agent answered the cancel, so it stays up and opens a fresh session.
    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-2");
    assert_eq!(h.log_count("start"), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_oversized_lines_from_the_agent_are_skipped() {
    let h = Harness::warm().await;
    let reply = h.ask("garbage please", false).await;
    assert_eq!(answer(&reply), "still fine");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_without_text_is_reported() {
    let h = Harness::warm().await;
    let reply = h.ask("silent", false).await;
    let (reason, _) = failure(&reply);
    assert_eq!(reason, FailReason::Empty);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_answer_is_presented_for_the_viewer() {
    let h = Harness::warm().await;
    ipc::ask(&h.socket, "long", false, false).await.unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| items.len() >= 2)
        .await
        .unwrap();
    match &items[1] {
        Presentation::Answer { text, .. } => assert!(text.starts_with("# Title")),
        other => panic!("{other:?}"),
    }
    h.stop().await;
}

async fn raw_exchange(socket: &Path, line: &[u8]) -> String {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream.write_all(line).await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).await.unwrap();
    reply
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_requests_get_an_error_and_the_daemon_carries_on() {
    let h = Harness::warm().await;
    for line in [
        &b"ask what time is it\n"[..],
        b"{\"type\":\"ask\"}\n",
        b"{\"type\":\"ask\",\"text\":\"   \"}\n",
        b"\xff\xfe\n",
    ] {
        let reply: Reply = serde_json::from_str(&raw_exchange(&h.socket, line).await).unwrap();
        assert!(matches!(reply, Reply::Error { .. }), "{reply:?}");
    }
    let huge = format!(
        "{{\"type\":\"ask\",\"text\":\"{}\"}}\n",
        "a".repeat(300_000)
    );
    let reply: Reply =
        serde_json::from_str(&raw_exchange(&h.socket, huge.as_bytes()).await).unwrap();
    match reply {
        Reply::Error { message } => assert!(message.contains("bytes"), "{message}"),
        other => panic!("{other:?}"),
    }
    // A client that connects and leaves without a word.
    drop(UnixStream::connect(&h.socket).await.unwrap());

    assert_eq!(h.status().await.agent, AgentState::Ready);
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_client_is_served_while_a_request_runs() {
    let h = Harness::warm().await;
    let (id, _waiting) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;
    let status = h.status().await;
    assert_eq!(status.running, Some(id));
    assert!(status.running_secs.is_some());
    assert_eq!(status.agent, AgentState::Ready);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_refuses_the_socket() {
    let h = Harness::warm().await;
    let presenter: Arc<dyn Presenter> = Recorder::new();
    // Its own state directory: the default one is the user's.
    let config = Config {
        state_dir: h.dir.join("second"),
        ..Config::default()
    };
    let error = daemon::serve(config, &h.socket, presenter, async {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already listening"), "{error}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_login_is_reported_with_what_to_do() {
    // agy 1.5.13 without a login: session/new fails with -32000
    // "Authentication required", and nothing else.
    let script = r#"{"session_error": "Authentication required", "default": ["echo"]}"#;
    let h = Harness::start(script, |_| {}).await;
    let reply = h.ask("anything", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::AgentError);
    assert!(message.contains("(Authentication required)"), "{message}");
    assert!(message.contains("agy acp --login"), "{message}");
    assert_eq!(h.status().await.agent, AgentState::Down);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_that_appears_later_is_picked_up_without_restarting_the_daemon() {
    // agy latches a failed silent sign-in in memory and writes a 1-hour
    // keyring-unavailable marker after a keyring timeout. After the keyring
    // unlocks or the user signs in, the next request must start a fresh agent
    // with the marker cleared.
    let script = r#"{"session_error": "Authentication required", "default": ["echo"]}"#;
    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new refused").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if h.status().await.agent == AgentState::Down {
            break;
        }
        assert!(Instant::now() < deadline, "the agent never stopped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Still not signed in: reports what to do and stops the agent again.
    let reply = h.ask("anything", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::AgentError);
    assert!(message.contains("agy acp --login"), "{message}");
    assert_eq!(h.status().await.agent, AgentState::Down);

    // Simulate a stale 1h keyring-timeout marker written by agy, then the
    // login becoming available.
    let marker = h
        .dir
        .join("state/agy/antigravity-cli/cache/antigravity-keyring-unavailable");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "2026-11-23T08:58:53Z").unwrap();
    std::fs::write(h.dir.join("script.json"), SCRIPT).unwrap();

    let reply = h.ask("plain", false).await;
    assert_eq!(answer(&reply), "Hello from the mock.");
    assert!(!marker.exists(), "stale keyring marker should be cleared");
    assert_eq!(h.status().await.agent, AgentState::Ready);
    assert_eq!(h.log_count("start"), 3);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_agent_is_reported() {
    let h = Harness::start(SCRIPT, |c| {
        c.agent.command = vec!["/nonexistent/agent-binary".to_string()];
        c.agent.warm_start = false;
    })
    .await;
    let reply = h.ask("plain", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::AgentDown);
    assert!(message.contains("agent-binary"), "{message}");
    assert_eq!(h.status().await.agent, AgentState::Down);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_agent_starts_on_the_first_request_and_the_vault_stays_outside_the_workspace() {
    let h = Harness::start(SCRIPT, |c| {
        c.agent.warm_start = false;
        c.instructions.vault = PathBuf::from("/tmp/Someone's Vault");
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.log_count("start"), 0, "nothing starts before a request");
    assert_eq!(h.status().await.agent, AgentState::Down);
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    // As a workspace directory, the vault's AGENTS.md would become standing
    // instructions for the agent.
    assert_eq!(h.log_count("new mock-1"), 1, "{:?}", h.log_lines());
    assert_eq!(h.log_count("dirs="), 0, "{:?}", h.log_lines());
    let agents = std::fs::read_to_string(h.dir.join("state/workspace/AGENTS.md")).unwrap();
    assert!(agents.contains("`/tmp/Someone's Vault`"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_agent_is_stopped_and_comes_back_on_demand() {
    let h = Harness::start(SCRIPT, |c| c.agent.idle_stop_secs = 0.4).await;
    h.wait_log("new mock-1").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if h.status().await.agent == AgentState::Down {
            break;
        }
        assert!(Instant::now() < deadline, "the agent never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    assert_eq!(h.log_count("start"), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_out_of_range_is_refused_at_start() {
    // 1e19 s used to pass and then panic the conductor, leaving a daemon
    // that accepted requests and answered none.
    let dir = std::env::temp_dir().join(format!("hc-range-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let mut config = Config {
        state_dir: dir.join("state"),
        ..Config::default()
    };
    config.agent.idle_stop_secs = 1e19;
    let presenter: Arc<dyn Presenter> = Recorder::new();
    let error = daemon::serve(config, &socket, presenter, async {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("idle_stop_secs"), "{error}");
    assert!(!socket.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_cancels_the_running_request_at_once() {
    // The harness's inactivity timeout is 5 s; the shutdown must not wait
    // for it, nor for an agent that keeps working.
    let mut h = Harness::warm().await;
    let (id, mut waiting) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;
    let took = h.shut_down().await;
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    assert_eq!(
        waiting.next(Some(Duration::from_secs(5))).await.unwrap(),
        Reply::Failed {
            id,
            reason: FailReason::AgentDown,
            message: "The daemon is shutting down.".to_string()
        }
    );
    assert_eq!(h.log_count("cancel mock-1"), 1);
    assert!(!h.socket.exists());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_replaces_the_progress_popup() {
    let mut h = Harness::warm().await;
    let (id, _client) = ipc::ask(&h.socket, "slow", false, false).await.unwrap();
    h.wait_log("prompt mock-1 slow").await;
    let took = h.shut_down().await;
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    let items = h.recorder.items();
    assert_eq!(
        items.last(),
        Some(&Presentation::Failure {
            id,
            reason: FailReason::AgentDown,
            message: "The daemon is shutting down.".to_string(),
            profile: None,
            background: false,
            icons: vec![],
        }),
        "{items:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_does_not_wait_for_a_hung_start() {
    let script = r#"{"init_delay_ms": 60000, "default": ["echo"]}"#;
    let mut h = Harness::start(script, |_| {}).await;
    h.wait_log("start").await;
    let took = h.shut_down().await;
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_correction_during_a_cold_start_keeps_the_starting_agent() {
    let script = r#"{"init_delay_ms": 600, "turns": [{"match": "plain", "steps": [{"chunk": "Hello from the mock."}]}], "default": ["echo"]}"#;
    let h = Harness::start(script, |c| c.agent.warm_start = false).await;
    let (first, _client) = ipc::ask(&h.socket, "plain", false, false).await.unwrap();
    h.wait_log("start").await;
    // "No, I meant…" while the agent is still starting.
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    assert_eq!(h.log_count("start"), 1, "{:?}", h.log_lines());
    let items = h
        .recorder
        .wait_for(Duration::from_secs(5), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Failure { .. }))
        })
        .await
        .unwrap();
    assert!(
        items.contains(&Presentation::Failure {
            id: first,
            reason: FailReason::Superseded,
            message: "A newer request replaced this one.".to_string(),
            profile: None,
            background: false,
            icons: vec![],
        }),
        "{items:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_at_the_socket_path_is_not_replaced() {
    let dir = std::env::temp_dir().join(format!("hc-notsock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.md");
    std::fs::write(&path, "keep me").unwrap();
    let config = Config {
        state_dir: dir.join("state"),
        ..Config::default()
    };
    let presenter: Arc<dyn Presenter> = Recorder::new();
    let error = daemon::serve(config, &path, presenter, async {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a socket"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_snapshot_is_frozen_until_next_agent_start() {
    let dir = std::env::temp_dir().join(format!("hc-mem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let memory = dir.join("memory.md");
    std::fs::write(&memory, "fact one").unwrap();

    let h = Harness::start(SCRIPT, |c| {
        c.instructions.memory_file = memory.clone();
    })
    .await;
    h.wait_log("new mock-1").await;

    let agents_path = h.dir.join("state/workspace/AGENTS.md");
    let first = std::fs::read_to_string(&agents_path).unwrap();
    assert!(first.contains("fact one"), "{first}");

    // Editing memory.md while the agent is alive must not change AGENTS.md.
    std::fs::write(&memory, "fact two").unwrap();
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    let frozen = std::fs::read_to_string(&agents_path).unwrap();
    assert!(frozen.contains("fact one"), "{frozen}");
    assert!(!frozen.contains("fact two"), "{frozen}");

    // Restarting the agent picks up the new snapshot.
    let _ = h.ask("crash now", false).await;
    h.eventually("second start", || h.log_count("new mock-1") == 2)
        .await;
    let refreshed = std::fs::read_to_string(&agents_path).unwrap();
    assert!(refreshed.contains("fact two"), "{refreshed}");
    assert!(!refreshed.contains("fact one"), "{refreshed}");

    h.stop().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_exceeding_max_tool_calls_is_stopped() {
    let script = r##"{
      "turns": [
        {
          "match": "within_limit",
          "steps": [
            {"update": {"sessionUpdate": "tool_call", "toolCallId": "read_1", "kind": "read", "title": "Read file"}},
            {"update": {"sessionUpdate": "tool_call", "toolCallId": "read_1", "kind": "read", "title": "Read file"}},
            {"update": {"sessionUpdate": "tool_call_update", "toolCallId": "read_1", "status": "in_progress"}},
            {"update": {"sessionUpdate": "tool_call_update", "toolCallId": "read_1", "status": "completed"}},
            {"permission": {"command": "hyprctl version"}},
            {"chunk": "done"}
          ]
        },
        {
          "match": "too_many",
          "steps": [
            {"permission": {"command": "echo one"}},
            {"permission": {"command": "echo two"}},
            {"permission": {"command": "echo three"}},
            {"chunk": "never reached"}
          ]
        },
        {"match": "turn", "steps": ["turn_info"]}
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |c| c.agent.max_tool_calls = 2).await;
    h.wait_log("new mock-1").await;

    // Two distinct tool calls (with duplicate updates) stay within the limit.
    assert_eq!(answer(&h.ask("within_limit", false).await), "done");

    // Three distinct tool calls trip the limit before the 3rd is allowed.
    let reply = h.ask("too_many", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::LimitReached);
    assert!(message.contains("2 tool calls"), "{message}");
    assert_eq!(h.log_count("permission allow_once echo "), 2);
    assert_eq!(h.log_count("permission allow_once echo three"), 0);
    assert_eq!(h.log_count("cancel mock-1"), 1);
    assert_eq!(h.status().await.follow_up_secs, None);

    // The broken session is closed and the next turn gets a fresh session on the same warm agent.
    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-2");
    h.wait_log("close mock-1").await;
    assert_eq!(h.log_count("start"), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_exceeding_max_turn_secs_is_stopped_despite_updates() {
    let script = r##"{
      "turns": [
        {
          "match": "chatter",
          "steps": [
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150},
            {"chunk": "."}, {"sleep_ms": 150}
          ]
        },
        {"match": "turn", "steps": ["turn_info"]}
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |c| {
        c.agent.inactivity_timeout_secs = 0.5;
        c.agent.max_turn_secs = 0.9;
    })
    .await;
    h.wait_log("new mock-1").await;

    let started = Instant::now();
    let reply = h.ask("chatter", false).await;
    let elapsed = started.elapsed();
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::Timeout);
    assert!(message.contains("ran for more than"), "{message}");
    assert!(elapsed >= Duration::from_millis(700), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(h.log_count("cancel mock-1"), 1);
    assert_eq!(h.status().await.follow_up_secs, None);

    assert_eq!(answer(&h.ask("turn", false).await), "turn 1 in mock-2");
    h.wait_log("close mock-1").await;
    assert_eq!(h.log_count("start"), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_budgets_disable_turn_and_tool_call_limits() {
    let script = r##"{
      "turns": [
        {
          "match": "three",
          "steps": [
            {"permission": {"command": "echo one"}},
            {"permission": {"command": "echo two"}},
            {"permission": {"command": "echo three"}},
            {"chunk": "all ran"}
          ]
        }
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |c| {
        c.agent.max_tool_calls = 0;
        c.agent.max_turn_secs = 0.0;
    })
    .await;
    h.wait_log("new mock-1").await;
    assert_eq!(answer(&h.ask("three", false).await), "all ran");
    assert_eq!(h.log_count("permission allow_once echo "), 3);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_wait_tool_calls_emit_progress_updates() {
    let script = r##"{
      "turns": [
        {
          "match": "check",
          "steps": [
            {"tool_call": {"kind": "read", "title": "Read today's note"}},
            {"permission": {"command": "hyprctl version", "summary": "Check Hyprland version"}},
            {"chunk": "All good."}
          ]
        }
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new mock-1").await;

    let (id, _client) = ipc::ask(&h.socket, "check desktop", false, false)
        .await
        .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|item| matches!(item, Presentation::Answer { .. }))
        })
        .await
        .expect("answer presented");
    assert_eq!(
        items,
        vec![
            Presentation::Progress {
                id,
                request: "check desktop".to_string(),
                profile: None,
                thinking_label: None,
            },
            Presentation::ProgressUpdate {
                id,
                status: "Read today's note".to_string(),
                icons: vec![],
            },
            Presentation::ProgressUpdate {
                id,
                status: "Check Hyprland version".to_string(),
                icons: vec![],
            },
            Presentation::Answer {
                id,
                text: "All good.".to_string(),
                denied: vec![],
                profile: None,
                widgets: vec![],
                icons: vec![],
            },
        ]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_wait_tool_calls_do_not_present_progress_updates() {
    let script = r##"{
      "turns": [
        {
          "match": "check",
          "steps": [
            {"tool_call": {"kind": "read", "title": "Read today's note"}},
            {"permission": {"command": "hyprctl version", "summary": "Check Hyprland version"}},
            {"chunk": "All good."}
          ]
        }
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new mock-1").await;
    assert_eq!(answer(&h.ask("check desktop", false).await), "All good.");
    assert!(h.recorder.items().is_empty());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_time_context_is_prepended_without_altering_progress_text() {
    let h = Harness::start(SCRIPT, |c| {
        c.context.time = true;
    })
    .await;
    h.wait_log("new mock-1").await;

    let agents = std::fs::read_to_string(h.dir.join("state/workspace/AGENTS.md")).unwrap();
    assert!(agents.contains("<untrusted_desktop_context>"), "{agents}");

    let (id, _client) = ipc::ask(&h.socket, "what day is it", false, false)
        .await
        .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|item| matches!(item, Presentation::Answer { .. }))
        })
        .await
        .expect("answer presented");
    assert_eq!(
        items[0],
        Presentation::Progress {
            id,
            request: "what day is it".to_string(),
            profile: None,
            thinking_label: None,
        }
    );
    match &items[1] {
        Presentation::Answer { text, .. } => {
            assert!(text.contains("<budget>"), "{text}");
            assert!(text.contains("max_tool_calls: 20"), "{text}");
            assert!(text.contains("<untrusted_desktop_context>"), "{text}");
            assert!(text.contains("local_time:"), "{text}");
            assert!(text.ends_with("what day is it"), "{text}");
        }
        other => panic!("{other:?}"),
    }

    // Follow-up in the same session also gets a fresh header.
    let reply = h.ask("and tomorrow", false).await;
    let echoed = answer(&reply);
    assert!(echoed.contains("<untrusted_desktop_context>"), "{echoed}");
    assert!(echoed.contains("local_time:"), "{echoed}");
    assert!(echoed.ends_with("and tomorrow"), "{echoed}");
    assert_eq!(h.log_count("new mock-1"), 1);
    h.stop().await;
}

struct ScriptedContext {
    calls: AtomicUsize,
}

impl ContextProvider for ScriptedContext {
    fn gather<'a>(
        &'a self,
        _config: &'a ContextConfig,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        let header = if n == 0 {
            "<untrusted_desktop_context>\nactive_workspace: 1\nactive_window_class: firefox\n</untrusted_desktop_context>"
        } else {
            "<untrusted_desktop_context>\nactive_workspace: 2\nactive_window_class: ghostty\n</untrusted_desktop_context>"
        };
        Box::pin(async move { Some(header.to_string()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_desktop_context_updates_across_follow_ups() {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hc-ctx-int-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, SCRIPT).unwrap();
    let log = dir.join("mock.log");
    let socket = dir.join("s.sock");

    let mut config = Config {
        state_dir: dir.join("state"),
        ..Config::default()
    };
    config.agent.command = vec![
        env!("CARGO_BIN_EXE_hey-computer").to_string(),
        "debug".to_string(),
        "mock-agent".to_string(),
        script_path.to_string_lossy().into_owned(),
        "--log".to_string(),
        log.to_string_lossy().into_owned(),
    ];
    config.context.desktop = true;
    config.conversation.follow_up_secs = 30.0;

    let recorder = Recorder::new();
    let presenter: Arc<dyn Presenter> = recorder.clone();
    let provider: Arc<dyn ContextProvider> = Arc::new(ScriptedContext {
        calls: AtomicUsize::new(0),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn({
        let socket = socket.clone();
        async move {
            daemon::serve_with_context(config, &socket, presenter, provider, async {
                let _ = stopped.await;
            })
            .await
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "socket never appeared");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (_, mut c1) = ipc::ask(&socket, "move this", true, false).await.unwrap();
    let r1 = c1.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(
        answer(&r1).contains("active_window_class: firefox"),
        "{r1:?}"
    );

    let (_, mut c2) = ipc::ask(&socket, "and now", true, false).await.unwrap();
    let r2 = c2.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(
        answer(&r2).contains("active_window_class: ghostty"),
        "{r2:?}"
    );

    let _ = stop.send(());
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_successful_answer_is_persisted_and_failures_do_not_overwrite_it() {
    use std::os::unix::fs::PermissionsExt;

    let mut h = Harness::warm().await;
    let state_dir = h.dir.join("state");
    assert_eq!(hey_computer::store::load_last(&state_dir).unwrap(), None);

    // Desktop ask (wait: false) persists history.db with 0600 permissions.
    ipc::ask(&h.socket, "plain", false, false).await.unwrap();
    h.recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|item| matches!(item, Presentation::Answer { .. }))
        })
        .await
        .expect("answer presented");
    h.eventually("history.db persisted", || {
        hey_computer::store::load_last(&state_dir)
            .ok()
            .flatten()
            .is_some()
    })
    .await;
    let first = hey_computer::store::load_last(&state_dir)
        .unwrap()
        .expect("history.db saved");
    assert_eq!(first.request, "plain");
    assert_eq!(first.text, "Hello from the mock.");
    let mode = std::fs::metadata(state_dir.join(hey_computer::store::DB_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    // Denied, Empty, and Cancelled turns must leave the previous last answer intact.
    let _ = h.ask("root", false).await;
    let _ = h.ask("silent", false).await;
    let (_, mut slow) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;
    ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    let _ = slow.next(Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!(
        hey_computer::store::load_last(&state_dir)
            .unwrap()
            .unwrap()
            .text,
        "Hello from the mock."
    );

    // Waiting ask (wait: true) updates history.db too, and it survives daemon shutdown.
    let reply = h.ask("long", false).await;
    assert!(answer(&reply).starts_with("# Title"));
    let _ = h.shut_down().await;
    let saved = hey_computer::store::load_last(&state_dir)
        .unwrap()
        .expect("history.db survives shutdown");
    assert_eq!(saved.request, "long");
    assert!(saved.text.starts_with("# Title"));

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profiles_preserve_spare_inherit_on_follow_up_and_switch_in_place() {
    use hey_computer::config::Profile;

    let script = r#"{
        "turns": [
            {"match": "cfg", "steps": ["config_info"]},
            {"match": "two_tools", "steps": [
                {"permission": {"command": "echo one"}},
                {"permission": {"command": "echo two"}},
                {"chunk": "ok"}
            ]}
        ],
        "default": ["config_info"]
    }"#;
    let h = Harness::start(script, |c| {
        c.profiles.insert(
            "deep".to_string(),
            Profile {
                model: Some("mock-deep".to_string()),
                thought_level: Some("high".to_string()),
                max_tool_calls: Some(1),
                thinking_label: Some("Pondering Hard…".to_string()),
                ..Profile::default()
            },
        );
    })
    .await;
    // Warm start prepares mock-1 for default [agent].
    h.wait_log("new mock-1").await;

    // Asking with `deep` opens mock-2, keeping mock-1 spare for a later default ask.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "cfg".to_string(),
            wait: true,
            new: false,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r1 = client.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(answer(&r1), "model=mock-deep thought=high turn=1 in mock-2");

    // Follow-up without --profile inherits `deep` (including its max_tool_calls=1 budget).
    let r2 = h.ask("cfg", false).await;
    assert_eq!(answer(&r2), "model=mock-deep thought=high turn=2 in mock-2");

    // Switching back to `default` inside the follow-up window updates mock-2 in place.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "cfg".to_string(),
            wait: true,
            new: false,
            profile: Some("default".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r3 = client.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(
        answer(&r3),
        "model=gemini-4-hydrogen thought=low turn=3 in mock-2"
    );

    // Starting a new default conversation consumes the preserved warm spare (mock-1).
    let r4 = h.ask("cfg", true).await;
    assert_eq!(
        answer(&r4),
        "model=gemini-4-hydrogen thought=low turn=1 in mock-1"
    );

    // Profile-specific budget (`deep`: max_tool_calls = 1) is enforced.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "two_tools".to_string(),
            wait: true,
            new: true,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r5 = client.next(Some(Duration::from_secs(10))).await.unwrap();
    let (reason, _) = failure(&r5);
    assert_eq!(reason, FailReason::LimitReached);

    // Non-waiting ask carries the profile name through Progress and Answer.
    let (id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "cfg".to_string(),
            wait: false,
            new: true,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|item| matches!(item, Presentation::Answer { id: got, .. } if *got == id))
        })
        .await
        .expect("deep answer presented");
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::Progress {
                id: got,
                profile: Some(p),
                thinking_label: Some(label),
                ..
            } if *got == id && p == "deep" && label == "Pondering Hard…"
        )),
        "{items:?}"
    );
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::Answer { id: got, profile: Some(p), .. } if *got == id && p == "deep"
        )),
        "{items:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_session_config_rejection_falls_back_to_a_fresh_session() {
    use hey_computer::config::Profile;

    let script = r#"{
        "reject_mid_session_config": true,
        "default": ["config_info"]
    }"#;
    let h = Harness::start(script, |c| {
        c.profiles.insert(
            "deep".to_string(),
            Profile {
                model: Some("mock-deep".to_string()),
                thought_level: Some("high".to_string()),
                ..Profile::default()
            },
        );
    })
    .await;
    h.wait_log("new mock-1").await;

    assert_eq!(
        answer(&h.ask("first", false).await),
        "model=gemini-4-hydrogen thought=low turn=1 in mock-1"
    );

    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "escalate".to_string(),
            wait: true,
            new: false,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let reply = client.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(
        answer(&reply),
        "model=mock-deep thought=high turn=1 in mock-2"
    );
    h.wait_log("close mock-1").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_profile_returns_error_without_superseding_running_request() {
    let h = Harness::warm().await;
    let (running_id, mut slow) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-1 slow").await;

    let reply = ipc::request(
        &h.socket,
        &Request::Ask {
            text: "hi".to_string(),
            wait: false,
            new: false,
            profile: Some("nonexistent".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    match reply {
        Reply::Error { message } => assert!(message.contains("unknown profile"), "{message}"),
        other => panic!("expected error, got {other:?}"),
    }

    // The running request must not have been superseded by the rejected ask.
    assert_eq!(h.status().await.running, Some(running_id));
    ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    let (reason, _) = failure(&slow.next(Some(Duration::from_secs(5))).await.unwrap());
    assert_eq!(reason, FailReason::Cancelled);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachments_are_loaded_before_accepted_and_forwarded_as_acp_blocks() {
    use hey_computer::protocol::{Attachment, AttachmentKind};

    let script = r#"{
        "turns": [
            {"match": "slow", "steps": ["hang"]}
        ],
        "default": ["attachment_info"]
    }"#;
    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new mock-1").await;

    let note = h.dir.join("notes.md");
    std::fs::write(&note, "hello world").unwrap();
    let img = h.dir.join("shot.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00").unwrap();

    // Send both file and image, then delete the image immediately after Accepted
    // to prove the daemon loaded it before replying Accepted.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "inspect".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![
                Attachment {
                    kind: AttachmentKind::File,
                    path: note.clone(),
                },
                Attachment {
                    kind: AttachmentKind::Image,
                    path: img.clone(),
                },
            ],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    std::fs::remove_file(&img).unwrap();
    let reply = client.next(Some(Duration::from_secs(10))).await.unwrap();
    let got = answer(&reply);
    assert!(got.contains("inspect"), "{got}");
    assert!(got.contains("resource:file://"), "{got}");
    assert!(got.contains("text/markdown:len=11"), "{got}");
    assert!(got.contains("image:image/png:b64=16"), "{got}");

    // Metadata (never raw bytes) is persisted in history.db.
    h.eventually("attachment metadata persisted", || {
        hey_computer::store::load_last(&h.dir.join("state"))
            .ok()
            .flatten()
            .is_some()
    })
    .await;
    let saved = hey_computer::store::load_last(&h.dir.join("state"))
        .unwrap()
        .expect("saved");
    assert_eq!(saved.attachments.len(), 2);
    assert_eq!(saved.attachments[0].name, "notes.md");
    assert_eq!(saved.attachments[1].mime_type, "image/png");

    // Empty text with an attachment synthesizes a default prompt.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: String::new(),
            wait: true,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![Attachment {
                kind: AttachmentKind::File,
                path: note,
            }],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let reply = client.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(
        answer(&reply).contains("Review or explain the attached file."),
        "{reply:?}"
    );

    // Invalid attachment fails fast with Reply::Error without superseding a running turn.
    let (running_id, mut slow) = h.ask_waiting("slow").await;
    h.wait_log("prompt mock-2 slow").await;
    let err = ipc::request(
        &h.socket,
        &Request::Ask {
            text: "bad".to_string(),
            wait: false,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                path: h.dir.join("missing.png"),
            }],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(err, Reply::Error { .. }), "{err:?}");
    assert_eq!(h.status().await.running, Some(running_id));
    ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    let _ = slow.next(Some(Duration::from_secs(5))).await.unwrap();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_task_runs_concurrently_with_interactive_and_survives_interactive_cancel() {
    let script = r#"{
        "turns": [
            {"match": "bg_job", "steps": [
                {"chunk": "bg-start "},
                {"wait_for": "fg_done"},
                {"chunk": "bg-end"}
            ]},
            {"match": "fg_quick", "steps": [
                {"permission": {"command": "hyprctl version"}},
                {"signal": "fg_done"},
                {"chunk": "fg-answer"}
            ]},
            {"match": "fg_slow", "steps": ["hang"]}
        ],
        "default": ["echo"]
    }"#;
    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new mock-1").await;

    // Submit a background task (wait=false) -> opens mock-2, keeping mock-1 spare for voice.
    let (bg_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "bg_job".to_string(),
            wait: false,
            new: false,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    h.wait_log("prompt mock-2 bg_job").await;

    let st = h.status().await;
    assert_eq!(st.running, None);
    assert_eq!(st.background_running, 1);
    assert_eq!(st.background_queued, 0);

    // Start an interactive ask and cancel it via plain Cancel { id: None };
    // the background task must keep running untouched.
    let (fg_slow_id, mut fg_slow) = h.ask_waiting("fg_slow").await;
    h.wait_log("prompt mock-1 fg_slow").await;
    let cancel_reply = ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    assert_eq!(
        cancel_reply,
        Reply::Cancelled {
            id: Some(fg_slow_id)
        }
    );
    let (reason, _) = failure(&fg_slow.next(Some(Duration::from_secs(5))).await.unwrap());
    assert_eq!(reason, FailReason::Cancelled);
    assert_eq!(h.status().await.background_running, 1);

    // Interactive ask runs concurrently with bg_job and releases the barrier.
    let fg_reply = h.ask("fg_quick", false).await;
    assert_eq!(answer(&fg_reply), "fg-answer");

    // Background task finishes and emits TaskComplete (never interactive Answer).
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == bg_id))
        })
        .await
        .expect("TaskComplete presented");
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::TaskComplete { id, text, .. } if *id == bg_id && text == "bg-start bg-end"
        )),
        "{items:?}"
    );
    h.wait_log("close mock-2").await;

    // Default `load_last` stays interactive-only ("fg-answer"), while `get_turn(bg_id)` has the task.
    let state_dir = h.dir.join("state");
    h.eventually("bg task completed in db", || {
        hey_computer::store::get_turn(&state_dir, bg_id)
            .ok()
            .flatten()
            .is_some_and(|t| t.status == hey_computer::store::TurnStatus::Completed)
    })
    .await;
    assert_eq!(
        hey_computer::store::load_last(&state_dir)
            .unwrap()
            .unwrap()
            .text,
        "fg-answer"
    );
    assert_eq!(
        hey_computer::store::get_turn(&state_dir, bg_id)
            .unwrap()
            .unwrap()
            .answer,
        "bg-start bg-end"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_tasks_queue_fifo_respect_max_queued_and_cancel_by_id() {
    let h = Harness::start(SCRIPT, |c| {
        c.tasks.max_concurrent = 1;
        c.tasks.max_queued = 1;
    })
    .await;
    h.wait_log("new mock-1").await;

    // bg1 runs immediately; bg2 sits in queue; bg3 is rejected (max_queued = 1).
    let (bg1, mut c1) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "slow".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    h.wait_log("prompt mock-2 slow").await;

    let (bg2, mut c2) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "plain".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let st = h.status().await;
    assert_eq!(st.background_running, 1);
    assert_eq!(st.background_queued, 1);

    let bg3 = ipc::request(
        &h.socket,
        &Request::Ask {
            text: "overflow".to_string(),
            wait: false,
            new: false,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(bg3, Reply::Error { .. }), "{bg3:?}");

    // Cancel queued bg2 by id -> immediately evicted without affecting bg1.
    let cancel2 = ipc::request(&h.socket, &Request::Cancel { id: Some(bg2) })
        .await
        .unwrap();
    assert_eq!(cancel2, Reply::Cancelled { id: Some(bg2) });
    let (r2, _) = failure(&c2.next(Some(Duration::from_secs(5))).await.unwrap());
    assert_eq!(r2, FailReason::Cancelled);
    assert_eq!(h.status().await.background_queued, 0);
    assert_eq!(h.status().await.background_running, 1);

    // Cancel running bg1 by id.
    let cancel1 = ipc::request(&h.socket, &Request::Cancel { id: Some(bg1) })
        .await
        .unwrap();
    assert_eq!(cancel1, Reply::Cancelled { id: Some(bg1) });
    let (r1, _) = failure(&c1.next(Some(Duration::from_secs(5))).await.unwrap());
    assert_eq!(r1, FailReason::Cancelled);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suppress_token_silences_heartbeats_and_notifies_on_alerts() {
    let script = r#"{
        "turns": [
            {"match": "hb_ok", "steps": [{"chunk": "HEARTBEAT_OK."}]},
            {"match": "hb_alert", "steps": [{"chunk": "Disk 92% full"}]}
        ],
        "default": ["echo"]
    }"#;
    let h = Harness::start(script, |_| {}).await;
    h.wait_log("new mock-1").await;

    // 1. Background fire-and-forget heartbeat matching token -> silent + persisted as suppressed.
    let (hb_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "hb_ok".to_string(),
            wait: false,
            new: false,
            profile: None,
            background: true,
            suppress_token: Some("HEARTBEAT_OK".to_string()),
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let state_dir = h.dir.join("state");
    h.eventually("heartbeat persisted as suppressed", || {
        hey_computer::store::get_turn(&state_dir, hb_id)
            .ok()
            .flatten()
            .is_some_and(|t| t.status == hey_computer::store::TurnStatus::Suppressed)
    })
    .await;
    assert!(h.recorder.items().is_empty());

    // 2. Waiting (--print) heartbeat matching token -> Reply::Answer { suppressed: true }.
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "hb_ok".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: true,
            suppress_token: Some("HEARTBEAT_OK".to_string()),
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    match c.next(Some(Duration::from_secs(10))).await.unwrap() {
        Reply::Answer { suppressed, .. } => assert!(suppressed),
        other => panic!("expected suppressed answer, got {other:?}"),
    }

    // 3. Non-matching alert -> emits TaskComplete!
    let (alert_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "hb_alert".to_string(),
            wait: false,
            new: false,
            profile: None,
            background: true,
            suppress_token: Some("HEARTBEAT_OK".to_string()),
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == alert_id))
        })
        .await
        .expect("alert TaskComplete");
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::TaskComplete { id, text, .. } if *id == alert_id && text == "Disk 92% full"
        )),
        "{items:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_config_is_written_privately_and_profile_mcp_allowlist_is_enforced() {
    use hey_computer::config::{McpServer, Profile};
    use std::os::unix::fs::PermissionsExt;

    let script = r#"{
        "turns": [
            {
                "match": "call_nixos",
                "steps": [
                    {"permission": {"command": "nix", "server": "nixos"}},
                    {"chunk": "nixos ok"}
                ]
            }
        ],
        "default": ["turn_info"]
    }"#;

    let h = Harness::start(script, |c| {
        c.mcp.insert(
            "nixos".to_string(),
            McpServer {
                enabled: true,
                command: "/bin/mcp-nixos".to_string(),
                ..McpServer::default()
            },
        );
        c.mcp.insert(
            "linear".to_string(),
            McpServer {
                enabled: true,
                url: "https://mcp.linear.app/mcp".to_string(),
                ..McpServer::default()
            },
        );
        c.profiles.insert(
            "lean".to_string(),
            Profile {
                mcp_servers: Some(vec![]),
                ..Profile::default()
            },
        );
    })
    .await;
    // Sessions pass empty `mcpServers` on `session/new` so agy enables lazy `call_mcp_tool`.
    h.wait_log("new mock-1").await;
    assert_eq!(h.log_count("new mock-1 mcp="), 0);

    // `<agent_dir>/config/mcp_config.json` carries all enabled servers with mode 0600.
    let mcp_path = h.dir.join("state/agy/config/mcp_config.json");
    let meta = std::fs::metadata(&mcp_path).expect("mcp_config.json created");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
    assert_eq!(doc["mcpServers"]["nixos"]["command"], "/bin/mcp-nixos");
    assert_eq!(
        doc["mcpServers"]["linear"]["serverUrl"],
        "https://mcp.linear.app/mcp"
    );

    // Default profile allows `call_mcp_tool` for `nixos`.
    assert_eq!(answer(&h.ask("call_nixos", false).await), "nixos ok");

    // Switching to `lean` (`mcp_servers = []`) denies `call_mcp_tool` for `nixos`.
    let (_, mut client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "call_nixos".to_string(),
            wait: true,
            new: false,
            profile: Some("lean".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let reply = client.next(Some(Duration::from_secs(10))).await.unwrap();
    let (reason, msg) = failure(&reply);
    assert_eq!(reason, FailReason::Denied);
    assert!(msg.contains("mcp:nixos/nix"), "{msg}");
    // Switching profiles across different `mcp_servers` keeps the same session in place.
    assert_eq!(h.log_count("new mock-2"), 0);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_tool_call_guard_aborts_wedged_retry_loops() {
    let script = r#"{
        "turns": [
            {
                "match": "looping",
                "steps": [
                    {"permission": {"command": "echo stuck"}},
                    {"permission": {"command": "echo stuck"}},
                    {"permission": {"command": "echo stuck"}},
                    {"chunk": "should not reach here"}
                ]
            }
        ],
        "default": ["echo"]
    }"#;
    let h = Harness::start(script, |c| {
        c.agent.max_repeat_tool_calls = 3;
    })
    .await;
    h.wait_log("new mock-1").await;

    let reply = h.ask("looping", false).await;
    let (reason, msg) = failure(&reply);
    assert_eq!(reason, FailReason::LimitReached);
    assert!(msg.contains("repeated the same tool call 3 times"), "{msg}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_runs_no_agent_pre_collector_and_split_failure_routing() {
    use hey_computer::store::{self, FailureDeliver, NewSchedule};

    let script = r#"{
        "turns": [
            {"match": "COLLECTOR_WAKE", "steps": [{"chunk": "Alert: build failed"}]}
        ],
        "default": ["echo"]
    }"#;
    let h = Harness::start(script, |c| {
        c.scheduler.enabled = true;
        c.scheduler.min_interval_secs = 1.0;
    })
    .await;
    h.wait_log("new mock-1").await;

    let state_dir = h.dir.join("state");
    let now = store::unix_timestamp_ms();

    // 1. --no-agent timed reminder (zero LLM tokens).
    store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "remind-me".to_string(),
            schedule_spec: "in 1h".to_string(),
            prompt: "Standup in 5 minutes".to_string(),
            profile: String::new(),
            script: String::new(),
            no_agent: true,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            next_run_at: now + 3_600_000,
        },
        128,
    )
    .unwrap();
    let res = ipc::request(
        &h.socket,
        &Request::ScheduleRun {
            target: "remind-me".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(res, Reply::Ok { .. }), "{res:?}");
    let items = h
        .recorder
        .wait_for(Duration::from_secs(5), |items| {
            items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::Notice { summary, body, .. }
                        if summary.contains("remind-me") && body == "Standup in 5 minutes"
                )
            })
        })
        .await
        .expect("reminder notice");
    assert!(!items.is_empty());

    // 2. --script pre-collector with empty stdout -> skipped (0 LLM turns).
    store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "quiet-check".to_string(),
            schedule_spec: "every 5m".to_string(),
            prompt: "COLLECTOR_WAKE".to_string(),
            profile: String::new(),
            script: "true".to_string(),
            no_agent: false,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            next_run_at: now + 300_000,
        },
        128,
    )
    .unwrap();
    ipc::request(
        &h.socket,
        &Request::ScheduleRun {
            target: "quiet-check".to_string(),
        },
    )
    .await
    .unwrap();
    h.eventually("quiet-check marked skipped", || {
        store::get_schedule(&state_dir, "quiet-check")
            .ok()
            .flatten()
            .is_some_and(|j| j.last_status.as_deref() == Some("skipped"))
    })
    .await;
    assert_eq!(h.log_count("prompt "), 0);

    // 3. --script pre-collector with output -> wakes agent with <untrusted_collector_output>.
    store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "noisy-check".to_string(),
            schedule_spec: "every 5m".to_string(),
            prompt: "COLLECTOR_WAKE".to_string(),
            profile: String::new(),
            script: "echo 'job 42 failed'".to_string(),
            no_agent: false,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            next_run_at: now + 300_000,
        },
        128,
    )
    .unwrap();
    ipc::request(
        &h.socket,
        &Request::ScheduleRun {
            target: "noisy-check".to_string(),
        },
    )
    .await
    .unwrap();
    h.recorder
        .wait_for(Duration::from_secs(10), |items| {
            items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::TaskComplete { text, .. } if text == "Alert: build failed"
                )
            })
        })
        .await
        .expect("collector triggered agent TaskComplete");
    h.wait_log("untrusted_collector_output").await;

    // 4. Failing collector notifies under failure_deliver = notify.
    store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "broken-check".to_string(),
            schedule_spec: "every 5m".to_string(),
            prompt: "unused".to_string(),
            profile: String::new(),
            script: "echo 'auth expired' >&2; exit 2".to_string(),
            no_agent: false,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            next_run_at: now + 300_000,
        },
        128,
    )
    .unwrap();
    ipc::request(
        &h.socket,
        &Request::ScheduleRun {
            target: "broken-check".to_string(),
        },
    )
    .await
    .unwrap();
    h.recorder
        .wait_for(Duration::from_secs(5), |items| {
            items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::Notice { summary, body, .. }
                        if summary.contains("broken-check") && body.contains("auth expired")
                )
            })
        })
        .await
        .expect("failure alert notice");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_routing_handles_heuristics_escalation_voice_zero_triage_and_typed_triage() {
    use hey_computer::config::Profile;

    let script = r#"{
        "turns": [
            {"match": "Classify the user request below", "steps": [{"chunk": "{\"profile\":\"deep\",\"lane\":\"interactive\"}"}]},
            {"match": "empty_fast", "steps": []},
            {"match": "Write a report on consensus algorithms", "steps": ["config_info"]}
        ],
        "default": ["config_info"]
    }"#;
    let h = Harness::start(script, |c| {
        c.profiles.insert(
            "fast".to_string(),
            Profile {
                model: Some("mock-fast".to_string()),
                thought_level: Some("low".to_string()),
                ..Profile::default()
            },
        );
        c.profiles.insert(
            "deep".to_string(),
            Profile {
                model: Some("mock-deep".to_string()),
                thought_level: Some("high".to_string()),
                ..Profile::default()
            },
        );
        c.routing.enabled = true;
        c.routing.fast_profile = "fast".to_string();
        c.routing.deep_profile = "deep".to_string();
        c.routing.auto_background = true;
        c.routing.escalate_on_failure = true;
        c.routing.triage_non_voice = true;
        c.routing.triage_timeout_secs = 4.0;
    })
    .await;
    h.wait_log("new mock-1").await;

    // 1. Voice short command -> fast (Tier 1), zero triage turns.
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "turn off the lamp".to_string(),
            wait: true,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r = c.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(answer(&r).contains("model=mock-fast thought=low"), "{r:?}");

    // 2. Follow-up correction ("no, ...") escalates in place to deep on the same session!
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "no, I meant the desk lamp".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r2 = c.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(
        answer(&r2).contains("model=mock-deep thought=high turn=2"),
        "{r2:?}"
    );

    // 3. Ambiguous ("Unsure") VOICE prompt MUST NOT invoke LLM triage; defaults to fast.
    let before_triage = h.log_count("Classify the user request below");
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Could you put together a short outline of the quarterly roadmap items we discussed?"
                .to_string(),
            wait: true,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r3 = c.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(answer(&r3).contains("model=mock-fast"), "{r3:?}");
    assert_eq!(
        h.log_count("Classify the user request below"),
        before_triage
    );

    // 4. Same ambiguous prompt from TYPED input DOES run Tier 3 restricted triage -> routes to deep!
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Could you put together a short outline of the quarterly roadmap items we discussed?"
                .to_string(),
            wait: true,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r4 = c.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(answer(&r4).contains("model=mock-deep"), "{r4:?}");
    assert_eq!(
        h.log_count("Classify the user request below"),
        before_triage + 1
    );

    // 5. Non-waiting multi-step report prompt auto-routes to background + deep with Notice + TaskComplete.
    let (bg_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Write a report on consensus algorithms".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::TaskComplete { id, profile: Some(p), .. }
                        if *id == bg_id && p == "deep"
                )
            })
        })
        .await
        .expect("auto-background TaskComplete");
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::Notice { summary, .. } if summary == "Working in the background"
        )),
        "{items:?}"
    );

    // 6. Tier 2 failure escalation: an auto-routed fast turn that returns Empty
    //    retries once transparently on `deep` (and never loops indefinitely).
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "empty_fast".to_string(),
            wait: true,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let r6 = c.next(Some(Duration::from_secs(10))).await.unwrap();
    let (reason6, _) = failure(&r6);
    assert_eq!(reason6, FailReason::Empty);
    assert_eq!(h.log_count("empty_fast"), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn triage_background_promotion_clears_running_and_expiring_recurring_schedule_disables() {
    use hey_computer::config::Profile;
    use hey_computer::store::{self, FailureDeliver, NewSchedule};

    let script = r#"{
        "turns": [
            {"match": "Classify the user request below", "steps": [{"chunk": "{\"profile\":\"deep\",\"lane\":\"background\"}"}]},
            {"match": "profile_budget_probe", "steps": ["echo"]}
        ],
        "default": [{"chunk": "done"}]
    }"#;
    let h = Harness::start(script, |c| {
        c.context.time = false;
        c.context.desktop = false;
        c.profiles.insert(
            "fast".to_string(),
            Profile {
                model: Some("mock-fast".to_string()),
                thought_level: Some("low".to_string()),
                ..Profile::default()
            },
        );
        c.profiles.insert(
            "deep".to_string(),
            Profile {
                model: Some("mock-deep".to_string()),
                thought_level: Some("high".to_string()),
                max_tool_calls: Some(123),
                ..Profile::default()
            },
        );
        c.routing.enabled = true;
        c.routing.fast_profile = "fast".to_string();
        c.routing.deep_profile = "deep".to_string();
        c.routing.auto_background = true;
        c.routing.triage_non_voice = true;
        c.scheduler.enabled = true;
        c.scheduler.min_interval_secs = 1.0;
    })
    .await;
    h.wait_log("new mock-1").await;

    // 1. Named profile receives `<budget>` even when `[context]` is disabled.
    let (_, mut c) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "profile_budget_probe".to_string(),
            wait: true,
            new: true,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let budget_reply = c.next(Some(Duration::from_secs(10))).await.unwrap();
    let body = answer(&budget_reply);
    assert!(body.contains("<budget>"), "{body}");
    assert!(body.contains("profile: deep"), "{body}");
    assert!(body.contains("max_tool_calls: 123"), "{body}");

    // 2. Tier 3 triage promoting an unsure typed turn to `background` must not
    //    leave `status.running` stuck after completion, and records `background` lane.
    let (promoted_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Could you put together a short outline of the quarterly roadmap items we discussed?"
                .to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    h.recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == promoted_id))
        })
        .await
        .expect("triage-promoted TaskComplete");
    match ipc::request(&h.socket, &Request::Status).await.unwrap() {
        Reply::Status(st) => assert_eq!(st.running, None, "status.running leaked: {st:?}"),
        other => panic!("{other:?}"),
    }
    let state_dir = h.dir.join("state");
    let rec = store::get_turn(&state_dir, promoted_id)
        .unwrap()
        .expect("promoted task in db");
    assert_eq!(rec.lane, store::Lane::Background);

    // 3. Recurring schedule whose next firing would cross `expires_at` disables cleanly.
    let now_ms = store::unix_timestamp_ms();
    let job = store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "expiring-every".to_string(),
            schedule_spec: "every 1s".to_string(),
            prompt: "ping".to_string(),
            profile: String::new(),
            script: String::new(),
            no_agent: true,
            suppress_token: None,
            failure_deliver: FailureDeliver::Silent,
            max_runs: None,
            expires_at: Some(now_ms + 900),
            next_run_at: now_ms + 150,
        },
        128,
    )
    .unwrap();
    let _ = ipc::request(&h.socket, &Request::ScheduleReload).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let cur = store::get_schedule(&state_dir, &job.id.to_string())
            .unwrap()
            .unwrap();
        if cur.run_count >= 1 && !cur.enabled {
            assert_eq!(cur.next_run_at, None);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recurring job with expires_at stayed enabled: {cur:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn section3_ux_and_conductor_quirks_regression() {
    use hey_computer::config::Profile;

    let script = r#"{
        "turns": [
            {"match": "Classify the user request below", "steps": [{"sleep_ms": 150}, {"chunk": "{\"profile\":\"deep\",\"lane\":\"background\"}"}]},
            {"match": "slow_fg", "steps": [{"sleep_ms": 350}, {"chunk": "foreground finished"}]},
            {"match": "Write a report on storage engines", "steps": [{"sleep_ms": 120}, "echo"]},
            {"match": "cancel_me_turn1", "steps": [{"sleep_ms": 5000}, {"chunk": "never"}]},
            {"match": "lazy_mcp_reads", "steps": [
                {"update": {"sessionUpdate": "tool_call", "toolCallId": "s1", "kind": "read", "name": "read_file", "rawInput": {"Path": "__AGENT_MCP_1__"}}},
                {"update": {"sessionUpdate": "tool_call", "toolCallId": "s2", "kind": "read", "name": "read_file", "rawInput": {"Path": "__AGENT_MCP_2__"}}},
                {"permission": {"command": "echo ok"}},
                {"chunk": "within budget"}
            ]}
        ],
        "default": ["echo"]
    }"#;

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_mem_dir = std::env::temp_dir().join(format!("hc-mem{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_mem_dir);
    std::fs::create_dir_all(&tmp_mem_dir).unwrap();
    let mem_path = tmp_mem_dir.join("memory.md");
    std::fs::write(&mem_path, "Memory v1: likes espresso").unwrap();

    let mem_for_cfg = mem_path.clone();
    let h = Harness::start(script, move |c| {
        c.instructions.memory_file = mem_for_cfg.clone();
        c.conversation.follow_up_secs = 0.6;
        c.context.time = true;
        c.context.desktop = false;
        c.profiles.insert(
            "fast".to_string(),
            Profile {
                model: Some("mock-fast".to_string()),
                thought_level: Some("low".to_string()),
                max_tool_calls: Some(1),
                ..Profile::default()
            },
        );
        c.profiles.insert(
            "deep".to_string(),
            Profile {
                model: Some("mock-deep".to_string()),
                thought_level: Some("high".to_string()),
                thinking_label: Some("Pondering Hard…".to_string()),
                ..Profile::default()
            },
        );
        c.routing.enabled = true;
        c.routing.fast_profile = "fast".to_string();
        c.routing.deep_profile = "deep".to_string();
        c.routing.auto_background = true;
        c.routing.triage_non_voice = true;
    })
    .await;
    h.wait_log("new mock-1").await;

    // 1. Cancelling turn 1 of a fresh conversation must NOT open a follow-up window.
    let (cancel_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "cancel_me_turn1".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    h.wait_log("prompt mock-").await;
    let cancel_reply = ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    assert_eq!(
        cancel_reply,
        Reply::Cancelled {
            id: Some(cancel_id)
        }
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    match ipc::request(&h.socket, &Request::Status).await.unwrap() {
        Reply::Status(st) => assert_eq!(
            st.follow_up_secs, None,
            "cancelled turn 1 leaked follow_up_secs: {st:?}"
        ),
        other => panic!("{other:?}"),
    }

    // 2. Routine fast turn suppresses `(fast)` badge (`profile == None` in Presentation::Answer).
    let (fast_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "turn off the lamp".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == fast_id))
        })
        .await
        .expect("fast answer presented");
    assert!(
        items.iter().any(|i| matches!(
            i,
            Presentation::Answer { id, profile: None, .. } if *id == fast_id
        )),
        "expected routine fast turn to have profile: None, got: {items:?}"
    );
    // Wait out the 0.6s follow-up window before the next independent check.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 3. Tier 1 auto-backgrounding does NOT supersede a running foreground turn!
    let (fg_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "slow_fg".to_string(),
            wait: false,
            new: true,
            profile: Some("deep".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (bg_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Write a report on storage engines".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let both = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            let has_fg = items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::Answer { id, profile: Some(p), .. }
                        if *id == fg_id && p == "deep"
                )
            });
            let has_bg = items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == bg_id));
            has_fg && has_bg
        })
        .await
        .expect("both fg_id and auto-bg bg_id completed without superseding");
    assert!(!both.iter().any(
        |i| matches!(i, Presentation::Failure { id, reason: FailReason::Superseded, .. } if *id == fg_id)
    ));
    tokio::time::sleep(Duration::from_millis(700)).await;

    // 4. Slow Tier 3 triage shows instant Progress first, then replaces it via Notice.replaces_id!
    let (triage_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "Could you put together a short outline of the quarterly roadmap items we discussed?"
                .to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let triage_items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == triage_id))
        })
        .await
        .expect("triage background complete");
    let prog_pos = triage_items
        .iter()
        .position(|i| matches!(i, Presentation::Progress { id, .. } if *id == triage_id))
        .expect("instant pre-triage Progress");
    let notice_pos = triage_items
        .iter()
        .position(|i| {
            matches!(
                i,
                Presentation::Notice { summary, replaces_id: Some(rid), .. }
                    if summary == "Working in the background" && *rid == triage_id
            )
        })
        .expect("Notice replacing Progress");
    assert!(prog_pos < notice_pos, "{triage_items:?}");

    // 5. Updating `memory.md` refreshes `AGENTS.md` automatically in the background while idle
    //    (outside `follow_up_secs`), and internal `<agent_dir>/mcp/...` `read_file` calls do NOT
    //    trip `max_tool_calls = 1`!
    let agents_md_path = h.dir.join("state/workspace/AGENTS.md");
    assert!(
        std::fs::read_to_string(&agents_md_path)
            .unwrap()
            .contains("Memory v1: likes espresso")
    );
    let patched_script = script
        .replace(
            "__AGENT_MCP_1__",
            &h.dir
                .join("state/agy/antigravity-cli/mcp/slack/conversations_history.json")
                .to_string_lossy(),
        )
        .replace(
            "__AGENT_MCP_2__",
            &h.dir
                .join("state/agy/antigravity-cli/mcp/google-workspace/list_events.json")
                .to_string_lossy(),
        );
    std::fs::write(h.dir.join("script.json"), patched_script).unwrap();
    std::fs::write(&mem_path, "Memory v2: prefers filter coffee").unwrap();
    h.eventually("idle memory auto-reload", || {
        std::fs::read_to_string(&agents_md_path)
            .unwrap_or_default()
            .contains("Memory v2: prefers filter coffee")
    })
    .await;
    // Wait for the recycled warm spare session (`new mock-1` count >= 2) before asking.
    h.eventually("recycled warm spare", || h.log_count("new mock-1") >= 2)
        .await;
    let (_, mut mc) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "lazy_mcp_reads".to_string(),
            wait: true,
            new: true,
            profile: Some("fast".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let mcp_reply = mc.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(answer(&mcp_reply), "within budget");

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp_mem_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase5_mcp_ui_streaming_reply_to_resumption_and_voice_tts_modality() {
    use hey_computer::config::Profile;
    use hey_computer::protocol::SpeakAction;

    let script = r##"{
      "turns": [
        {
          "match": "show_agenda",
          "steps": [
            {
              "permission": {
                "server": "mcp-ui",
                "command": "show_widget",
                "arguments": {
                  "id": "w-cal",
                  "kind": "calendar_agenda",
                  "title": "Morning Agenda",
                  "data": { "events": [{ "title": "Standup", "start": "09:30" }] }
                }
              }
            },
            { "chunk": "Here is " },
            { "chunk": "your agenda." }
          ]
        },
        {
          "match": "live_followup",
          "steps": ["turn_info"]
        },
        {
          "match": "resume_after_close",
          "steps": ["echo"]
        },
        {
          "match": "bg_seed_fact",
          "steps": [{ "chunk": "Seed fact: project codename is Aurora." }]
        },
        {
          "match": "voice_hello",
          "steps": ["echo"]
        },
        {
          "match": "typed_hello",
          "steps": ["echo"]
        }
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |cfg| {
        // `follow_up_secs` is tiny so we can verify `reply_to` reuses the live
        // ACP session even after `follow_up_secs` expires!
        cfg.conversation.follow_up_secs = 0.05;
        cfg.profiles.insert(
            "lean".to_string(),
            Profile {
                // Even with `mcp_servers = Some([])`, `mcp-ui` must stay allowed
                // on non-restricted lanes!
                mcp_servers: Some(vec![]),
                ..Profile::default()
            },
        );
    })
    .await;
    h.wait_log("new mock-1").await;

    // 1. Verify `mcp-ui` is auto-registered in `<agent_dir>/config/mcp_config.json`.
    let mcp_cfg_path = h.dir.join("state/agy/config/mcp_config.json");
    let mcp_cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_cfg_path).unwrap()).unwrap();
    assert_eq!(mcp_cfg["mcpServers"]["mcp-ui"]["type"], "stdio");

    // 2. `stream: true` on `lean` profile streams `Chunk` + `Widget` + `Answer`
    //    and persists `widgets_json` + `conversation_id` in SQLite v4.
    let (agenda_id, mut stream_client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "show_agenda".to_string(),
            wait: true,
            new: true,
            profile: Some("lean".to_string()),
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: None,
            stream: true,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();

    let mut chunks = String::new();
    let mut streamed_widgets = Vec::new();
    let final_answer = loop {
        match stream_client
            .next(Some(Duration::from_secs(10)))
            .await
            .unwrap()
        {
            Reply::Chunk { id, text } => {
                assert_eq!(id, agenda_id);
                chunks.push_str(&text);
            }
            Reply::Progress { id, .. } => assert_eq!(id, agenda_id),
            Reply::Widget { id, widget } => {
                assert_eq!(id, agenda_id);
                streamed_widgets.push(widget);
            }
            Reply::Answer {
                id,
                text,
                widgets,
                conversation_id,
                profile,
                ..
            } => {
                assert_eq!(id, agenda_id);
                assert_eq!(conversation_id, Some(agenda_id));
                assert_eq!(profile.as_deref(), Some("lean"));
                assert_eq!(widgets.len(), 1);
                assert_eq!(widgets[0].kind, "calendar_agenda");
                break text;
            }
            other => panic!("unexpected stream frame: {other:?}"),
        }
    };
    assert_eq!(chunks, "Here is your agenda.");
    assert_eq!(final_answer, "Here is your agenda.");
    assert_eq!(streamed_widgets.len(), 1);

    let stored = hey_computer::store::get_turn(&h.dir.join("state"), agenda_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.widgets.len(), 1);
    assert_eq!(stored.conversation_id, Some(agenda_id));

    // 3. Path A: Wait past `follow_up_secs` (50 ms) and send `reply_to: Some(agenda_id)` —
    //    must reuse the still-open live ACP session (`turn 2 in mock-1`)!
    tokio::time::sleep(Duration::from_millis(80)).await;
    let (_, mut live_reply_client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "live_followup".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: Some(agenda_id),
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let live_reply = live_reply_client
        .next(Some(Duration::from_secs(10)))
        .await
        .unwrap();
    assert!(
        answer(&live_reply).starts_with("turn 2 in "),
        "expected live session reuse on reply_to, got: {}",
        answer(&live_reply)
    );

    // 4. Path B: Background task closes its ACP session immediately on completion;
    //    `reply_to: Some(bg_id)` seeds `<untrusted_conversation_history>` from `history.db`!
    let (bg_id, mut bg_client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "bg_seed_fact".to_string(),
            wait: true,
            new: true,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let bg_ans = bg_client.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(answer(&bg_ans), "Seed fact: project codename is Aurora.");

    let (resumed_id, mut resumed_client) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "resume_after_close".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: Some(bg_id),
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let resumed_ans = resumed_client
        .next(Some(Duration::from_secs(10)))
        .await
        .unwrap();
    let echoed = answer(&resumed_ans);
    assert!(
        echoed.contains("<untrusted_conversation_history>")
            && echoed.contains("Aurora")
            && echoed.contains("resume_after_close"),
        "expected DB-seeded history block, got: {echoed}"
    );
    let resumed_row = hey_computer::store::get_turn(&h.dir.join("state"), resumed_id)
        .unwrap()
        .unwrap();
    assert_eq!(resumed_row.conversation_id, Some(bg_id));

    // 5. Voice vs. text modality + post-turn `cancel` stopping TTS playback:
    let speak_reply = ipc::request(
        &h.socket,
        &Request::Speak {
            action: SpeakAction::On,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        speak_reply,
        Reply::SpeakStatus {
            enabled: true,
            speaking: false
        }
    );
    assert!(h.status().await.speak);

    // 5a. Typed non-waiting ask: text-only (no `<modality source="voice">`, zero `Speech*` events).
    let (typed_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "typed_hello".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items_after_typed = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == typed_id))
        })
        .await
        .expect("typed answer presented");
    assert!(
        !items_after_typed.iter().any(|i| matches!(
            i,
            Presentation::SpeechStart { id }
                | Presentation::SpeechChunk { id, .. }
                | Presentation::SpeechFinish { id, .. }
                if *id == typed_id
        )),
        "typed ask must not trigger TTS speech: {items_after_typed:?}"
    );

    // 5b. Voice non-waiting ask: injects `<modality source="voice">` AND streams TTS events!
    let (voice_id, _) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "voice_hello".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Voice,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let items_after_voice = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == voice_id))
        })
        .await
        .expect("voice answer presented");
    let voice_answer = items_after_voice
        .iter()
        .find_map(|i| match i {
            Presentation::Answer { id, text, .. } if *id == voice_id => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(
        voice_answer.contains("<modality source=\"voice\">"),
        "voice prompt must carry voice modality hint: {voice_answer}"
    );
    assert!(
        items_after_voice
            .iter()
            .any(|i| matches!(i, Presentation::SpeechStart { id } if *id == voice_id))
            && items_after_voice
                .iter()
                .any(|i| matches!(i, Presentation::SpeechChunk { id, .. } if *id == voice_id))
            && items_after_voice
                .iter()
                .any(|i| matches!(i, Presentation::SpeechFinish { id, .. } if *id == voice_id)),
        "voice ask must emit SpeechStart/SpeechChunk/SpeechFinish: {items_after_voice:?}"
    );

    // 5c. Simulate ongoing TTS audio after the ACP turn has completed; `Request::Cancel`
    //     must stop active speech and return `Some(voice_id)`!
    h.recorder.simulate_speaking(Some(voice_id));
    let cancel_reply = ipc::request(&h.socket, &Request::Cancel { id: None })
        .await
        .unwrap();
    assert_eq!(cancel_reply, Reply::Cancelled { id: Some(voice_id) });
    assert!(h.recorder.stopped_speech().contains(&None));

    h.stop().await;
}

#[tokio::test]
async fn read_aloud_streams_completion_and_stops_immediately_on_disconnect() {
    let h = Harness::start(r#"{"turns": []}"#, |cfg| {
        cfg.agent.warm_start = false;
    })
    .await;

    // 1. Empty text returns Reply::Error cleanly.
    let err = ipc::request(
        &h.socket,
        &Request::ReadAloud {
            text: "   ".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(err, Reply::Error { .. }), "{err:?}");

    // 2. Normal completion: SpeakStarted -> SpeakFinished.
    let mut client = ipc::Client::send(
        &h.socket,
        &Request::ReadAloud {
            text: "# Brief\n\nAll systems are nominal.".to_string(),
        },
    )
    .await
    .unwrap();
    let started_id = match client.next(Some(Duration::from_secs(5))).await.unwrap() {
        Reply::SpeakStarted { id } => id,
        other => panic!("expected SpeakStarted, got {other:?}"),
    };
    let finished_id = match client.next(Some(Duration::from_secs(5))).await.unwrap() {
        Reply::SpeakFinished { id } => id,
        other => panic!("expected SpeakFinished, got {other:?}"),
    };
    assert_eq!(started_id, finished_id);
    assert_eq!(h.recorder.read_alouds().len(), 1);

    // 3. Held ReadAloud: dropping the client socket immediately invokes stop_speech(Some(id)).
    h.recorder.set_hold_read_aloud(true);
    let mut held_client = ipc::Client::send(
        &h.socket,
        &Request::ReadAloud {
            text: "Long paragraph being spoken aloud.".to_string(),
        },
    )
    .await
    .unwrap();
    let held_id = match held_client
        .next(Some(Duration::from_secs(5)))
        .await
        .unwrap()
    {
        Reply::SpeakStarted { id } => id,
        other => panic!("expected SpeakStarted, got {other:?}"),
    };
    assert!(h.status().await.speaking);
    drop(held_client);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if h.recorder.stopped_speech().contains(&Some(held_id)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(
        h.recorder.stopped_speech().contains(&Some(held_id)),
        "expected stop_speech(Some({held_id})) on disconnect, got {:?}",
        h.recorder.stopped_speech()
    );
    assert!(!h.status().await.speaking);

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyring_wait_blocks_agent_start_until_unlock_and_drains_queued_tasks() {
    use hey_computer::keyring::MockKeyringWatcher;
    use hey_computer::notify_filter::NotificationAppRule;
    use hey_computer::protocol::NotifyReleaseAction;

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hc-kr-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, SCRIPT).unwrap();
    let log = dir.join("mock.log");
    let socket = dir.join("s.sock");

    let mut config = Config {
        state_dir: dir.join("state"),
        ..Config::default()
    };
    config.agent.command = vec![
        env!("CARGO_BIN_EXE_hey-computer").to_string(),
        "debug".to_string(),
        "mock-agent".to_string(),
        script_path.to_string_lossy().into_owned(),
        "--log".to_string(),
        log.to_string_lossy().into_owned(),
    ];
    config.agent.warm_start = true;
    config.agent.wait_for_keyring = true;
    config.notifications.enabled = true;
    config.notifications.apps.insert(
        "signal".to_string(),
        NotificationAppRule {
            match_apps: vec!["signal".to_string()],
            allow_senders: vec!["Alice".to_string()],
            ..NotificationAppRule::default()
        },
    );

    let keyring = MockKeyringWatcher::locked();
    let recorder = Recorder::new();
    let presenter: Arc<dyn Presenter> = recorder.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn({
        let socket = socket.clone();
        let kr = Arc::clone(&keyring);
        async move {
            daemon::serve_with_deps(
                config,
                &socket,
                presenter,
                Arc::new(hey_computer::context::SystemContext),
                kr,
                async {
                    let _ = stopped.await;
                },
            )
            .await
        }
    });

    // 1. Socket binds immediately and reports AgentState::Locked without spawning the agent.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "socket never appeared");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Reply::Status(st) = ipc::request(&socket, &Request::Status).await.unwrap()
            && st.agent == AgentState::Locked
        {
            break;
        }
        assert!(Instant::now() < deadline, "status never reached Locked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !log.exists(),
        "agent must not spawn while keyring is locked"
    );

    // 2. NotifyHold fails open immediately (< 500 ms) with show_raw while locked.
    let hold_start = Instant::now();
    let hold_reply = ipc::request(
        &socket,
        &Request::NotifyHold {
            noti_id: 1,
            replaces_id: 0,
            app_name: "Signal".to_string(),
            desktop_entry: "signal".to_string(),
            summary: "Alice".to_string(),
            body: "Ping".to_string(),
            category: "im.received".to_string(),
            urgency: "normal".to_string(),
            has_default_action: true,
            dnd: false,
        },
    )
    .await
    .unwrap();
    assert!(hold_start.elapsed() < Duration::from_millis(500));
    assert!(matches!(
        hold_reply,
        Reply::NotifyRelease {
            action: NotifyReleaseAction::ShowRaw,
            reason: Some(ref r),
            ..
        } if r == "keyring_locked"
    ));

    // 3. Background ask stays queued while locked (does not fail).
    let (bg_id, _bg_client) = ipc::ask_request(
        &socket,
        &Request::Ask {
            text: "plain bg while locked".to_string(),
            wait: false,
            new: true,
            profile: None,
            background: true,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Cli,
            reply_to: None,
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();

    // 4. Interactive waiting ask fails fast with FailReason::KeyringLocked after the 1.5s grace.
    let (_id1, mut fg_locked) = ipc::ask(&socket, "plain while locked", true, true)
        .await
        .unwrap();
    let locked_reply = fg_locked.next(Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!(failure(&locked_reply).0, FailReason::KeyringLocked);
    assert!(!log.exists(), "agent still must not have spawned");

    // 5. Unlocking during the 1.5s grace lets a waiting interactive ask succeed
    //    AND drains the queued background task!
    let (_id2, mut fg_unlock) = ipc::ask(&socket, "plain after unlock", true, true)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    keyring.unlock();

    let unlocked_reply = fg_unlock.next(Some(Duration::from_secs(10))).await.unwrap();
    assert_eq!(answer(&unlocked_reply), "Hello from the mock.");

    let bg_done = recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::TaskComplete { id, .. } if *id == bg_id))
        })
        .await;
    assert!(
        bg_done.is_some(),
        "queued background task must drain on unlock"
    );

    let _ = stop.send(());
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_tool_icons_propagate_wipe_on_tier2_reset_and_forward_in_scheduler() {
    use hey_computer::config::{McpServer, Profile};
    use hey_computer::store::{self, FailureDeliver, NewSchedule};

    let script = r##"{
      "turns": [
        {
          "match": "icons_ok",
          "steps": [
            {"permission": {"command": "conversations_history", "server": "slack"}},
            {"permission": {"command": "search_gmail_messages", "server": "google-workspace"}},
            {"permission": {"command": "gh pr list", "kind": "execute"}},
            {"chunk": "All three checked."}
          ]
        },
        {
          "match": "icons_escalate",
          "steps": [
            {"permission": {"command": "conversations_history", "server": "slack"}},
            {"stop": "end_turn"}
          ]
        },
        {
          "match": "sched_slack",
          "steps": [
            {"permission": {"command": "channels_list", "server": "slack"}},
            {"chunk": "Slack checked."}
          ]
        }
      ],
      "default": ["echo"]
    }"##;

    let h = Harness::start(script, |cfg| {
        cfg.present.tool_icons.enabled = true;
        cfg.mcp.insert(
            "slack".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg.mcp.insert(
            "google-workspace".to_string(),
            McpServer {
                command: "/bin/true".to_string(),
                ..McpServer::default()
            },
        );
        cfg.profiles.insert(
            "fast".to_string(),
            Profile {
                thought_level: Some("low".to_string()),
                ..Profile::default()
            },
        );
        cfg.profiles.insert(
            "deep".to_string(),
            Profile {
                thought_level: Some("high".to_string()),
                ..Profile::default()
            },
        );
        cfg.routing.enabled = true;
        cfg.routing.fast_profile = "fast".to_string();
        cfg.routing.deep_profile = "deep".to_string();
        cfg.routing.triage_non_voice = false;
        cfg.routing.escalate_on_failure = true;
    })
    .await;
    h.wait_log("new mock-1").await;

    // 1. Waiting ask returns sequential tool_icons in Reply::Answer.
    let reply = h.ask("icons_ok", true).await;
    match reply {
        Reply::Answer {
            text, tool_icons, ..
        } => {
            assert_eq!(text, "All three checked.");
            assert_eq!(tool_icons, vec!["\u{e8a4}", "\u{f42f}", "\u{e709}"]);
        }
        other => panic!("expected Answer, got {other:?}"),
    }

    // 2. Non-waiting ask emits progressive ProgressUpdate icons and final Answer icons.
    let (id2, _) = ipc::ask(&h.socket, "icons_ok", false, true).await.unwrap();
    let items = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Answer { id, .. } if *id == id2))
        })
        .await
        .expect("Answer presented");
    let answer_icons = items.iter().find_map(|i| match i {
        Presentation::Answer { id, icons, .. } if *id == id2 => Some(icons.clone()),
        _ => None,
    });
    assert_eq!(
        answer_icons,
        Some(vec![
            "\u{e8a4}".to_string(),
            "\u{f42f}".to_string(),
            "\u{e709}".to_string(),
        ])
    );
    let progress_updates: Vec<Vec<String>> = items
        .iter()
        .filter_map(|i| match i {
            Presentation::ProgressUpdate { id, icons, .. } if *id == id2 => Some(icons.clone()),
            _ => None,
        })
        .collect();
    assert!(
        progress_updates
            .iter()
            .any(|v| v == &vec!["\u{e8a4}".to_string()]),
        "expected first Slack icon update in {progress_updates:?}"
    );
    assert!(
        progress_updates.iter().any(|v| {
            v == &vec![
                "\u{e8a4}".to_string(),
                "\u{f42f}".to_string(),
                "\u{e709}".to_string(),
            ]
        }),
        "expected final 3-icon update in {progress_updates:?}"
    );

    // 3. Tier 2 smart-routing escalation emits a fresh Progress (wiping failed fast-attempt icons).
    let (id3, _) = ipc::ask(&h.socket, "icons_escalate", false, true)
        .await
        .unwrap();
    let items3 = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items
                .iter()
                .any(|i| matches!(i, Presentation::Failure { id, .. } if *id == id3))
        })
        .await
        .expect("escalated failure presented");
    let progress_starts: Vec<Option<String>> = items3
        .iter()
        .filter_map(|i| match i {
            Presentation::Progress { id, profile, .. } if *id == id3 => Some(profile.clone()),
            _ => None,
        })
        .collect();
    assert!(
        progress_starts.len() >= 2 && progress_starts.last() == Some(&Some("deep".to_string())),
        "Tier 2 escalation must re-emit Progress for deep profile to wipe fast icons: {progress_starts:?}"
    );

    // 4. Scheduler forwards Reply::Answer.tool_icons into Presentation::TaskComplete.icons.
    let state_dir = h.dir.join("state");
    let now_ms = store::unix_timestamp_ms();
    store::create_schedule(
        &state_dir,
        &NewSchedule {
            name: "slack-check".to_string(),
            schedule_spec: "daily 08:00".to_string(),
            prompt: "sched_slack".to_string(),
            profile: "deep".to_string(),
            script: String::new(),
            no_agent: false,
            suppress_token: None,
            failure_deliver: FailureDeliver::Notify,
            max_runs: None,
            expires_at: None,
            next_run_at: now_ms + 86_400_000,
        },
        128,
    )
    .unwrap();
    let run_reply = ipc::request(
        &h.socket,
        &Request::ScheduleRun {
            target: "slack-check".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(run_reply, Reply::Ok { .. }), "{run_reply:?}");

    let items4 = h
        .recorder
        .wait_for(Duration::from_secs(10), |items| {
            items.iter().any(|i| {
                matches!(
                    i,
                    Presentation::TaskComplete { request, .. } if request == "slack-check"
                )
            })
        })
        .await
        .expect("scheduled TaskComplete presented");
    let sched_icons = items4.iter().find_map(|i| match i {
        Presentation::TaskComplete { request, icons, .. } if request == "slack-check" => {
            Some(icons.clone())
        }
        _ => None,
    });
    assert_eq!(sched_icons, Some(vec!["\u{e8a4}".to_string()]));

    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_hold_enrichment_disconnect_cancel_draft_resume_restart_and_restricted_taint() {
    use hey_computer::notify_filter::NotificationAppRule;
    use hey_computer::protocol::{DraftAction, NotifyReleaseAction};
    use hey_computer::store::{self, Lane};

    let script = r##"{
      "turns": [
        {"match": "try shell", "steps": [{"permission": {"command": "echo hacked", "kind": "execute"}}, {"chunk": "never"}]},
        {"match": "quiet ping", "steps": [{"chunk": "HEARTBEAT_OK"}]},
        {"match": "dinner at 7", "steps": [{"chunk": "CONTEXT: Dinner invite\nDRAFT: Sounds great, see you at 7!"}]},
        {"match": "hold hang", "steps": ["hang"]}
      ],
      "default": [{"chunk": "refined restricted draft"}]
    }"##;

    let configure = |cfg: &mut Config| {
        cfg.notifications.enabled = true;
        cfg.notifications.max_hold_secs = 30.0;
        cfg.notifications.max_turn_secs = 20.0;
        cfg.notifications.max_tool_calls = 0;
        cfg.notifications.per_sender_cooldown_secs = 0.0;
        cfg.notifications.dedupe_window_secs = 0.0;
        cfg.present.clipboard = Vec::new();
        cfg.notifications.apps.insert(
            "signal".to_string(),
            NotificationAppRule {
                match_apps: vec!["signal".to_string()],
                allow_senders: vec!["Alice".to_string()],
                ..NotificationAppRule::default()
            },
        );
    };

    let mut h = Harness::start(script, configure).await;
    h.wait_log("new mock-1").await;

    // 1. Unallowlisted app drops immediately (< 50 ms) with show_raw and 0 LLM calls.
    let t0 = Instant::now();
    let drop_reply = ipc::request(
        &h.socket,
        &Request::NotifyHold {
            noti_id: 1,
            replaces_id: 0,
            app_name: "Spotify".to_string(),
            desktop_entry: "spotify".to_string(),
            summary: "Track".to_string(),
            body: "Artist".to_string(),
            category: String::new(),
            urgency: "normal".to_string(),
            has_default_action: false,
            dnd: false,
        },
    )
    .await
    .unwrap();
    assert!(t0.elapsed() < Duration::from_millis(200));
    assert!(matches!(
        drop_reply,
        Reply::NotifyRelease {
            action: NotifyReleaseAction::ShowRaw,
            reason: Some(ref r),
            ..
        } if r == "app_not_allowlisted"
    ));

    // 2. Allowed Signal message returning HEARTBEAT_OK -> NotifyAccepted then ShowRaw.
    let mut c_hb = Client::send(
        &h.socket,
        &Request::NotifyHold {
            noti_id: 2,
            replaces_id: 0,
            app_name: "Signal".to_string(),
            desktop_entry: "signal".to_string(),
            summary: "Alice".to_string(),
            body: "quiet ping".to_string(),
            category: "im.received".to_string(),
            urgency: "normal".to_string(),
            has_default_action: true,
            dnd: false,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        c_hb.next(Some(Duration::from_secs(5))).await.unwrap(),
        Reply::NotifyAccepted { max_hold_ms: 30000 }
    ));
    assert!(matches!(
        c_hb.next(Some(Duration::from_secs(10))).await.unwrap(),
        Reply::NotifyRelease {
            action: NotifyReleaseAction::ShowRaw,
            ..
        }
    ));

    // 3. Actionable Signal message -> NotifyAccepted then ShowEnriched with persisted token & turn_id.
    let mut c_ok = Client::send(
        &h.socket,
        &Request::NotifyHold {
            noti_id: 3,
            replaces_id: 0,
            app_name: "Signal".to_string(),
            desktop_entry: "signal".to_string(),
            summary: "Alice".to_string(),
            body: "Are we still on for dinner at 7?".to_string(),
            category: "im.received".to_string(),
            urgency: "normal".to_string(),
            has_default_action: true,
            dnd: false,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        c_ok.next(Some(Duration::from_secs(5))).await.unwrap(),
        Reply::NotifyAccepted { max_hold_ms: 30000 }
    ));
    let (draft_turn_id, draft_token) = match c_ok.next(Some(Duration::from_secs(10))).await.unwrap()
    {
        Reply::NotifyRelease {
            action: NotifyReleaseAction::ShowEnriched,
            turn_id: Some(tid),
            token: Some(tok),
            context_note: Some(ctx),
            draft_text: Some(draft),
            readonly: Some(true),
            ..
        } => {
            assert_eq!(ctx, "Dinner invite");
            assert_eq!(draft, "Sounds great, see you at 7!");
            assert!(tok.starts_with("hc_d_"));
            (tid, tok)
        }
        other => panic!("expected ShowEnriched, got {other:?}"),
    };

    // 4. reply_to taint guard: replying to the restricted draft turn stays pinned to Lane::Restricted
    //    and refuses execute tools!
    let (reply_id, mut c_taint) = ipc::ask_request(
        &h.socket,
        &Request::Ask {
            text: "try shell".to_string(),
            wait: true,
            new: false,
            profile: None,
            background: false,
            suppress_token: None,
            attachments: vec![],
            source: InputSource::Typed,
            reply_to: Some(draft_turn_id),
            stream: false,
            distill: false,
            schedule_id: None,
        },
    )
    .await
    .unwrap();
    let taint_reply = c_taint.next(Some(Duration::from_secs(10))).await.unwrap();
    assert!(
        matches!(
            taint_reply,
            Reply::Failed {
                reason: FailReason::LimitReached | FailReason::Denied,
                ..
            }
        ),
        "restricted reply_to must block tool calls: {taint_reply:?}"
    );

    // 5. Client disconnect during NotifyHold cancels the restricted turn promptly.
    let mut c_hang = Client::send(
        &h.socket,
        &Request::NotifyHold {
            noti_id: 4,
            replaces_id: 0,
            app_name: "Signal".to_string(),
            desktop_entry: "signal".to_string(),
            summary: "Alice".to_string(),
            body: "hold hang".to_string(),
            category: "im.received".to_string(),
            urgency: "normal".to_string(),
            has_default_action: true,
            dnd: false,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        c_hang.next(Some(Duration::from_secs(5))).await.unwrap(),
        Reply::NotifyAccepted { .. }
    ));
    h.wait_log("hold hang").await;
    drop(c_hang);
    h.wait_log("turn cancelled").await;

    // 6. Restart survival + strict read-only DraftResume:
    //    Stop daemon, verify restricted lane persisted in SQLite, start a brand new
    //    daemon on the same state_dir, and claim the draft token (Paste degrades to Copy on Signal).
    let state_dir = h.dir.join("state");
    let socket2 = h.dir.join("s2.sock");
    let script_path = h.dir.join("script.json");
    let log2 = h.dir.join("mock2.log");
    h.shut_down().await;

    let draft_turn = store::get_turn(&state_dir, draft_turn_id).unwrap().unwrap();
    assert_eq!(draft_turn.lane, Lane::Restricted);
    let reply_turn = store::get_turn(&state_dir, reply_id).unwrap().unwrap();
    assert_eq!(reply_turn.lane, Lane::Restricted);

    let mut cfg2 = Config {
        state_dir: state_dir.clone(),
        ..Config::default()
    };
    cfg2.agent.command = vec![
        env!("CARGO_BIN_EXE_hey-computer").to_string(),
        "debug".to_string(),
        "mock-agent".to_string(),
        script_path.to_string_lossy().into_owned(),
        "--log".to_string(),
        log2.to_string_lossy().into_owned(),
    ];
    cfg2.agent.warm_start = false;
    configure(&mut cfg2);

    let rec2 = Recorder::new();
    let (stop2, stopped2) = oneshot::channel::<()>();
    let sock2_clone = socket2.clone();
    let task2 = tokio::spawn(async move {
        daemon::serve(cfg2, &sock2_clone, rec2, async {
            let _ = stopped2.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket2.exists() {
        assert!(Instant::now() < deadline, "restarted socket never appeared");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let resumed = ipc::request(
        &socket2,
        &Request::DraftResume {
            token: draft_token.clone(),
            action: DraftAction::Paste,
        },
    )
    .await
    .unwrap();
    match resumed {
        Reply::DraftResumed {
            turn_id,
            action,
            status,
            ..
        } => {
            assert_eq!(turn_id, Some(draft_turn_id));
            // Strict Signal read-only guarantee: Paste degrades to Copy.
            assert_eq!(action, DraftAction::Copy);
            assert_eq!(status, "claimed");
        }
        other => panic!("expected DraftResumed(claimed), got {other:?}"),
    }

    // Replaying the same single-use token returns already_used.
    let replay = ipc::request(
        &socket2,
        &Request::DraftResume {
            token: draft_token,
            action: DraftAction::Copy,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        replay,
        Reply::DraftResumed {
            status,
            ..
        } if status == "already_used"
    ));

    let _ = stop2.send(());
    task2.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&h.dir);
}

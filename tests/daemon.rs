//! The daemon end to end, with the scripted mock agent in place of agy.
//!
//! Each test starts the daemon in-process on its own socket, with a recording
//! presenter, and the agent command pointing at `hey-computer debug
//! mock-agent`. The mock logs what it saw, which is how a test checks what the
//! daemon sent it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hey_computer::config::Config;
use hey_computer::daemon;
use hey_computer::ipc::{self, Client};
use hey_computer::present::{Presentation, Presenter, Recorder};
use hey_computer::protocol::{AgentState, FailReason, Reply, Request};
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
                request: "plain".to_string()
            },
            Presentation::Answer {
                id,
                text: "Hello from the mock.".to_string(),
                denied: vec![]
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
    assert!(message.contains("root"), "{message}");
    assert_eq!(h.log_count("permission reject_once sudo -n true"), 1);
    // The agent stays up and the conversation goes on.
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
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

    let reply = ipc::request(&h.socket, &Request::Cancel).await.unwrap();
    assert_eq!(reply, Reply::Cancelled { id: Some(id) });
    let reply = waiting.next(Some(Duration::from_secs(5))).await.unwrap();
    let (reason, _) = failure(&reply);
    assert_eq!(reason, FailReason::Cancelled);

    // Nothing left to cancel.
    let reply = ipc::request(&h.socket, &Request::Cancel).await.unwrap();
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
    let started = Instant::now();
    let reply = h.ask("slow", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::Timeout);
    assert!(message.contains("sent nothing"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(h.log_count("cancel mock-1"), 1);
    // The agent answered the cancel, so it stays.
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
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
            message: "The daemon is shutting down.".to_string()
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
            message: "A newer request replaced this one.".to_string()
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

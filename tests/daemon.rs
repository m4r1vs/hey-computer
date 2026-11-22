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
    let error = daemon::serve(Config::default(), &h.socket, presenter, async {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already listening"), "{error}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_login_is_reported_in_the_agents_words() {
    let script = r#"{"session_error": "Authentication required: sign in again with agy", "default": ["echo"]}"#;
    let h = Harness::start(script, |_| {}).await;
    let reply = h.ask("anything", false).await;
    let (reason, message) = failure(&reply);
    assert_eq!(reason, FailReason::AgentError);
    assert_eq!(message, "Authentication required: sign in again with agy");
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
async fn a_cold_agent_starts_on_the_first_request_and_the_vault_joins_the_workspace() {
    let h = Harness::start(SCRIPT, |c| {
        c.agent.warm_start = false;
        c.instructions.vault = PathBuf::from("/tmp/Someone's Vault");
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.log_count("start"), 0, "nothing starts before a request");
    assert_eq!(h.status().await.agent, AgentState::Down);
    assert_eq!(answer(&h.ask("plain", false).await), "Hello from the mock.");
    assert_eq!(
        h.log_count(r#"new mock-1 dirs=["/tmp/Someone's Vault"]"#),
        1,
        "{:?}",
        h.log_lines()
    );
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

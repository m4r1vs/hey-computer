//! Integration tests for `hey-computer-bridge` (strictly read-only WhatsApp & Signal bridge).
//!
//! Uses invented phone numbers and contact names only (`+4915100000000`,
//! `+4915100000001`, `"Alice Test"`, etc.) and self-contained POSIX `/bin/sh`
//! + in-process Rust Unix socket mocks so tests run hermetically inside `nix flake check`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use hey_computer::bridge::config::BridgeConfig;
use hey_computer::bridge::protocol::{
    BridgeEvent, BridgeRequest, ContextAuthor, MAX_BRIDGE_LINE, MessageKind, Platform,
    PlatformState,
};
use hey_computer::bridge::{self, Subscriber};
use hey_computer::lines::{self, Line};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(label: &str) -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "hc-bridge-test-{}-{}-{n}",
        std::process::id(),
        label
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if socket.exists() && UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("bridge socket did not become ready at {}", socket.display());
}

async fn send_raw_line(socket: &Path, line: &str) -> BridgeEvent {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    write.write_all(line.as_bytes()).await.unwrap();
    write.write_all(b"\n").await.unwrap();
    write.flush().await.unwrap();
    let mut reader = BufReader::new(read);
    match lines::read(&mut reader, MAX_BRIDGE_LINE).await.unwrap() {
        Line::Text(raw) => serde_json::from_str(&raw).unwrap(),
        other => panic!("unexpected raw socket reply: {other:?}"),
    }
}

#[tokio::test]
async fn read_only_false_config_is_rejected() {
    let err = BridgeConfig::parse_toml("read_only = false\n").unwrap_err();
    assert!(
        err.to_string().contains("strictly read-only"),
        "unexpected error: {err:#}"
    );

    let dir = scratch_dir("readonly-cfg");
    let socket = dir.join("bridge.sock");
    let cfg = BridgeConfig {
        state_dir: dir.join("state"),
        read_only: false,
        ..BridgeConfig::default()
    };
    let err = bridge::serve(cfg, &socket, std::future::pending())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("strictly read-only"));
    assert!(!socket.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_socket_request_is_rejected_as_strictly_read_only() {
    let dir = scratch_dir("readonly-sock");
    let socket = dir.join("bridge.sock");
    let cfg = BridgeConfig {
        state_dir: dir.join("state"),
        ..BridgeConfig::default()
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let sock_clone = socket.clone();
    let task = tokio::spawn(async move {
        bridge::serve(cfg, &sock_clone, async {
            let _ = stop_rx.await;
        })
        .await
    });
    wait_for_socket(&socket).await;

    // Socket permissions must be strictly 0600.
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    for payload in [
        r#"{"type":"send","to":"+4915100000001","text":"should never send"}"#,
        r#"{"type":"read","id":"msg-1"}"#,
        r#"{"type":"typing","to":"+4915100000001"}"#,
        r#"{"type":"status","extra_field":true}"#,
    ] {
        match send_raw_line(&socket, payload).await {
            BridgeEvent::Error { message } => {
                assert!(
                    message.contains("bridge is strictly read-only"),
                    "expected read-only rejection for {payload}, got {message}"
                );
            }
            other => panic!("expected Error for {payload}, got {other:?}"),
        }
    }

    match bridge::query(&socket, &BridgeRequest::Status)
        .await
        .unwrap()
    {
        BridgeEvent::Status {
            read_only,
            whatsapp,
            signal,
            tracked_chats,
            ..
        } => {
            assert!(read_only);
            assert_eq!(whatsapp, PlatformState::Disabled);
            assert_eq!(signal, PlatformState::Disabled);
            assert_eq!(tracked_chats, 0);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn whatsapp_lid_allowlist_burst_append_self_chat_and_get_context() {
    let dir = scratch_dir("whatsapp");
    let socket = dir.join("bridge.sock");
    let allow_file = dir.join("allow.txt");
    std::fs::write(&allow_file, "# Test allowlist\n+49 151 0000 0001\n").unwrap();

    let script = dir.join("mock-baileys.sh");
    let t_now = now_ms();
    let t_old = t_now - 300_000; // 5 minutes ago (> 120s age fence -> context only)
    write_executable(
        &script,
        &format!(
            r#"#!/bin/sh
set -eu
# Wait slightly so the test subscriber connects after Ready.
printf '{{"type":"state","state":"connected","self_jid":"4915100000000@s.whatsapp.net","self_lid":"111222333444@lid","self_e164":"+4915100000000"}}\n'
sleep 0.15
# 1. Historical catch-up from Alice via @lid + remoteJidAlt (must buffer only, never emit Message)
printf '{{"type":"message","upsert_type":"append","id":"wa-old-1","remote_jid":"99887766554433@lid","remote_jid_alt":"4915100000001@s.whatsapp.net","from_me":false,"is_group":false,"is_self_chat":false,"push_name":"Alice Test","timestamp_ms":{t_old},"text":"Earlier message from Alice"}}\n'
# 2. Stranger not on allowlist (must be ignored)
printf '{{"type":"message","upsert_type":"notify","id":"wa-stranger-1","remote_jid":"4915100000099@s.whatsapp.net","from_me":false,"is_group":false,"is_self_chat":false,"push_name":"Stranger Test","timestamp_ms":{t_now},"text":"Spam message"}}\n'
# 3. Live 2-message burst from Alice via @lid + remoteJidAlt (must coalesce into 1 DirectMessage)
printf '{{"type":"message","upsert_type":"notify","id":"wa-live-1","remote_jid":"99887766554433@lid","remote_jid_alt":"4915100000001@s.whatsapp.net","from_me":false,"is_group":false,"is_self_chat":false,"push_name":"Alice Test","timestamp_ms":{t1},"text":"Hey!"}}\n'
printf '{{"type":"message","upsert_type":"notify","id":"wa-live-2","remote_jid":"99887766554433@lid","remote_jid_alt":"4915100000001@s.whatsapp.net","from_me":false,"is_group":false,"is_self_chat":false,"push_name":"Alice Test","timestamp_ms":{t2},"text":"Still on for 18:30?"}}\n'
sleep 0.25
# 4. WhatsApp append self-chat from own phone
printf '{{"type":"message","upsert_type":"append","id":"wa-self-1","remote_jid":"111222333444@lid","from_me":true,"is_group":false,"is_self_chat":true,"push_name":"Me","timestamp_ms":{t3},"text":"Turn off living room lights"}}\n'
sleep 0.2
# 5. Outbound reply from phone to Alice -> context author:me + user_replied
printf '{{"type":"message","upsert_type":"append","id":"wa-out-1","remote_jid":"99887766554433@lid","remote_jid_alt":"4915100000001@s.whatsapp.net","from_me":true,"is_group":false,"is_self_chat":false,"timestamp_ms":{t4},"text":"Yes, see you at 18:30!"}}\n'
sleep 30
"#,
            t_old = t_old,
            t_now = t_now,
            t1 = t_now + 10,
            t2 = t_now + 20,
            t3 = t_now + 30,
            t4 = t_now + 40,
        ),
    );

    let mut cfg = BridgeConfig {
        state_dir: dir.join("state"),
        debounce_ms: 80,
        self_chat_debounce_ms: 60,
        max_debounce_ms: 300,
        ..BridgeConfig::default()
    };
    cfg.whatsapp.enabled = true;
    cfg.whatsapp.self_chat = true;
    cfg.whatsapp.allow_from_file = Some(allow_file);
    cfg.whatsapp.reader_command =
        vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()];

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let sock_clone = socket.clone();
    let task = tokio::spawn(async move {
        bridge::serve(cfg, &sock_clone, async {
            let _ = stop_rx.await;
        })
        .await
    });
    wait_for_socket(&socket).await;

    let (ready, mut sub) = Subscriber::connect(&socket).await.unwrap();
    assert!(matches!(
        ready,
        BridgeEvent::Ready {
            read_only: true,
            ..
        }
    ));

    let mut saw_dm = false;
    let mut saw_self = false;
    let mut saw_user_replied = false;

    for _ in 0..10 {
        let ev = sub.next(Some(Duration::from_secs(5))).await.unwrap();
        match ev {
            BridgeEvent::PlatformState { .. } => {}
            BridgeEvent::Message {
                platform,
                kind: MessageKind::DirectMessage,
                chat_id,
                sender_name,
                text,
                recent_messages,
                ..
            } => {
                assert_eq!(platform, Platform::Whatsapp);
                assert_eq!(chat_id, "+4915100000001");
                assert_eq!(sender_name, "Alice Test");
                assert_eq!(text, "Hey!\nStill on for 18:30?");
                // Context must include historical catch-up + both live messages.
                assert_eq!(recent_messages.len(), 3);
                assert_eq!(recent_messages[0].text, "Earlier message from Alice");
                assert_eq!(recent_messages[1].text, "Hey!");
                assert_eq!(recent_messages[2].text, "Still on for 18:30?");
                saw_dm = true;
            }
            BridgeEvent::Message {
                platform,
                kind: MessageKind::SelfChat,
                text,
                ..
            } => {
                assert_eq!(platform, Platform::Whatsapp);
                assert_eq!(text, "Turn off living room lights");
                saw_self = true;
            }
            BridgeEvent::UserReplied {
                platform, chat_id, ..
            } => {
                assert_eq!(platform, Platform::Whatsapp);
                assert_eq!(chat_id, "+4915100000001");
                saw_user_replied = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
        if saw_dm && saw_self && saw_user_replied {
            break;
        }
    }
    assert!(saw_dm && saw_self && saw_user_replied);

    // GetContext lookup must work by contact name, E.164 phone number, and @lid alias!
    for query_key in ["Alice Test", "+4915100000001", "99887766554433@lid"] {
        match bridge::query(
            &socket,
            &BridgeRequest::GetContext {
                platform: Some(Platform::Whatsapp),
                sender_or_chat: query_key.to_string(),
                limit: None,
            },
        )
        .await
        .unwrap()
        {
            BridgeEvent::Context {
                platform,
                chat_id,
                sender_name,
                messages,
            } => {
                assert_eq!(platform, Some(Platform::Whatsapp));
                assert_eq!(chat_id.as_deref(), Some("+4915100000001"));
                assert_eq!(sender_name.as_deref(), Some("Alice Test"));
                assert_eq!(messages.len(), 4);
                assert_eq!(messages[3].author, ContextAuthor::Me);
                assert_eq!(messages[3].text, "Yes, see you at 18:30!");
            }
            other => panic!("expected Context for {query_key}, got {other:?}"),
        }
    }

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn signal_note_to_self_outbound_context_and_read_only_rpc() {
    let dir = scratch_dir("signal");
    let socket = dir.join("bridge.sock");
    let args_file = dir.join("signal_args.txt");
    let sock_marker = dir.join("signal_sock.txt");
    let account_file = dir.join("signal_account.txt");
    std::fs::write(&account_file, "+4915100000000\n").unwrap();

    // Minimal POSIX /bin/sh mock `signal-cli` that records its exact arguments
    // and tells the in-process Rust mock RPC server where `--socket` should bind.
    let mock_signal_sh = dir.join("mock-signal-cli.sh");
    write_executable(
        &mock_signal_sh,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" > "{args}"
sock=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--socket" ]; then
    sock="$2"
    shift 2
  else
    shift
  fi
done
printf '%s' "$sock" > "{marker}.tmp"
mv "{marker}.tmp" "{marker}"
trap 'exit 0' TERM INT
sleep 30 &
wait
"#,
            args = args_file.display(),
            marker = sock_marker.display(),
        ),
    );

    let mut cfg = BridgeConfig {
        state_dir: dir.join("state"),
        debounce_ms: 80,
        self_chat_debounce_ms: 60,
        max_debounce_ms: 300,
        ..BridgeConfig::default()
    };
    cfg.signal.enabled = true;
    cfg.signal.self_chat = true;
    cfg.signal.account_file = Some(account_file);
    cfg.signal.allow_from = vec!["+4915100000001".to_string()];
    cfg.signal.command = vec![
        "/bin/sh".to_string(),
        mock_signal_sh.to_string_lossy().into_owned(),
    ];

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let sock_clone = socket.clone();
    let bridge_task = tokio::spawn(async move {
        bridge::serve(cfg, &sock_clone, async {
            let _ = stop_rx.await;
        })
        .await
    });
    wait_for_socket(&socket).await;

    // Wait for mock-signal-cli.sh to report the `--socket` path.
    let signal_sock_path = loop {
        if let Ok(s) = std::fs::read_to_string(&sock_marker)
            && !s.trim().is_empty()
        {
            break PathBuf::from(s.trim());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // Assert exact read-only flags passed to signal-cli.
    let args = std::fs::read_to_string(&args_file).unwrap();
    assert!(args.contains("--no-receive-stdout"), "args: {args}");
    assert!(args.contains("--receive-mode manual"), "args: {args}");
    assert!(args.contains("--ignore-stories"), "args: {args}");
    assert!(args.contains("--ignore-attachments"), "args: {args}");
    assert!(
        !args.contains("--send-read-receipts"),
        "signal-cli must NEVER enable read receipts: {args}"
    );
    assert!(
        !args.contains("--http"),
        "signal-cli must use --socket, not --http: {args}"
    );

    // Connect subscriber before the mock signal RPC server streams envelopes.
    let (_ready, mut sub) = Subscriber::connect(&socket).await.unwrap();

    let t = now_ms();
    let mock_rpc_task = tokio::spawn(async move {
        let listener = UnixListener::bind(&signal_sock_path).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        let mut methods = Vec::new();
        for _ in 0..3 {
            let Line::Text(raw) = lines::read(&mut reader, MAX_BRIDGE_LINE).await.unwrap() else {
                panic!("expected RPC line");
            };
            let req: Value = serde_json::from_str(&raw).unwrap();
            let id = req.get("id").cloned().unwrap();
            let method = req
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            methods.push(method.clone());
            let result = match method.as_str() {
                "version" => json!({"version": "0.14.10"}),
                "listContacts" => json!([
                    {"number": "+4915100000000", "uuid": "11111111-1111-1111-1111-111111111111"},
                    {"number": "+4915100000001", "uuid": "22222222-2222-2222-2222-222222222222", "name": "Alice Test"}
                ]),
                "subscribeReceive" => json!(1),
                other => panic!("unexpected non-read-only signal-cli RPC method: {other}"),
            };
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
            write
                .write_all(format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(methods, vec!["version", "listContacts", "subscribeReceive"]);

        let envelopes = vec![
            // 1. Inbound DM from Alice Test (by UUID, resolved via listContacts)
            json!({
                "sourceUuid": "22222222-2222-2222-2222-222222222222",
                "timestamp": t + 10,
                "dataMessage": {
                    "timestamp": t + 10,
                    "message": "Can you check the train?"
                }
            }),
            // 2. Outbound syncMessage to Alice (must NOT emit Message; records author:me + user_replied)
            json!({
                "sourceNumber": "+4915100000000",
                "sourceUuid": "11111111-1111-1111-1111-111111111111",
                "timestamp": t + 20,
                "syncMessage": {
                    "sentMessage": {
                        "destinationUuid": "22222222-2222-2222-2222-222222222222",
                        "timestamp": t + 20,
                        "message": "Next train is in 8 min"
                    }
                }
            }),
            // 3. Spoofed syncMessage (source != own) -> must be dropped
            json!({
                "sourceNumber": "+4915100000001",
                "timestamp": t + 25,
                "syncMessage": {
                    "sentMessage": {
                        "destinationNumber": "+4915100000000",
                        "timestamp": t + 25,
                        "message": "Forged self chat"
                    }
                }
            }),
            // 4. Verified Note to Self (source == own && destination == own && no groupInfo)
            json!({
                "sourceNumber": "+4915100000000",
                "sourceUuid": "11111111-1111-1111-1111-111111111111",
                "timestamp": t + 30,
                "syncMessage": {
                    "sentMessage": {
                        "destinationNumber": "+4915100000000",
                        "timestamp": t + 30,
                        "message": "Add dentist to daily note"
                    }
                }
            }),
        ];

        for (idx, env) in envelopes.into_iter().enumerate() {
            if idx == 1 {
                // Give the inbound DM debounce a moment to flush before outbound reply arrives.
                tokio::time::sleep(Duration::from_millis(160)).await;
            }
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "receive",
                "params": {
                    "account": "+4915100000000",
                    "envelope": env
                }
            });
            write
                .write_all(format!("{}\n", serde_json::to_string(&notif).unwrap()).as_bytes())
                .await
                .unwrap();
        }
        write.flush().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let mut saw_dm = false;
    let mut saw_user_replied = false;
    let mut saw_note_to_self = false;

    for _ in 0..10 {
        let ev = sub.next(Some(Duration::from_secs(5))).await.unwrap();
        match ev {
            BridgeEvent::PlatformState { .. } => {}
            BridgeEvent::Message {
                platform: Platform::Signal,
                kind: MessageKind::DirectMessage,
                chat_id,
                sender_name,
                text,
                ..
            } => {
                assert_eq!(chat_id, "+4915100000001");
                assert_eq!(sender_name, "Alice Test");
                assert_eq!(text, "Can you check the train?");
                saw_dm = true;
            }
            BridgeEvent::UserReplied {
                platform: Platform::Signal,
                chat_id,
                ..
            } => {
                assert_eq!(chat_id, "+4915100000001");
                saw_user_replied = true;
            }
            BridgeEvent::Message {
                platform: Platform::Signal,
                kind: MessageKind::SelfChat,
                text,
                ..
            } => {
                assert_eq!(text, "Add dentist to daily note");
                saw_note_to_self = true;
            }
            other => panic!("unexpected Signal event: {other:?}"),
        }
        if saw_dm && saw_user_replied && saw_note_to_self {
            break;
        }
    }
    assert!(saw_dm && saw_user_replied && saw_note_to_self);

    // GetContext by contact name returns Alice's inbound DM + our outbound reply (`author: Me`).
    match bridge::query(
        &socket,
        &BridgeRequest::GetContext {
            platform: Some(Platform::Signal),
            sender_or_chat: "Alice Test".to_string(),
            limit: None,
        },
    )
    .await
    .unwrap()
    {
        BridgeEvent::Context {
            platform,
            chat_id,
            messages,
            ..
        } => {
            assert_eq!(platform, Some(Platform::Signal));
            assert_eq!(chat_id.as_deref(), Some("+4915100000001"));
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].author, ContextAuthor::Them);
            assert_eq!(messages[0].text, "Can you check the train?");
            assert_eq!(messages[1].author, ContextAuthor::Me);
            assert_eq!(messages[1].text, "Next train is in 8 min");
        }
        other => panic!("expected Context, got {other:?}"),
    }

    mock_rpc_task.abort();
    let _ = stop_tx.send(());
    bridge_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn whatsapp_401_unlink_emits_auth_required_without_respawn_loop() {
    let dir = scratch_dir("wa-401");
    let socket = dir.join("bridge.sock");
    let spawn_count = dir.join("spawns.txt");
    let script = dir.join("mock-401.sh");
    write_executable(
        &script,
        &format!(
            r#"#!/bin/sh
echo 1 >> "{spawns}"
sleep 0.1
printf '{{"type":"auth_required","detail":"WhatsApp session unlinked (401); run `hey-computer-bridge pair whatsapp`."}}\n'
exit 0
"#,
            spawns = spawn_count.display(),
        ),
    );

    let mut cfg = BridgeConfig {
        state_dir: dir.join("state"),
        ..BridgeConfig::default()
    };
    cfg.whatsapp.enabled = true;
    cfg.whatsapp.reader_command =
        vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()];

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let sock_clone = socket.clone();
    let task = tokio::spawn(async move {
        bridge::serve(cfg, &sock_clone, async {
            let _ = stop_rx.await;
        })
        .await
    });
    wait_for_socket(&socket).await;

    let (_ready, mut sub) = Subscriber::connect(&socket).await.unwrap();
    let mut saw_auth_required = false;
    for _ in 0..5 {
        match sub.next(Some(Duration::from_secs(5))).await.unwrap() {
            BridgeEvent::PlatformState {
                platform: Platform::Whatsapp,
                state: PlatformState::AuthRequired,
                detail,
            } => {
                assert!(detail.unwrap_or_default().contains("401"));
                saw_auth_required = true;
                break;
            }
            BridgeEvent::PlatformState { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_auth_required);

    match bridge::query(&socket, &BridgeRequest::Status)
        .await
        .unwrap()
    {
        BridgeEvent::Status { whatsapp, .. } => assert_eq!(whatsapp, PlatformState::AuthRequired),
        other => panic!("expected Status, got {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(250)).await;
    let lines = std::fs::read_to_string(&spawn_count).unwrap();
    assert_eq!(
        lines.lines().count(),
        1,
        "bridge must not tight-loop respawn after 401 auth_required"
    );

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

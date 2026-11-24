//! hey-computer: a voice assistant daemon that keeps one ACP agent warm.
//!
//! One binary, two roles: `daemon` owns the agent and the socket, every other
//! subcommand is a client that talks to it over the socket and exits.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;

use hey_computer::attachments;
use hey_computer::config::Config;
use hey_computer::ipc::{self, NotRunning};
use hey_computer::present::{self, Urgency};
use hey_computer::protocol::{
    self, Attachment, AttachmentKind, InputSource, MAX_REQUEST_LINE, Reply, Request,
};
use hey_computer::{daemon, debug, mock, scheduler, store};

#[derive(Debug, Parser)]
#[command(name = "hey-computer", version, about, long_about = None)]
struct Cli {
    /// Path to the config file (default: ~/.config/hey-computer/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The daemon's socket (default: $HEY_COMPUTER_SOCKET, then
    /// $XDG_RUNTIME_DIR/hey-computer.sock).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Increase log verbosity; repeat for more.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon that keeps the agent warm.
    Daemon,
    /// Hand the assistant a request: the arguments, or stdin when there are none.
    Ask {
        /// Wait for the answer and print it, instead of having the daemon show it.
        #[arg(long)]
        print: bool,
        /// Start a new conversation, even right after the last answer.
        #[arg(long)]
        new: bool,
        /// Run in the background lane (not superseded by newer asks); prints
        /// the task ID to stdout when not using `--print`.
        #[arg(short = 'b', long)]
        background: bool,
        /// Suppress notifications and stdout when the answer matches this exact
        /// token (e.g. `HEARTBEAT_OK`), exiting 0 cleanly.
        #[arg(long, value_name = "TOKEN")]
        suppress_token: Option<String>,
        /// Use a named profile from `[profiles.<name>]` (or `default`).
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
        /// Tag this request as coming from voice dictation (shorthand for `--source voice`;
        /// skips LLM triage when `[routing]` is enabled).
        #[arg(long, conflicts_with = "source")]
        voice: bool,
        /// Input source (`cli`, `voice`, or `typed`) for automatic routing.
        #[arg(long, value_enum)]
        source: Option<InputSourceArg>,
        /// Attach a UTF-8 text file as embedded context (repeatable).
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        files: Vec<PathBuf>,
        /// Attach a raster image (PNG, JPEG, WebP, GIF) (repeatable).
        #[arg(short = 'i', long = "image", value_name = "PATH")]
        images: Vec<PathBuf>,
        /// The request; several words are joined with spaces.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Cancel the running interactive request (or a specific task ID).
    Cancel {
        /// Specific request or background task ID to cancel.
        #[arg(long, value_name = "ID")]
        id: Option<u64>,
    },
    /// Inspect and manage background tasks and recent turns.
    Tasks {
        #[command(subcommand)]
        action: Option<TasksCommand>,
    },
    /// Create, inspect, and manage scheduled jobs in the daemon's SQLite store.
    Schedule {
        #[command(subcommand)]
        action: Option<ScheduleCommand>,
    },
    /// Search past completed answers using the local SQLite FTS5 index.
    Recall {
        /// Only include turns created at or after this time (`24h`, `7d`, `2w`,
        /// `today`, `yesterday`, or `YYYY-MM-DD`).
        #[arg(long, value_name = "SPEC")]
        since: Option<String>,
        /// Only include turns created before this time.
        #[arg(long, value_name = "SPEC")]
        until: Option<String>,
        /// Maximum number of matches to return (1..=50).
        #[arg(short = 'n', long, default_value_t = 5)]
        limit: usize,
        /// Restrict search to one lane.
        #[arg(long, value_enum, default_value_t = LaneFilter::All)]
        lane: LaneFilter,
        /// Sort order (`relevance` or `recent`).
        #[arg(long, value_enum, default_value_t = RecallSortArg::Relevance)]
        sort: RecallSortArg,
        /// Match any search term (`OR`) instead of all terms (`AND`).
        #[arg(long)]
        any: bool,
        /// Return full request/answer text (truncated to `--max-chars`) instead of snippets.
        #[arg(long)]
        full: bool,
        /// Maximum characters per field when `--full` is set.
        #[arg(long, default_value_t = 1200)]
        max_chars: usize,
        /// Pass the query directly to SQLite FTS5 `MATCH` without sanitization.
        #[arg(long)]
        raw_fts: bool,
        /// Emit results as JSON.
        #[arg(long)]
        json: bool,
        /// Omit `<untrusted_recall>` framing (for direct human terminal piping).
        #[arg(long)]
        plain: bool,
        /// Search terms; omit to list the most recent completed turns.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        query: Vec<String>,
    },
    /// Print what the daemon is doing.
    Status {
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Retrieve the last answer: print it, copy it to the clipboard, or open it in the viewer.
    Last {
        /// Copy the last answer to the clipboard instead of printing it.
        #[arg(long, conflicts_with = "open")]
        copy: bool,
        /// Open the last answer in the Markdown viewer instead of printing it.
        #[arg(long, conflicts_with = "copy")]
        open: bool,
        /// Which lane to read from (`interactive` by default).
        #[arg(long, value_enum, default_value_t = LaneFilter::Interactive)]
        lane: LaneFilter,
        /// Print the record as JSON.
        #[arg(long, conflicts_with_all = ["copy", "open"])]
        json: bool,
    },
    /// Check one part of the program on its own.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        what: DebugCommand,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum InputSourceArg {
    Cli,
    Voice,
    Typed,
}

impl InputSourceArg {
    fn into_source(self) -> InputSource {
        match self {
            Self::Cli => InputSource::Cli,
            Self::Voice => InputSource::Voice,
            Self::Typed => InputSource::Typed,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum FailureDeliverArg {
    Notify,
    Log,
    Silent,
}

impl FailureDeliverArg {
    fn into_deliver(self) -> store::FailureDeliver {
        match self {
            Self::Notify => store::FailureDeliver::Notify,
            Self::Log => store::FailureDeliver::Log,
            Self::Silent => store::FailureDeliver::Silent,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ScheduleCommand {
    /// Add a new scheduled job or reminder.
    Add {
        /// Unique job name (letters, digits, `-`, `_`).
        name: String,
        /// Schedule specification (`every 15m`, `in 20m`, `at 14:30`, `daily 08:00`,
        /// `weekdays 09:56`, `fri 16:00`, or 5-field cron `MIN HOUR DOM MON DOW`).
        #[arg(short = 's', long = "schedule", value_name = "SPEC")]
        schedule: String,
        /// Prompt sent to the assistant (or reminder text when `--no-agent` is set).
        #[arg(long, default_value = "")]
        prompt: String,
        /// Named profile from `[profiles.<name>]` for agent runs.
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
        /// Optional shell pre-collector: exit 0 + empty stdout skips silently (0 LLM tokens);
        /// non-empty stdout wakes the agent (or notifies directly with `--no-agent`).
        #[arg(long, value_name = "CMD")]
        script: Option<String>,
        /// Notify directly without invoking the LLM (timed reminder or direct script alert).
        #[arg(long)]
        no_agent: bool,
        /// Suppress notifications when the agent replies with this exact token.
        #[arg(long, value_name = "TOKEN")]
        suppress_token: Option<String>,
        /// Failure routing (`notify`, `log`, or `silent`).
        #[arg(long, value_enum, default_value_t = FailureDeliverArg::Notify)]
        failure_deliver: FailureDeliverArg,
        /// Disable automatically after this many runs.
        #[arg(long, value_name = "N")]
        max_runs: Option<u64>,
        /// Auto-disable after this time (`in <N><s|m|h|d>`, `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM`).
        #[arg(long, value_name = "SPEC")]
        expires: Option<String>,
    },
    /// List scheduled jobs (enabled jobs by default; works even when daemon is stopped).
    List {
        /// Include paused and expired jobs.
        #[arg(long)]
        all: bool,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show full details of one scheduled job by numeric ID or name.
    Show {
        target: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Edit an existing scheduled job.
    Edit {
        target: String,
        #[arg(short = 's', long = "schedule", value_name = "SPEC")]
        schedule: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
        #[arg(long, value_name = "CMD")]
        script: Option<String>,
        #[arg(long)]
        no_agent: Option<bool>,
        /// Set or clear (`""`) the suppress token.
        #[arg(long, value_name = "TOKEN")]
        suppress_token: Option<String>,
        #[arg(long, value_enum)]
        failure_deliver: Option<FailureDeliverArg>,
        /// Set max runs (0 clears the limit).
        #[arg(long, value_name = "N")]
        max_runs: Option<u64>,
        /// Set or clear (`""`) expiration (`in ...`, `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM`).
        #[arg(long, value_name = "SPEC")]
        expires: Option<String>,
    },
    /// Pause a scheduled job.
    Pause { target: String },
    /// Resume a paused scheduled job.
    Resume { target: String },
    /// Permanently remove a scheduled job.
    Remove { target: String },
    /// Trigger an immediate test run via the running daemon.
    Run { target: String },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum LaneFilter {
    Interactive,
    Background,
    All,
}

impl LaneFilter {
    fn into_lane(self) -> Option<store::Lane> {
        match self {
            Self::Interactive => Some(store::Lane::Interactive),
            Self::Background => Some(store::Lane::Background),
            Self::All => None,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RecallSortArg {
    Relevance,
    Recent,
}

impl RecallSortArg {
    fn into_sort(self) -> store::RecallSort {
        match self {
            Self::Relevance => store::RecallSort::Relevance,
            Self::Recent => store::RecallSort::Recent,
        }
    }
}

#[derive(Debug, Subcommand)]
enum TasksCommand {
    /// List recent tasks (background lane by default).
    List {
        /// Which lane to list.
        #[arg(long, value_enum, default_value_t = LaneFilter::Background)]
        lane: LaneFilter,
        /// Filter by status (queued, running, completed, suppressed, failed, cancelled).
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
        /// Include suppressed heartbeat turns.
        #[arg(long)]
        all: bool,
        /// Maximum rows to display.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the full details and answer of one task.
    Show {
        id: u64,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open a task's answer in the Markdown viewer.
    Open { id: u64 },
    /// Copy a task's answer to the clipboard.
    Copy { id: u64 },
    /// Cancel a running or queued background task.
    Cancel { id: u64 },
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Check the configuration and print it with the defaults filled in.
    Config,
    /// Print the instructions the agent gets in its AGENTS.md.
    Instructions,
    /// Print the ambient context block that would be prepended to a turn.
    Context,
    /// Prepare the agent's directory and print where everything is.
    Prepare,
    /// Say whether an answer would go to a notification or the viewer.
    Classify { text: Vec<String> },
    /// Ask the agent directly, without a daemon, and print the answer.
    Prompt {
        /// Use a named profile from `[profiles.<name>]` (or `default`).
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
        /// Attach a UTF-8 text file as embedded context (repeatable).
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        files: Vec<PathBuf>,
        /// Attach a raster image (PNG, JPEG, WebP, GIF) (repeatable).
        #[arg(short = 'i', long = "image", value_name = "PATH")]
        images: Vec<PathBuf>,
        text: Vec<String>,
    },
    /// Show an answer the way the daemon would.
    Present { text: Vec<String> },
    /// Act as a scripted ACP agent, for tests.
    MockAgent {
        script: PathBuf,
        /// Record what the mock sees in this file.
        #[arg(long, value_name = "FILE")]
        log: Option<PathBuf>,
    },
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "hey_computer=info",
        1 => "hey_computer=debug",
        _ => "hey_computer=trace,debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // The mock agent's stderr ends up in the daemon's log; keep it quiet.
    if !matches!(
        cli.command,
        Command::Debug {
            what: DebugCommand::MockAgent { .. }
        }
    ) {
        init_tracing(cli.verbose);
    }
    match dispatch(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hey-computer: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    if let Command::Debug {
        what: DebugCommand::MockAgent { script, log },
    } = &cli.command
    {
        mock::run(script, log.clone()).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Daemon => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            daemon::run(config, &socket).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ask {
            print,
            new,
            background,
            suppress_token,
            profile,
            voice,
            source,
            files,
            images,
            text,
        } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            let input_source = if voice {
                InputSource::Voice
            } else {
                source
                    .map(InputSourceArg::into_source)
                    .unwrap_or(InputSource::Cli)
            };
            ask(
                &config,
                &socket,
                text,
                print,
                new,
                background,
                suppress_token,
                profile,
                input_source,
                files,
                images,
            )
            .await
        }
        Command::Cancel { id } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            cancel_id(&socket, id).await
        }
        Command::Tasks { action } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            tasks(&config, &socket, action).await
        }
        Command::Schedule { action } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            schedule_cmd(&config, &socket, action).await
        }
        Command::Recall {
            since,
            until,
            limit,
            lane,
            sort,
            any,
            full,
            max_chars,
            raw_fts,
            json,
            plain,
            query,
        } => recall_cmd(
            &config,
            since,
            until,
            limit,
            lane.into_lane(),
            sort.into_sort(),
            any,
            full,
            max_chars,
            raw_fts,
            json,
            plain,
            query,
        ),
        Command::Status { json } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            match ipc::request(&socket, &Request::Status).await? {
                Reply::Status(status) => {
                    if json {
                        println!("{}", serde_json::to_string(&status)?);
                    } else {
                        println!("{status}");
                    }
                    Ok(ExitCode::SUCCESS)
                }
                other => unexpected(other),
            }
        }
        Command::Last {
            copy,
            open,
            lane,
            json,
        } => last(&config, copy, open, lane.into_lane(), json).await,
        Command::Debug { what } => {
            let ok = match what {
                DebugCommand::Config => {
                    debug::config(&config)?;
                    true
                }
                DebugCommand::Instructions => {
                    debug::instructions(&config);
                    true
                }
                DebugCommand::Context => {
                    debug::context(&config).await;
                    true
                }
                DebugCommand::Prepare => {
                    debug::prepare(&config)?;
                    true
                }
                DebugCommand::Classify { text } => {
                    debug::classify(&config, &text_or_stdin(text).await?);
                    true
                }
                DebugCommand::Prompt {
                    profile,
                    files,
                    images,
                    text,
                } => {
                    let attach = resolve_attachments(files, images)?;
                    let prompt_text = if text.is_empty()
                        && !attach.is_empty()
                        && std::io::stdin().is_terminal()
                    {
                        String::new()
                    } else {
                        text_or_stdin(text).await?
                    };
                    debug::prompt(config, prompt_text, profile, attach).await?
                }
                DebugCommand::Present { text } => {
                    debug::present(&config, text_or_stdin(text).await?).await?;
                    true
                }
                DebugCommand::MockAgent { .. } => unreachable!("handled above"),
            };
            Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn unexpected(reply: Reply) -> Result<ExitCode> {
    match reply {
        Reply::Error { message } => eprintln!("hey-computer: {message}"),
        other => eprintln!("hey-computer: unexpected reply {other:?}"),
    }
    Ok(ExitCode::FAILURE)
}

async fn last(
    config: &Config,
    copy: bool,
    open: bool,
    lane: Option<store::Lane>,
    json: bool,
) -> Result<ExitCode> {
    let state_dir = config.state_dir()?;
    let Some(answer) = store::load_last_for_lane(&state_dir, lane)? else {
        eprintln!("hey-computer: no answer recorded yet");
        if copy || open {
            present::notify(
                &config.present,
                "No previous answer",
                &present::escape_markup("Ask the assistant something first."),
                Urgency::Normal,
                5_000,
                None,
            )
            .await;
        }
        return Ok(ExitCode::FAILURE);
    };

    if copy {
        present::copy_to_clipboard(&config.present, &answer.text).await?;
    } else if open {
        let answers = ipc::runtime_dir()?.join("hey-computer");
        present::open_answer(
            &config.present,
            &answers,
            answer.id,
            &answer.text,
            &answer.denied,
        )
        .await?;
    } else if json {
        println!("{}", serde_json::to_string(&answer)?);
    } else {
        println!("{}", answer.text);
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_attachments(files: Vec<PathBuf>, images: Vec<PathBuf>) -> Result<Vec<Attachment>> {
    let mut out = Vec::with_capacity(files.len() + images.len());
    for path in files {
        out.push(Attachment {
            kind: AttachmentKind::File,
            path: attachments::resolve_client_path(&path)?,
        });
    }
    for path in images {
        out.push(Attachment {
            kind: AttachmentKind::Image,
            path: attachments::resolve_client_path(&path)?,
        });
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn recall_cmd(
    config: &Config,
    since: Option<String>,
    until: Option<String>,
    limit: usize,
    lane: Option<store::Lane>,
    sort: store::RecallSort,
    match_any: bool,
    full: bool,
    max_chars: usize,
    raw_fts: bool,
    json: bool,
    plain: bool,
    words: Vec<String>,
) -> Result<ExitCode> {
    let now_ms = store::unix_timestamp_ms();
    let since_ms = since
        .as_deref()
        .map(|s| store::parse_time_spec(s, now_ms))
        .transpose()?;
    let until_ms = until
        .as_deref()
        .map(|s| store::parse_time_spec(s, now_ms))
        .transpose()?;
    let query = if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    };
    let state_dir = config.state_dir()?;
    let hits = store::recall(
        &state_dir,
        &store::RecallOptions {
            query,
            lane,
            since_ms,
            until_ms,
            limit,
            sort,
            match_any,
            raw_fts,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string(&hits)?);
    } else {
        println!(
            "{}",
            store::format_recall_output(&hits, full, max_chars, plain)
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn cancel_id(socket: &Path, id: Option<u64>) -> Result<ExitCode> {
    match ipc::request(socket, &Request::Cancel { id }).await? {
        Reply::Cancelled { id: Some(id) } => {
            println!("cancelled request {id}");
            Ok(ExitCode::SUCCESS)
        }
        Reply::Cancelled { id: None } => {
            println!("nothing to cancel");
            Ok(ExitCode::SUCCESS)
        }
        other => unexpected(other),
    }
}

async fn tasks(config: &Config, socket: &Path, action: Option<TasksCommand>) -> Result<ExitCode> {
    let action = action.unwrap_or(TasksCommand::List {
        lane: LaneFilter::Background,
        status: None,
        all: false,
        limit: 20,
        json: false,
    });
    match action {
        TasksCommand::Cancel { id } => cancel_id(socket, Some(id)).await,
        TasksCommand::List {
            lane,
            status,
            all,
            limit,
            json,
        } => {
            let parsed_status = match status.as_deref() {
                Some(s) => match store::TurnStatus::parse(s) {
                    Some(st) => Some(st),
                    None => anyhow::bail!(
                        "unknown status `{s}` (expected queued, running, completed, suppressed, failed, or cancelled)"
                    ),
                },
                None => None,
            };
            let state_dir = config.state_dir()?;
            let rows = store::list_turns(
                &state_dir,
                &store::ListFilter {
                    lane: lane.into_lane(),
                    status: parsed_status,
                    include_suppressed: all,
                    limit,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string(&rows)?);
                return Ok(ExitCode::SUCCESS);
            }
            if rows.is_empty() {
                println!("no tasks recorded");
                return Ok(ExitCode::SUCCESS);
            }
            for row in rows {
                let ts = store::format_local_timestamp(row.created_at_ms);
                let prof = if row.profile.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", row.profile)
                };
                let preview = row.request.replace('\n', " ");
                let preview = match preview.char_indices().nth(72) {
                    Some((i, _)) => format!("{}…", preview[..i].trim_end()),
                    None => preview,
                };
                println!(
                    "#{}  {ts}  {:<10}{prof}  {preview}",
                    row.id,
                    row.status.as_str()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        TasksCommand::Show { id, json } => {
            let state_dir = config.state_dir()?;
            let Some(row) = store::get_turn(&state_dir, id)? else {
                eprintln!("hey-computer: task {id} not found");
                return Ok(ExitCode::FAILURE);
            };
            if json {
                println!("{}", serde_json::to_string(&row)?);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "#{} [{} / {}]{}  {}",
                row.id,
                row.lane,
                row.status,
                if row.profile.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", row.profile)
                },
                store::format_local_timestamp(row.created_at_ms)
            );
            println!("Request: {}", row.request);
            if !row.answer.is_empty() {
                println!("\n{}", row.answer);
            } else if let Some(msg) = &row.error_message {
                println!("\nError: {msg}");
            } else if let Some(tok) = &row.suppressed_token {
                println!("\nSuppressed ({tok})");
            }
            Ok(ExitCode::SUCCESS)
        }
        TasksCommand::Open { id } => {
            let state_dir = config.state_dir()?;
            let Some(row) = store::get_turn(&state_dir, id)? else {
                eprintln!("hey-computer: task {id} not found");
                return Ok(ExitCode::FAILURE);
            };
            if row.answer.trim().is_empty() {
                eprintln!("hey-computer: task {id} has no answer to open");
                return Ok(ExitCode::FAILURE);
            }
            let answers = ipc::runtime_dir()?.join("hey-computer");
            present::open_answer(&config.present, &answers, row.id, &row.answer, &row.denied)
                .await?;
            Ok(ExitCode::SUCCESS)
        }
        TasksCommand::Copy { id } => {
            let state_dir = config.state_dir()?;
            let Some(row) = store::get_turn(&state_dir, id)? else {
                eprintln!("hey-computer: task {id} not found");
                return Ok(ExitCode::FAILURE);
            };
            if row.answer.trim().is_empty() {
                eprintln!("hey-computer: task {id} has no answer to copy");
                return Ok(ExitCode::FAILURE);
            }
            present::copy_to_clipboard(&config.present, &row.answer).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn notify_scheduler_reload(socket: &Path) {
    let _ = ipc::request(socket, &Request::ScheduleReload).await;
}

fn validate_schedule_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        anyhow::bail!("schedule name `{name}` must be 1..=64 ASCII letters, digits, '-' or '_'");
    }
    if name.bytes().all(|b| b.is_ascii_digit()) {
        anyhow::bail!("schedule name `{name}` cannot be purely numeric (ambiguous with job IDs)");
    }
    Ok(())
}

async fn schedule_cmd(
    config: &Config,
    socket: &Path,
    action: Option<ScheduleCommand>,
) -> Result<ExitCode> {
    let action = action.unwrap_or(ScheduleCommand::List {
        all: false,
        json: false,
    });
    match action {
        ScheduleCommand::List { all, json } => {
            let state_dir = config.state_dir()?;
            let jobs = store::list_schedules(&state_dir, all)?;
            if json {
                println!("{}", serde_json::to_string(&jobs)?);
                return Ok(ExitCode::SUCCESS);
            }
            if jobs.is_empty() {
                println!("no scheduled jobs");
                return Ok(ExitCode::SUCCESS);
            }
            for job in jobs {
                let state = if job.enabled { "active" } else { "paused" };
                let next = job
                    .next_run_at
                    .map(store::format_local_timestamp)
                    .unwrap_or_else(|| "-".to_string());
                let mode = if !job.script.is_empty() && job.no_agent {
                    "script+notify"
                } else if !job.script.is_empty() {
                    "script+agent"
                } else if job.no_agent {
                    "reminder"
                } else {
                    "agent"
                };
                println!(
                    "#{}  {:<20}  {:<6}  {:<13}  {:<16}  next: {}",
                    job.id, job.name, state, mode, job.schedule_spec, next
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Show { target, json } => {
            let state_dir = config.state_dir()?;
            let Some(job) = store::get_schedule(&state_dir, &target)? else {
                eprintln!("hey-computer: schedule `{target}` not found");
                return Ok(ExitCode::FAILURE);
            };
            if json {
                println!("{}", serde_json::to_string(&job)?);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "#{} {} [{}]",
                job.id,
                job.name,
                if job.enabled { "active" } else { "paused" }
            );
            println!("Schedule:        {}", job.schedule_spec);
            if let Some(next) = job.next_run_at {
                println!("Next run:        {}", store::format_local_timestamp(next));
            }
            if let Some(last) = job.last_run_at {
                println!(
                    "Last run:        {} ({})",
                    store::format_local_timestamp(last),
                    job.last_status.as_deref().unwrap_or("unknown")
                );
            }
            println!(
                "Runs:            {}{}",
                job.run_count,
                job.max_runs.map(|m| format!(" / {m}")).unwrap_or_default()
            );
            println!("Failure deliver: {}", job.failure_deliver);
            if !job.profile.is_empty() {
                println!("Profile:         {}", job.profile);
            }
            if job.no_agent {
                println!("No agent:        true");
            }
            if let Some(tok) = &job.suppress_token {
                println!("Suppress token:  {tok}");
            }
            if let Some(exp) = job.expires_at {
                println!("Expires:         {}", store::format_local_timestamp(exp));
            }
            if !job.script.is_empty() {
                println!("Script:          {}", job.script);
            }
            if !job.prompt.is_empty() {
                println!("Prompt:          {}", job.prompt);
            }
            if let Some(err) = &job.last_error {
                println!("Last error:      {err}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Add {
            name,
            schedule,
            prompt,
            profile,
            script,
            no_agent,
            suppress_token,
            failure_deliver,
            max_runs,
            expires,
        } => {
            validate_schedule_name(&name)?;
            let parsed = scheduler::ScheduleSpec::parse(&schedule)?;
            parsed.check_min_interval(config.scheduler.min_interval_secs)?;
            if let Some(prof) = profile.as_deref()
                && let Err(msg) = config.resolve_profile(Some(prof))
            {
                anyhow::bail!("{msg}");
            }
            let script_str = script.unwrap_or_default();
            scheduler::validate_script(&script_str, &config.permissions.deny)?;
            if prompt.trim().is_empty() && script_str.trim().is_empty() {
                anyhow::bail!("provide --prompt, --script, or both");
            }
            if let Some(tok) = &suppress_token
                && tok.trim().is_empty()
            {
                anyhow::bail!("--suppress-token must not be empty");
            }
            if max_runs == Some(0) {
                anyhow::bail!("--max-runs must be at least 1");
            }
            let now_ms = store::unix_timestamp_ms();
            let next_run_at = parsed.first_run_after(now_ms)?;
            let expires_at = expires
                .as_deref()
                .map(|s| scheduler::parse_expires_at(s, now_ms))
                .transpose()?;
            if let Some(exp) = expires_at
                && exp <= now_ms
            {
                anyhow::bail!("--expires time must be in the future");
            }
            let state_dir = config.state_dir()?;
            let created = store::create_schedule(
                &state_dir,
                &store::NewSchedule {
                    name,
                    schedule_spec: schedule.trim().to_string(),
                    prompt,
                    profile: profile.filter(|p| p != "default").unwrap_or_default(),
                    script: script_str,
                    no_agent,
                    suppress_token,
                    failure_deliver: failure_deliver.into_deliver(),
                    max_runs,
                    expires_at,
                    next_run_at,
                },
                config.scheduler.max_jobs,
            )?;
            notify_scheduler_reload(socket).await;
            println!(
                "scheduled #{} ({}) — next run: {}",
                created.id,
                created.name,
                created
                    .next_run_at
                    .map(store::format_local_timestamp)
                    .unwrap_or_else(|| "-".to_string())
            );
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Edit {
            target,
            schedule,
            prompt,
            profile,
            script,
            no_agent,
            suppress_token,
            failure_deliver,
            max_runs,
            expires,
        } => {
            let now_ms = store::unix_timestamp_ms();
            let mut patch = store::SchedulePatch::default();
            if let Some(spec_str) = schedule {
                let parsed = scheduler::ScheduleSpec::parse(&spec_str)?;
                parsed.check_min_interval(config.scheduler.min_interval_secs)?;
                let next = parsed.first_run_after(now_ms)?;
                patch.schedule_spec = Some(spec_str.trim().to_string());
                patch.next_run_at = Some(Some(next));
            }
            if let Some(prof) = profile {
                if !prof.is_empty()
                    && let Err(msg) = config.resolve_profile(Some(&prof))
                {
                    anyhow::bail!("{msg}");
                }
                patch.profile = Some(if prof == "default" {
                    String::new()
                } else {
                    prof
                });
            }
            if let Some(s) = script {
                scheduler::validate_script(&s, &config.permissions.deny)?;
                patch.script = Some(s);
            }
            patch.prompt = prompt;
            patch.no_agent = no_agent;
            if let Some(tok) = suppress_token {
                patch.suppress_token = Some((!tok.trim().is_empty()).then_some(tok));
            }
            if let Some(fd) = failure_deliver {
                patch.failure_deliver = Some(fd.into_deliver());
            }
            if let Some(mr) = max_runs {
                patch.max_runs = Some((mr > 0).then_some(mr));
            }
            if let Some(exp) = expires {
                if exp.trim().is_empty() {
                    patch.expires_at = Some(None);
                } else {
                    let ts = scheduler::parse_expires_at(&exp, now_ms)?;
                    patch.expires_at = Some(Some(ts));
                }
            }
            let state_dir = config.state_dir()?;
            let updated = store::update_schedule(&state_dir, &target, &patch)?;
            notify_scheduler_reload(socket).await;
            println!("updated schedule #{} ({})", updated.id, updated.name);
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Pause { target } => {
            let state_dir = config.state_dir()?;
            let job = store::set_schedule_enabled(&state_dir, &target, false, None)?;
            notify_scheduler_reload(socket).await;
            println!("paused schedule #{} ({})", job.id, job.name);
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Resume { target } => {
            let state_dir = config.state_dir()?;
            let Some(existing) = store::get_schedule(&state_dir, &target)? else {
                eprintln!("hey-computer: schedule `{target}` not found");
                return Ok(ExitCode::FAILURE);
            };
            let parsed = scheduler::ScheduleSpec::parse(&existing.schedule_spec)?;
            let next = parsed.first_run_after(store::unix_timestamp_ms())?;
            let job = store::set_schedule_enabled(&state_dir, &target, true, Some(next))?;
            notify_scheduler_reload(socket).await;
            println!(
                "resumed schedule #{} ({}) — next run: {}",
                job.id,
                job.name,
                store::format_local_timestamp(next)
            );
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Remove { target } => {
            let state_dir = config.state_dir()?;
            let Some(removed) = store::delete_schedule(&state_dir, &target)? else {
                eprintln!("hey-computer: schedule `{target}` not found");
                return Ok(ExitCode::FAILURE);
            };
            notify_scheduler_reload(socket).await;
            println!("removed schedule #{} ({})", removed.id, removed.name);
            Ok(ExitCode::SUCCESS)
        }
        ScheduleCommand::Run { target } => {
            match ipc::request(socket, &Request::ScheduleRun { target }).await? {
                Reply::Ok { message } => {
                    println!("{message}");
                    Ok(ExitCode::SUCCESS)
                }
                other => unexpected(other),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn ask(
    config: &Config,
    socket: &Path,
    words: Vec<String>,
    print: bool,
    new: bool,
    background: bool,
    suppress_token: Option<String>,
    profile: Option<String>,
    source: InputSource,
    files: Vec<PathBuf>,
    images: Vec<PathBuf>,
) -> Result<ExitCode> {
    if let Some(tok) = &suppress_token
        && tok.trim().is_empty()
    {
        eprintln!("hey-computer: --suppress-token must not be empty");
        return Ok(ExitCode::from(2));
    }
    // Every refusal after this point is also a notification (see below).
    let has_attachments = !files.is_empty() || !images.is_empty();
    let text = if words.is_empty() && has_attachments && std::io::stdin().is_terminal() {
        Ok(String::new())
    } else {
        text_or_stdin(words).await
    };
    if let Ok(text) = &text
        && text.trim().is_empty()
        && !has_attachments
    {
        eprintln!("hey-computer: the request is empty; nothing was asked");
        return Ok(ExitCode::from(2));
    }

    let accepted = match (text, resolve_attachments(files, images)) {
        (Ok(text), Ok(attachments)) => {
            let req = Request::Ask {
                text,
                wait: print,
                new,
                profile,
                background,
                suppress_token,
                attachments,
                source,
            };
            match request_size(&req) {
                Ok(()) => ipc::ask_request(socket, &req).await,
                Err(e) => Err(e),
            }
        }
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    let (id, mut client) = match accepted {
        Ok(accepted) => accepted,
        Err(e) => {
            // Usually started by a voice command, where nobody reads stderr;
            // with --print the caller reads it.
            if !print || e.downcast_ref::<NotRunning>().is_some() {
                let (summary, body) = if e.downcast_ref::<NotRunning>().is_some() {
                    (
                        "The assistant is not running",
                        "Start it with: systemctl --user start hey-computer".to_string(),
                    )
                } else {
                    ("The assistant did not take the request", format!("{e:#}"))
                };
                present::notify(
                    &config.present,
                    summary,
                    &present::escape_markup(&body),
                    Urgency::Normal,
                    8_000,
                    None,
                )
                .await;
            }
            return Err(e);
        }
    };
    tracing::debug!("request {id} accepted");
    if !print {
        if background {
            println!("{id}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    match client.next(None).await? {
        Reply::Answer {
            text,
            denied,
            suppressed,
            ..
        } => {
            if !suppressed {
                println!("{text}");
            }
            for command in denied {
                eprintln!("hey-computer: refused to run {command}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Reply::Failed { message, .. } => {
            eprintln!("hey-computer: {message}");
            Ok(ExitCode::FAILURE)
        }
        other => unexpected(other),
    }
}

/// Refuse a request the daemon would refuse, before connecting: its line,
/// once encoded (escaping makes it longer than the text), must fit.
fn request_size(request: &Request) -> Result<()> {
    let line = protocol::encode(request);
    if line.len() + 1 > MAX_REQUEST_LINE {
        anyhow::bail!(
            "the request is {} bytes, more than the {MAX_REQUEST_LINE} the daemon takes",
            line.len()
        );
    }
    Ok(())
}

/// The words joined with spaces, or all of stdin when there are none. Stdin
/// is read only as far as a request can go.
async fn text_or_stdin(words: Vec<String>) -> Result<String> {
    if !words.is_empty() {
        return Ok(words.join(" "));
    }
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading the request from stdin")?;
    if bytes.len() > MAX_REQUEST_LINE {
        anyhow::bail!(
            "the request on stdin is longer than the {MAX_REQUEST_LINE} bytes the daemon takes"
        );
    }
    String::from_utf8(bytes).context("the request on stdin is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_every_subcommand() {
        for args in [
            vec!["hey-computer", "daemon"],
            vec!["hey-computer", "ask", "what", "time", "is", "it"],
            vec!["hey-computer", "ask", "--print", "--new", "--", "-5 plus 3"],
            vec!["hey-computer", "ask", "-p", "deep", "derive", "this"],
            vec![
                "hey-computer",
                "ask",
                "-f",
                "a.txt",
                "-i",
                "b.png",
                "explain",
            ],
            vec!["hey-computer", "ask", "--image", "shot.png"],
            vec![
                "hey-computer",
                "ask",
                "-b",
                "--suppress-token",
                "HEARTBEAT_OK",
                "check",
            ],
            vec!["hey-computer", "ask", "--profile", "default", "hi"],
            vec!["hey-computer", "ask", "--print"],
            vec!["hey-computer", "--socket", "/tmp/s", "cancel"],
            vec!["hey-computer", "cancel", "--id", "7"],
            vec!["hey-computer", "tasks"],
            vec![
                "hey-computer",
                "tasks",
                "list",
                "--lane",
                "all",
                "--all",
                "--json",
            ],
            vec!["hey-computer", "tasks", "show", "3", "--json"],
            vec!["hey-computer", "tasks", "open", "3"],
            vec!["hey-computer", "tasks", "copy", "3"],
            vec!["hey-computer", "tasks", "cancel", "3"],
            vec!["hey-computer", "recall"],
            vec![
                "hey-computer",
                "recall",
                "--since",
                "7d",
                "--limit",
                "3",
                "nixos",
                "rebuild",
            ],
            vec!["hey-computer", "recall", "--json", "--any", "--full", "foo"],
            vec!["hey-computer", "status", "--json"],
            vec!["hey-computer", "last"],
            vec!["hey-computer", "last", "--lane", "all", "--json"],
            vec!["hey-computer", "last", "--copy"],
            vec!["hey-computer", "last", "--open"],
            vec!["hey-computer", "debug", "config"],
            vec!["hey-computer", "debug", "context"],
            vec!["hey-computer", "debug", "classify", "# heading"],
            vec!["hey-computer", "debug", "prompt", "hi"],
            vec![
                "hey-computer",
                "debug",
                "mock-agent",
                "s.json",
                "--log",
                "l",
            ],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn ask_keeps_words_after_the_separator_verbatim() {
        let cli = Cli::try_parse_from(["hey-computer", "ask", "--", "--new", "idea"]).unwrap();
        match cli.command {
            Command::Ask { new, text, .. } => {
                assert!(!new);
                assert_eq!(text, ["--new", "idea"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn last_rejects_copy_and_open_together() {
        assert!(Cli::try_parse_from(["hey-computer", "last", "--copy", "--open"]).is_err());
    }
}

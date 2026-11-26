//! hey-computer: a voice assistant daemon that keeps one ACP agent warm.
//!
//! One binary, two roles: `daemon` owns the agent and the socket, every other
//! subcommand is a client that talks to it over the socket and exits.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio::io::AsyncReadExt;

use hey_computer::attachments;
use hey_computer::config::Config;
use hey_computer::distill::{self, DistillOptions};
use hey_computer::ipc::{self, NotRunning};
use hey_computer::present::{self, Urgency};
use hey_computer::protocol::{
    self, Attachment, AttachmentKind, DraftAction, InputSource, MAX_REQUEST_LINE,
    NotifyReleaseAction, Reply, Request, SpeakAction,
};
use hey_computer::{daemon, debug, mcp, mock, scheduler, store};

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
        /// Continue the conversation of an earlier turn ID.
        #[arg(long, value_name = "ID", conflicts_with = "new")]
        reply_to: Option<u64>,
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
    /// Inspect, copy, edit, dismiss, or test-hold notification reply drafts.
    Draft {
        #[command(subcommand)]
        action: Option<DraftCommand>,
    },
    /// Browse recent conversation history and active tasks (fast read-only SQLite feed).
    History {
        /// Maximum number of history items to return (1..=200).
        #[arg(short = 'n', long, default_value_t = 60)]
        limit: usize,
        /// Restrict history items to one lane (`all` by default).
        #[arg(long, value_enum, default_value_t = LaneFilter::All)]
        lane: LaneFilter,
        /// Show individual turns instead of collapsing multi-turn conversations.
        #[arg(long)]
        no_threads: bool,
        /// Include failed and cancelled turns alongside completed answers.
        #[arg(long)]
        include_failed: bool,
        /// Include suppressed heartbeat turns (implies `--include-failed`).
        #[arg(long)]
        all: bool,
        /// Only include turns created at or after this time (`24h`, `7d`, `2w`,
        /// `today`, `yesterday`, or `YYYY-MM-DD`).
        #[arg(long, value_name = "SPEC")]
        since: Option<String>,
        /// Emit structured JSON (`{now_ms, query, active_tasks, items}`).
        #[arg(long)]
        json: bool,
        /// Optional full-text search query (uses SQLite FTS5).
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        query: Vec<String>,
    },
    /// Search past completed answers using the local SQLite FTS5 index.
    Recall {
        /// Only include turns created at or after this time (`24h`, `7d`, `2w`,
        /// `today`, `yesterday`, or `YYYY-MM-DD`).
        #[arg(long, value_name = "SPEC", conflicts_with_all = ["on", "id", "thread"])]
        since: Option<String>,
        /// Only include turns created before this time (`24h`, `7d`, `today`,
        /// `yesterday`, or `YYYY-MM-DD`; calendar dates are inclusive of that entire day).
        #[arg(long, value_name = "SPEC", conflicts_with_all = ["on", "id", "thread"])]
        until: Option<String>,
        /// Only include turns created on a single local calendar day (`today`,
        /// `yesterday`, or `YYYY-MM-DD`).
        #[arg(long, value_name = "DATE", conflicts_with_all = ["since", "until", "id", "thread"])]
        on: Option<String>,
        /// Exclude automated scheduled, pre-collector, and distill turns.
        #[arg(long)]
        user_only: bool,
        /// Fetch a single turn by numeric ID (sanitized inside `<untrusted_recall>`).
        #[arg(long, value_name = "ID", conflicts_with_all = ["thread", "on", "since", "until"])]
        id: Option<u64>,
        /// Fetch the full chronological conversation thread for a turn ID
        /// (sanitized inside `<untrusted_recall>`).
        #[arg(long, value_name = "ID", conflicts_with_all = ["id", "on", "since", "until"])]
        thread: Option<u64>,
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
        /// Maximum characters per field when `--full`, `--id`, or `--thread` is set.
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
    /// Review recent user turns and propose durable memory updates or draft skills.
    Distill {
        /// Only review user turns created at or after this time (`7d`, `2w`, `24h`,
        /// `today`, `yesterday`, `YYYY-MM-DD`, or `all`).
        #[arg(long, value_name = "SPEC", default_value = distill::DEFAULT_DISTILL_SINCE)]
        since: String,
        /// Maximum number of recent user turns to inspect (1..=500).
        #[arg(short = 'n', long, default_value_t = distill::DEFAULT_DISTILL_LIMIT)]
        limit: usize,
        /// Named profile from `[profiles.<name>]` (defaults to `deep` when configured).
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
        /// Queue distillation in the background lane and print its task ID.
        #[arg(short = 'b', long, conflicts_with_all = ["print", "json"])]
        background: bool,
        /// Wait and print the prose summary and plain-text diff instead of opening the viewer.
        #[arg(long, conflicts_with = "json")]
        print: bool,
        /// Wait and print the full `Reply::Answer` (`text` + `widgets`) as JSON.
        #[arg(long, conflicts_with = "print")]
        json: bool,
    },
    /// Query or toggle spoken answers (`present.speak`).
    Speak {
        /// Control action (`on`, `off`, `toggle`, or `status`; defaults to `status`).
        #[arg(value_enum)]
        action: Option<SpeakCliAction>,
        /// Emit speak status as JSON (`{"enabled":true,"speaking":false}`).
        #[arg(long)]
        json: bool,
    },
    /// Open the native answer viewer (`hey-computer-viewer`).
    View {
        /// Optional Markdown file to render, or numeric turn ID (`42` / `#42`)
        /// when no file with that name exists.
        #[arg(value_name = "TARGET")]
        target: Option<String>,
        /// Conversation turn ID to load from `history.db`.
        #[arg(long, value_name = "ID", conflicts_with_all = ["schedule", "schedules"])]
        turn_id: Option<u64>,
        /// Open the Schedule Inspector for one scheduled job (numeric ID or name).
        #[arg(long, value_name = "TARGET", conflicts_with_all = ["schedules", "turn_id", "history", "recall"])]
        schedule: Option<String>,
        /// Open the Scheduled Actions overview in the viewer.
        #[arg(long, conflicts_with_all = ["schedule", "turn_id", "history", "recall"])]
        schedules: bool,
        /// Open the viewer with the History drawer expanded.
        #[arg(long, conflicts_with_all = ["schedule", "schedules", "recall"])]
        history: bool,
        /// Open the viewer with the History drawer pre-filtered by this recall query.
        #[arg(long, value_name = "QUERY", conflicts_with_all = ["schedule", "schedules", "history"])]
        recall: Option<String>,
        /// Focus the bottom follow-up composer on startup.
        #[arg(long)]
        focus_composer: bool,
        /// Spawn the viewer detached and return immediately (auto-enabled inside
        /// agent turns when `HEY_COMPUTER_AGENT_TURN=1`).
        #[arg(short = 'd', long, conflicts_with = "foreground")]
        detach: bool,
        /// Force foreground exec even when `HEY_COMPUTER_AGENT_TURN=1` is set.
        #[arg(long, hide = true, conflicts_with = "detach")]
        foreground: bool,
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
    /// Internal stdio MCP server exposing structured UI widget tools.
    #[command(hide = true, name = "mcp-ui")]
    McpUi,
    /// Check one part of the program on its own.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        what: DebugCommand,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SpeakCliAction {
    On,
    Off,
    Toggle,
    Status,
}

impl SpeakCliAction {
    fn into_action(self) -> SpeakAction {
        match self {
            Self::On => SpeakAction::On,
            Self::Off => SpeakAction::Off,
            Self::Toggle => SpeakAction::Toggle,
            Self::Status => SpeakAction::Status,
        }
    }
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
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "true",
            value_name = "BOOL"
        )]
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
    /// Open one schedule inspector or the all-schedules overview in `hey-computer-viewer`.
    #[command(visible_alias = "view")]
    Open {
        /// Optional numeric ID or schedule name (omit to open the overview).
        target: Option<String>,
        /// Spawn the viewer detached and return immediately.
        #[arg(short = 'd', long)]
        detach: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LaneFilter {
    Interactive,
    Background,
    Restricted,
    All,
}

impl LaneFilter {
    fn into_lane(self) -> Option<store::Lane> {
        match self {
            Self::Interactive => Some(store::Lane::Interactive),
            Self::Background => Some(store::Lane::Background),
            Self::Restricted => Some(store::Lane::Restricted),
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
enum DraftCommand {
    /// List persisted notification draft checkpoints (pending by default; works offline).
    List {
        /// Include consumed, dismissed, and expired drafts.
        #[arg(long)]
        all: bool,
        /// Maximum drafts to list.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show full details of one draft checkpoint by token or `#id` / `id` (works offline).
    Show {
        target: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Claim a draft and copy its text to the clipboard (falls back offline when daemon is stopped).
    Copy { target: String },
    /// Claim a draft, copy its text, and open it in the viewer composer (falls back offline).
    Edit { target: String },
    /// Dismiss a pending draft checkpoint (falls back offline when daemon is stopped).
    Dismiss { target: String },
    /// Simulate a SwayNC `notify_hold` round-trip against the running daemon.
    Hold {
        /// Desktop app name (e.g. `Signal`, `whatsapp-electron`, `Slack`).
        #[arg(long)]
        app: String,
        /// Optional desktop entry ID.
        #[arg(long, default_value = "")]
        desktop_entry: String,
        /// Notification summary / sender name.
        #[arg(long)]
        sender: String,
        /// Notification message body.
        #[arg(long)]
        body: String,
        /// Optional notification category (e.g. `im.received`).
        #[arg(long, default_value = "")]
        category: String,
        /// Simulate Do Not Disturb active.
        #[arg(long)]
        dnd: bool,
        /// Output raw JSON replies.
        #[arg(long)]
        json: bool,
    },
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
    // Internal MCP server and mock ACP agent own stdio; never initialize tracing
    // before handling `mcp-ui` or `mock-agent`.
    if matches!(cli.command, Command::McpUi) {
        return match tokio::task::block_in_place(mcp::run_stdio_ui_server) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("hey-computer mcp-ui: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
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
    if matches!(cli.command, Command::McpUi) {
        tokio::task::block_in_place(mcp::run_stdio_ui_server)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Command::Debug {
        what: DebugCommand::MockAgent { script, log },
    } = &cli.command
    {
        mock::run(script, log.clone()).await?;
        return Ok(ExitCode::SUCCESS);
    }

    // Pure socket client subcommands (and `view` exec) do not read config.toml
    // first so a long-running front end or Waybar watcher survives config
    // additions across rebuilds.
    match &cli.command {
        Command::Cancel { id } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            return cancel_id(&socket, *id).await;
        }
        Command::Status { json } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            return match ipc::request(&socket, &Request::Status).await? {
                Reply::Status(status) => {
                    if *json {
                        println!("{}", serde_json::to_string(&status)?);
                    } else {
                        println!("{status}");
                    }
                    Ok(ExitCode::SUCCESS)
                }
                other => unexpected(other),
            };
        }
        Command::Speak { action, json } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            let action = action.unwrap_or(SpeakCliAction::Status).into_action();
            return speak_cmd(cli.config.as_deref(), &socket, action, *json).await;
        }
        Command::View {
            target,
            turn_id,
            schedule,
            schedules,
            history,
            recall,
            focus_composer,
            detach,
            foreground,
        } => {
            return view_cmd(
                cli.config.as_deref(),
                cli.socket.as_deref(),
                target.as_deref(),
                *turn_id,
                schedule.as_deref(),
                *schedules,
                *history,
                recall.as_deref(),
                *focus_composer,
                *detach,
                *foreground,
            )
            .await;
        }
        _ => {}
    }

    let config_path = cli.config.clone();
    let config = match (&cli.command, cli.config.as_deref()) {
        (
            Command::Ask { .. }
            | Command::Schedule {
                action: None | Some(ScheduleCommand::List { .. } | ScheduleCommand::Show { .. }),
            }
            | Command::Tasks {
                action: None | Some(TasksCommand::List { .. } | TasksCommand::Show { .. }),
            }
            | Command::Recall { .. }
            | Command::Draft {
                action: None | Some(DraftCommand::List { .. } | DraftCommand::Show { .. }),
            },
            None,
        ) => match Config::load(None) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::debug!("falling back to default client config: {e:#}");
                Config::default()
            }
        },
        _ => Config::load(cli.config.as_deref())?,
    };

    match cli.command {
        Command::Daemon => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            daemon::run(config, &socket).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ask {
            print,
            new,
            reply_to,
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
                reply_to,
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
            schedule_cmd(
                &config,
                cli.config.as_deref(),
                cli.socket.as_deref(),
                &socket,
                action,
            )
            .await
        }
        Command::Draft { action } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            draft_cmd(&config, &socket, action).await
        }
        Command::History {
            limit,
            lane,
            no_threads,
            include_failed,
            all,
            since,
            json,
            query,
        } => history_cmd(
            &config,
            limit,
            lane.into_lane(),
            no_threads,
            include_failed,
            all,
            since,
            json,
            query,
        ),
        Command::Recall {
            since,
            until,
            on,
            user_only,
            id,
            thread,
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
            on,
            user_only,
            id,
            thread,
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
        Command::Distill {
            since,
            limit,
            profile,
            background,
            print,
            json,
        } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            distill::run(
                &config,
                &socket,
                &DistillOptions {
                    since,
                    limit,
                    profile,
                    background,
                    print,
                    json,
                    config_path,
                },
            )
            .await
        }
        Command::Speak { .. } | Command::View { .. } | Command::McpUi => {
            unreachable!("handled above")
        }
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

fn print_speak_status(enabled: bool, speaking: bool, as_json: bool) -> Result<()> {
    if as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "enabled": enabled,
                "speaking": speaking,
            }))?
        );
    } else {
        println!("{}", if enabled { "on" } else { "off" });
    }
    Ok(())
}

/// Apply a `SpeakAction` offline directly against `<state_dir>/speak_enabled`.
fn apply_offline_speak_action(
    state_dir: &Path,
    default_enabled: bool,
    action: SpeakAction,
) -> Result<bool> {
    let current = store::load_speak_state(state_dir, default_enabled);
    match action {
        SpeakAction::Status => Ok(current),
        SpeakAction::On => {
            store::save_speak_state(state_dir, true)?;
            Ok(true)
        }
        SpeakAction::Off => {
            store::save_speak_state(state_dir, false)?;
            Ok(false)
        }
        SpeakAction::Toggle => {
            let next = !current;
            store::save_speak_state(state_dir, next)?;
            Ok(next)
        }
    }
}

async fn speak_cmd(
    config_path: Option<&Path>,
    socket: &Path,
    action: SpeakAction,
    as_json: bool,
) -> Result<ExitCode> {
    match ipc::request(socket, &Request::Speak { action }).await {
        Ok(Reply::SpeakStatus { enabled, speaking }) => {
            print_speak_status(enabled, speaking, as_json)?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => unexpected(other),
        Err(e) if e.downcast_ref::<NotRunning>().is_some() => {
            let config = match config_path {
                Some(path) => Config::load(Some(path))?,
                None => Config::load(None).unwrap_or_default(),
            };
            let state_dir = config.state_dir()?;
            let enabled =
                apply_offline_speak_action(&state_dir, config.present.speak.enabled, action)?;
            print_speak_status(enabled, false, as_json)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug, Default)]
struct ViewerLaunchArgs<'a> {
    config_path: Option<&'a Path>,
    socket_path: Option<&'a Path>,
    file: Option<&'a Path>,
    turn_id: Option<u64>,
    schedule: Option<&'a str>,
    schedules: bool,
    history: bool,
    recall: Option<&'a str>,
    focus_composer: bool,
}

fn build_viewer_args(opts: &ViewerLaunchArgs<'_>) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(cfg) = opts.config_path {
        args.push(OsString::from("--config"));
        args.push(cfg.as_os_str().to_os_string());
    }
    if let Some(sock) = opts.socket_path {
        args.push(OsString::from("--socket"));
        args.push(sock.as_os_str().to_os_string());
    }
    if let Some(id) = opts.turn_id {
        args.push(OsString::from("--turn-id"));
        args.push(OsString::from(id.to_string()));
    }
    if let Some(target) = opts.schedule {
        args.push(OsString::from("--schedule"));
        args.push(OsString::from(target));
    }
    if opts.schedules {
        args.push(OsString::from("--schedules"));
    }
    if opts.history {
        args.push(OsString::from("--history"));
    }
    if let Some(q) = opts.recall {
        args.push(OsString::from("--recall"));
        args.push(OsString::from(q));
    }
    if opts.focus_composer {
        args.push(OsString::from("--focus-composer"));
    }
    if let Some(file) = opts.file {
        if file.to_string_lossy().starts_with('-') {
            args.push(OsString::from("--"));
        }
        args.push(file.as_os_str().to_os_string());
    }
    args
}

/// Interpret a positional `view` target: numeric `<ID>` or `#<ID>` becomes
/// `turn_id` when no file with that exact path exists on disk.
fn resolve_view_target(
    target: Option<&str>,
    explicit_turn_id: Option<u64>,
    has_other_mode: bool,
) -> (Option<PathBuf>, Option<u64>) {
    let Some(raw) = target else {
        return (None, explicit_turn_id);
    };
    let path = Path::new(raw);
    if explicit_turn_id.is_none() && !has_other_mode && !path.exists() {
        let stripped = raw.strip_prefix('#').unwrap_or(raw);
        if !stripped.is_empty()
            && stripped.bytes().all(|b| b.is_ascii_digit())
            && let Ok(parsed) = stripped.parse::<u64>()
        {
            return (None, Some(parsed));
        }
    }
    (Some(path.to_path_buf()), explicit_turn_id)
}

fn should_detach_viewer(detach: bool, foreground: bool) -> bool {
    detach
        || (!foreground
            && std::env::var("HEY_COMPUTER_AGENT_TURN")
                .ok()
                .as_deref()
                .is_some_and(|v| v == "1"))
}

fn exec_viewer(opts: &ViewerLaunchArgs<'_>) -> Result<ExitCode> {
    let viewer_bin = distill::resolve_viewer_binary();
    let args = build_viewer_args(opts);
    let err = std::process::Command::new(&viewer_bin)
        .args(&args)
        .env_remove("HEY_COMPUTER_AGENT_TURN")
        .exec();
    Err(err).with_context(|| format!("launching {}", viewer_bin.display()))
}

#[allow(clippy::too_many_arguments)]
async fn view_cmd(
    config_path: Option<&Path>,
    socket_path: Option<&Path>,
    target: Option<&str>,
    turn_id: Option<u64>,
    schedule: Option<&str>,
    schedules: bool,
    history: bool,
    recall: Option<&str>,
    focus_composer: bool,
    detach: bool,
    foreground: bool,
) -> Result<ExitCode> {
    let has_other_mode = schedule.is_some() || schedules || history || recall.is_some();
    let (file, resolved_turn_id) = resolve_view_target(target, turn_id, has_other_mode);
    let launch_args = ViewerLaunchArgs {
        config_path,
        socket_path,
        file: file.as_deref(),
        turn_id: resolved_turn_id,
        schedule,
        schedules,
        history,
        recall,
        focus_composer,
    };

    if !should_detach_viewer(detach, foreground) {
        return exec_viewer(&launch_args);
    }

    let config = match config_path {
        Some(path) => Config::load(Some(path))?,
        None => Config::load(None).unwrap_or_default(),
    };
    let state_dir = config.state_dir()?;

    let mut fallback_turn_owned: Option<(u64, String, Vec<String>)> = None;
    if let Some(id) = resolved_turn_id {
        let Some(turn) = store::get_turn(&state_dir, id)? else {
            eprintln!("hey-computer: turn #{id} not found");
            return Ok(ExitCode::FAILURE);
        };
        if turn.answer.trim().is_empty() && turn.widgets.is_empty() {
            eprintln!("hey-computer: turn #{id} has no answer or widgets to open");
            return Ok(ExitCode::FAILURE);
        }
        fallback_turn_owned = Some((turn.id, turn.answer, turn.denied));
    }
    if let Some(sched_target) = schedule
        && store::get_schedule(&state_dir, sched_target)?.is_none()
    {
        eprintln!("hey-computer: schedule `{sched_target}` not found");
        return Ok(ExitCode::FAILURE);
    }
    if let Some(path) = file.as_deref()
        && !path.exists()
    {
        eprintln!("hey-computer: file {} does not exist", path.display());
        return Ok(ExitCode::FAILURE);
    }

    let viewer_args = build_viewer_args(&launch_args);
    let fallback_ref = fallback_turn_owned
        .as_ref()
        .map(|(id, ans, denied)| (*id, ans.as_str(), denied.as_slice()));
    present::open_viewer_detached(&config.present, &viewer_args, fallback_ref).await?;

    if let Some(id) = resolved_turn_id {
        println!("opened turn #{id} in viewer");
    } else if let Some(sched) = schedule {
        println!("opened schedule `{sched}` in viewer");
    } else if schedules {
        println!("opened schedules overview in viewer");
    } else if let Some(path) = file.as_deref() {
        println!("opened {} in viewer", path.display());
    } else {
        println!("opened viewer");
    }
    Ok(ExitCode::SUCCESS)
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
        let viewer_cfg = present::viewer_config_with_fallback(&config.present, answer.id);
        present::open_answer(
            &viewer_cfg,
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
fn history_cmd(
    config: &Config,
    limit: usize,
    lane: Option<store::Lane>,
    no_threads: bool,
    include_failed: bool,
    all: bool,
    since: Option<String>,
    json: bool,
    words: Vec<String>,
) -> Result<ExitCode> {
    let now_ms = store::unix_timestamp_ms();
    let since_ms = since
        .as_deref()
        .map(|s| store::parse_time_spec(s, now_ms))
        .transpose()?;
    let query = if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    };
    let state_dir = config.state_dir()?;
    let feed = store::history_feed(
        &state_dir,
        &store::HistoryFeedOptions {
            query,
            lane,
            since_ms,
            limit,
            no_threads,
            include_failed: include_failed || all,
            include_suppressed: all,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string(&feed)?);
        return Ok(ExitCode::SUCCESS);
    }
    if !feed.active_tasks.is_empty() {
        println!("Active tasks:");
        for task in &feed.active_tasks {
            let prof = if task.profile.is_empty() {
                String::new()
            } else {
                format!(" · {}", task.profile)
            };
            println!(
                "  #{}  [{} {}s{prof}]  {}",
                task.id,
                task.status.as_str(),
                task.elapsed_secs,
                task.title
            );
        }
    }
    if feed.items.is_empty() {
        if feed.active_tasks.is_empty() {
            println!("no history recorded");
        }
        return Ok(ExitCode::SUCCESS);
    }
    for item in &feed.items {
        let turns_suffix = if item.turn_count > 1 {
            format!(" ({} turns)", item.turn_count)
        } else {
            String::new()
        };
        println!(
            "#{}  {:<6}  {:<10}  {}{}",
            item.id,
            item.relative_time,
            item.origin.as_str(),
            item.title,
            turns_suffix
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn recall_cmd(
    config: &Config,
    since: Option<String>,
    until: Option<String>,
    on: Option<String>,
    user_only: bool,
    id: Option<u64>,
    thread: Option<u64>,
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
    let state_dir = config.state_dir()?;
    if let Some(target_id) = id.or(thread) {
        let full_thread = thread.is_some();
        if json {
            if full_thread {
                let mut turns = store::conversation_turns(&state_dir, target_id, true, 0)?;
                if turns.is_empty()
                    && let Some(single) = store::get_turn(&state_dir, target_id)?
                {
                    turns.push(single);
                }
                if turns.is_empty() {
                    eprintln!("hey-computer: turn #{target_id} not found");
                    return Ok(ExitCode::FAILURE);
                }
                println!("{}", serde_json::to_string(&turns)?);
            } else {
                let Some(turn) = store::get_turn(&state_dir, target_id)? else {
                    eprintln!("hey-computer: turn #{target_id} not found");
                    return Ok(ExitCode::FAILURE);
                };
                println!("{}", serde_json::to_string(&turn)?);
            }
            return Ok(ExitCode::SUCCESS);
        }
        let Some(formatted) =
            store::format_recall_thread(&state_dir, target_id, full_thread, max_chars, plain)?
        else {
            eprintln!("hey-computer: turn #{target_id} not found");
            return Ok(ExitCode::FAILURE);
        };
        println!("{formatted}");
        return Ok(ExitCode::SUCCESS);
    }

    let now_ms = store::unix_timestamp_ms();
    let (since_ms, until_ms) = if let Some(day_spec) = on.as_deref() {
        let (start, end) = store::parse_day_range(day_spec, now_ms)?;
        (Some(start), Some(end))
    } else {
        (
            since
                .as_deref()
                .map(|s| store::parse_time_spec(s, now_ms))
                .transpose()?,
            until
                .as_deref()
                .map(|s| store::parse_until_time_spec(s, now_ms))
                .transpose()?,
        )
    };
    let query = if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    };
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
            user_only,
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

async fn draft_cmd(
    config: &Config,
    socket: &Path,
    action: Option<DraftCommand>,
) -> Result<ExitCode> {
    let action = action.unwrap_or(DraftCommand::List {
        all: false,
        limit: 20,
        json: false,
    });
    match action {
        DraftCommand::List { all, limit, json } => {
            let state_dir = config.state_dir()?;
            let rows = store::list_draft_checkpoints(&state_dir, all, limit)?;
            if json {
                println!("{}", serde_json::to_string(&rows)?);
                return Ok(ExitCode::SUCCESS);
            }
            if rows.is_empty() {
                println!("no drafts");
                return Ok(ExitCode::SUCCESS);
            }
            for row in rows {
                let preview = if !row.draft_text.trim().is_empty() {
                    row.draft_text.lines().next().unwrap_or("").trim()
                } else {
                    row.context_note.lines().next().unwrap_or("").trim()
                };
                println!(
                    "#{}  {:<9}  {:<10}  {}  {}  [{}]",
                    row.id,
                    row.status.as_str(),
                    row.channel,
                    row.sender,
                    preview,
                    row.token,
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        DraftCommand::Show { target, json } => {
            let state_dir = config.state_dir()?;
            let Some(row) = store::get_draft_checkpoint(&state_dir, &target)? else {
                eprintln!("hey-computer: draft `{target}` not found");
                return Ok(ExitCode::FAILURE);
            };
            if json {
                println!("{}", serde_json::to_string(&row)?);
                return Ok(ExitCode::SUCCESS);
            }
            println!("id: #{}", row.id);
            println!("token: {}", row.token);
            println!("status: {}", row.status);
            println!("channel: {} ({})", row.channel, row.app_name);
            println!("sender: {}", row.sender);
            if let Some(tid) = row.turn_id {
                println!("turn_id: #{tid}");
            }
            if !row.context_note.trim().is_empty() {
                println!("context: {}", row.context_note);
            }
            if !row.draft_text.trim().is_empty() {
                println!("\n{}", row.draft_text);
            }
            Ok(ExitCode::SUCCESS)
        }
        DraftCommand::Copy { target } => {
            draft_resume_cli(config, socket, target, DraftAction::Copy).await
        }
        DraftCommand::Edit { target } => {
            draft_resume_cli(config, socket, target, DraftAction::Edit).await
        }
        DraftCommand::Dismiss { target } => {
            draft_resume_cli(config, socket, target, DraftAction::Dismiss).await
        }
        DraftCommand::Hold {
            app,
            desktop_entry,
            sender,
            body,
            category,
            dnd,
            json,
        } => {
            let req = Request::NotifyHold {
                noti_id: 1,
                replaces_id: 0,
                app_name: app,
                desktop_entry,
                summary: sender,
                body,
                category,
                urgency: "normal".to_string(),
                has_default_action: true,
                dnd,
            };
            let mut client = ipc::Client::send(socket, &req).await?;
            let max_wait = std::time::Duration::from_secs_f64(
                config.notifications.max_hold_secs.clamp(5.0, 300.0) + 5.0,
            );
            loop {
                let reply = client.next(Some(max_wait)).await?;
                if json {
                    println!("{}", protocol::encode(&reply));
                }
                match reply {
                    Reply::NotifyAccepted { max_hold_ms } => {
                        if !json {
                            eprintln!("accepted (max_hold_ms={max_hold_ms})");
                        }
                    }
                    Reply::NotifyRelease {
                        action,
                        turn_id,
                        token,
                        context_note,
                        draft_text,
                        readonly,
                        reason,
                    } => {
                        if !json {
                            match action {
                                NotifyReleaseAction::ShowRaw => {
                                    println!(
                                        "show_raw{}",
                                        reason
                                            .as_deref()
                                            .map(|r| format!(" ({r})"))
                                            .unwrap_or_default()
                                    );
                                }
                                NotifyReleaseAction::ShowEnriched => {
                                    println!(
                                        "show_enriched turn={:?} token={} readonly={}",
                                        turn_id,
                                        token.as_deref().unwrap_or(""),
                                        readonly.unwrap_or(true),
                                    );
                                    if let Some(ctx) = context_note {
                                        println!("context: {ctx}");
                                    }
                                    if let Some(draft) = draft_text {
                                        println!("draft: {draft}");
                                    }
                                }
                            }
                        }
                        return Ok(ExitCode::SUCCESS);
                    }
                    other => return unexpected(other),
                }
            }
        }
    }
}

async fn draft_resume_cli(
    config: &Config,
    socket: &Path,
    target: String,
    action: DraftAction,
) -> Result<ExitCode> {
    match ipc::request(
        socket,
        &Request::DraftResume {
            token: target.clone(),
            action,
        },
    )
    .await
    {
        Ok(Reply::DraftResumed {
            id,
            status,
            message,
            ..
        }) => {
            if message.is_empty() {
                println!("draft #{id}: {status}");
            } else {
                println!("draft #{id} ({status}): {message}");
            }
            Ok(if status == "expired" {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Ok(other) => unexpected(other),
        Err(e) if e.downcast_ref::<NotRunning>().is_some() => {
            let state_dir = config.state_dir()?;
            let now_ms = store::unix_timestamp_ms();
            match store::claim_draft_checkpoint(&state_dir, &target, action, now_ms)? {
                store::CheckpointClaim::NotFound => {
                    eprintln!("hey-computer: draft `{target}` not found");
                    Ok(ExitCode::FAILURE)
                }
                store::CheckpointClaim::Expired(cp) => {
                    eprintln!("hey-computer: draft #{} expired", cp.id);
                    Ok(ExitCode::FAILURE)
                }
                store::CheckpointClaim::AlreadyUsed(cp) => {
                    if action == DraftAction::Edit && cp.status != store::DraftStatus::Dismissed {
                        let _ =
                            present::open_draft_editor(&config.present, cp.turn_id, &cp.draft_text)
                                .await;
                    }
                    println!("draft #{} (already_used): {}", cp.id, cp.status);
                    Ok(ExitCode::SUCCESS)
                }
                store::CheckpointClaim::Claimed(cp) => {
                    match action {
                        DraftAction::Copy | DraftAction::Paste => {
                            if !cp.draft_text.is_empty() {
                                present::copy_to_clipboard(&config.present, &cp.draft_text).await?;
                            }
                            println!("copied draft #{} to clipboard", cp.id);
                        }
                        DraftAction::Edit => {
                            if !cp.draft_text.is_empty() && !config.present.clipboard.is_empty() {
                                let _ = present::copy_to_clipboard(&config.present, &cp.draft_text)
                                    .await;
                            }
                            present::open_draft_editor(&config.present, cp.turn_id, &cp.draft_text)
                                .await?;
                            println!("opened draft #{} in viewer", cp.id);
                        }
                        DraftAction::Dismiss => {
                            println!("dismissed draft #{}", cp.id);
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        Err(e) => Err(e),
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
            if row.answer.trim().is_empty() && row.widgets.is_empty() {
                eprintln!("hey-computer: task {id} has no answer to open");
                return Ok(ExitCode::FAILURE);
            }
            let answers = ipc::runtime_dir()?.join("hey-computer");
            let viewer_cfg = present::viewer_config_with_fallback(&config.present, row.id);
            present::open_answer(&viewer_cfg, &answers, row.id, &row.answer, &row.denied).await?;
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

fn schedule_mode(job: &store::ScheduleRecord) -> &'static str {
    if !job.script.is_empty() && job.no_agent {
        "script+notify"
    } else if !job.script.is_empty() {
        "script+agent"
    } else if job.no_agent {
        "reminder"
    } else {
        "agent"
    }
}

#[derive(serde::Serialize)]
struct ScheduleTurnSummary {
    id: u64,
    status: store::TurnStatus,
    created_at_ms: i64,
    created_local: String,
    relative_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    profile: String,
    has_answer: bool,
    widget_count: usize,
    answer_preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suppressed_token: Option<String>,
}

#[derive(serde::Serialize)]
struct ScheduleListEntry {
    #[serde(flatten)]
    record: store::ScheduleRecord,
    title: String,
    human_schedule: String,
    mode: &'static str,
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_run_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_run_relative: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run_relative: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_turn_id: Option<u64>,
}

#[derive(serde::Serialize)]
struct ScheduleShowEntry {
    #[serde(flatten)]
    entry: ScheduleListEntry,
    recent_turns: Vec<ScheduleTurnSummary>,
}

async fn query_running_schedules(socket: &Path) -> Vec<u64> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        ipc::request(socket, &Request::Status),
    )
    .await
    {
        Ok(Ok(Reply::Status(status))) => status.running_schedules,
        _ => Vec::new(),
    }
}

fn enrich_schedule(
    state_dir: &Path,
    job: store::ScheduleRecord,
    running_ids: &[u64],
    now_ms: i64,
    recent_limit: usize,
) -> (ScheduleListEntry, Vec<ScheduleTurnSummary>) {
    let turns =
        store::list_turns_for_schedule_job(state_dir, &job, recent_limit).unwrap_or_default();
    let latest_turn_id = turns
        .iter()
        .find(|t| {
            t.status == store::TurnStatus::Completed
                && (!t.answer.trim().is_empty() || !t.widgets.is_empty())
        })
        .map(|t| t.id);
    let active_turn_id = turns
        .iter()
        .find(|t| {
            matches!(
                t.status,
                store::TurnStatus::Running | store::TurnStatus::Queued
            )
        })
        .map(|t| t.id);
    let running = running_ids.contains(&job.id) || active_turn_id.is_some();
    let title = scheduler::humanize_schedule_name(&job.name);
    let human_schedule = scheduler::humanize_schedule_spec(&job.schedule_spec);
    let mode = schedule_mode(&job);
    let next_run_local = job.next_run_at.map(store::format_local_timestamp);
    let next_run_relative = job
        .next_run_at
        .map(|ts| store::format_relative_time(ts, now_ms));
    let last_run_local = job.last_run_at.map(store::format_local_timestamp);
    let last_run_relative = job
        .last_run_at
        .map(|ts| store::format_relative_ago(ts, now_ms));

    let recent_turns = turns
        .into_iter()
        .map(|t| {
            let has_answer = !t.answer.trim().is_empty() || !t.widgets.is_empty();
            let answer_preview = if !t.answer.trim().is_empty() {
                store::compact_single_line(&t.answer, 160)
            } else if !t.widgets.is_empty() {
                format!("[{} widget(s)]", t.widgets.len())
            } else if let Some(ref err) = t.error_message {
                store::compact_single_line(err, 140)
            } else if let Some(ref tok) = t.suppressed_token {
                format!("suppressed ({tok})")
            } else {
                String::new()
            };
            ScheduleTurnSummary {
                id: t.id,
                status: t.status,
                created_at_ms: t.created_at_ms,
                created_local: store::format_local_timestamp(t.created_at_ms),
                relative_time: store::format_relative_time(t.created_at_ms, now_ms),
                finished_at_ms: t.finished_at_ms,
                profile: t.profile,
                has_answer,
                widget_count: t.widgets.len(),
                answer_preview,
                error_message: t.error_message,
                suppressed_token: t.suppressed_token,
            }
        })
        .collect();

    let entry = ScheduleListEntry {
        record: job,
        title,
        human_schedule,
        mode,
        running,
        next_run_local,
        next_run_relative,
        last_run_local,
        last_run_relative,
        latest_turn_id,
        active_turn_id,
    };
    (entry, recent_turns)
}

async fn schedule_cmd(
    config: &Config,
    config_path: Option<&Path>,
    cli_socket: Option<&Path>,
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
                let running_ids = query_running_schedules(socket).await;
                let now_ms = store::unix_timestamp_ms();
                let enriched: Vec<ScheduleListEntry> = jobs
                    .into_iter()
                    .map(|job| enrich_schedule(&state_dir, job, &running_ids, now_ms, 10).0)
                    .collect();
                println!("{}", serde_json::to_string(&enriched)?);
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
                let mode = schedule_mode(&job);
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
                let running_ids = query_running_schedules(socket).await;
                let now_ms = store::unix_timestamp_ms();
                let (entry, recent_turns) =
                    enrich_schedule(&state_dir, job, &running_ids, now_ms, 20);
                println!(
                    "{}",
                    serde_json::to_string(&ScheduleShowEntry {
                        entry,
                        recent_turns,
                    })?
                );
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
                    if ts <= now_ms {
                        anyhow::bail!("--expires time must be in the future");
                    }
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
        ScheduleCommand::Open { target, detach } => {
            view_cmd(
                config_path,
                cli_socket,
                None,
                None,
                target.as_deref(),
                target.is_none(),
                false,
                None,
                false,
                detach,
                false,
            )
            .await
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
    reply_to: Option<u64>,
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
                reply_to,
                stream: false,
                distill: false,
                schedule_id: None,
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
                present::notify_error(
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
            vec!["hey-computer", "ask", "--reply-to", "42", "follow", "up"],
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
            vec!["hey-computer", "tasks", "list", "--lane", "restricted"],
            vec!["hey-computer", "tasks", "show", "3", "--json"],
            vec!["hey-computer", "tasks", "open", "3"],
            vec!["hey-computer", "tasks", "copy", "3"],
            vec!["hey-computer", "tasks", "cancel", "3"],
            vec!["hey-computer", "schedule", "edit", "foo", "--no-agent"],
            vec![
                "hey-computer",
                "schedule",
                "edit",
                "foo",
                "--no-agent=false",
            ],
            vec!["hey-computer", "recall"],
            vec!["hey-computer", "history"],
            vec![
                "hey-computer",
                "history",
                "--limit",
                "60",
                "--lane",
                "all",
                "--include-failed",
                "--json",
            ],
            vec![
                "hey-computer",
                "history",
                "--no-threads",
                "--since",
                "7d",
                "nixos",
            ],
            vec![
                "hey-computer",
                "recall",
                "--on",
                "yesterday",
                "--user-only",
                "coffee",
            ],
            vec!["hey-computer", "recall", "--id", "42"],
            vec!["hey-computer", "recall", "--thread", "42", "--plain"],
            vec!["hey-computer", "schedule", "open"],
            vec!["hey-computer", "schedule", "open", "morning-digest", "-d"],
            vec!["hey-computer", "schedule", "view", "morning-digest"],
            vec!["hey-computer", "view", "42", "--detach"],
            vec!["hey-computer", "view", "#42", "--foreground"],
            vec!["hey-computer", "view", "--schedule", "morning-digest"],
            vec!["hey-computer", "view", "--schedules"],
            vec!["hey-computer", "view", "--history"],
            vec!["hey-computer", "view", "--recall", "nixos"],
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
            vec![
                "hey-computer",
                "recall",
                "--lane",
                "restricted",
                "--json",
                "--any",
                "--full",
                "foo",
            ],
            vec!["hey-computer", "distill"],
            vec![
                "hey-computer",
                "distill",
                "--since",
                "14d",
                "--limit",
                "80",
                "--profile",
                "deep",
                "--print",
            ],
            vec!["hey-computer", "distill", "-b"],
            vec!["hey-computer", "distill", "--json"],
            vec!["hey-computer", "speak"],
            vec!["hey-computer", "speak", "--json"],
            vec!["hey-computer", "speak", "on"],
            vec!["hey-computer", "speak", "off"],
            vec!["hey-computer", "speak", "toggle", "--json"],
            vec!["hey-computer", "speak", "status", "--json"],
            vec!["hey-computer", "view"],
            vec!["hey-computer", "view", "/tmp/answer.md"],
            vec![
                "hey-computer",
                "view",
                "--turn-id",
                "42",
                "--focus-composer",
                "/tmp/answer.md",
            ],
            vec!["hey-computer", "mcp-ui"],
            vec!["hey-computer", "status", "--json"],
            vec!["hey-computer", "last"],
            vec!["hey-computer", "last", "--lane", "all", "--json"],
            vec!["hey-computer", "last", "--lane", "restricted"],
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
    fn ask_rejects_reply_to_with_new_and_distill_rejects_conflicting_modes() {
        assert!(
            Cli::try_parse_from(["hey-computer", "ask", "--new", "--reply-to", "7", "hi"]).is_err()
        );
        assert!(Cli::try_parse_from(["hey-computer", "distill", "-b", "--print"]).is_err());
        assert!(Cli::try_parse_from(["hey-computer", "distill", "-b", "--json"]).is_err());
        assert!(Cli::try_parse_from(["hey-computer", "distill", "--print", "--json"]).is_err());
    }

    #[test]
    fn last_rejects_copy_and_open_together() {
        assert!(Cli::try_parse_from(["hey-computer", "last", "--copy", "--open"]).is_err());
    }

    #[test]
    fn lane_filter_maps_restricted_lane() {
        assert_eq!(
            LaneFilter::Restricted.into_lane(),
            Some(store::Lane::Restricted)
        );
        assert_eq!(LaneFilter::All.into_lane(), None);
    }

    #[tokio::test]
    async fn speak_works_offline_and_via_live_socket() {
        use hey_computer::lines::{self, Line};
        use tokio::io::{AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("hc-main-speak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_dir = dir.join("state");
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, format!("state_dir = {:?}\n", state_dir)).unwrap();
        let offline_sock = dir.join("offline.sock");

        // Offline toggle: false -> true -> false -> on -> status.
        assert!(!apply_offline_speak_action(&state_dir, false, SpeakAction::Status).unwrap());
        assert_eq!(
            speak_cmd(Some(&cfg_path), &offline_sock, SpeakAction::Toggle, false)
                .await
                .unwrap(),
            ExitCode::SUCCESS
        );
        assert!(store::load_speak_state(&state_dir, false));
        assert_eq!(
            speak_cmd(Some(&cfg_path), &offline_sock, SpeakAction::Toggle, true)
                .await
                .unwrap(),
            ExitCode::SUCCESS
        );
        assert!(!store::load_speak_state(&state_dir, true));

        // Live daemon socket takes precedence when running.
        let live_sock = dir.join("live.sock");
        let listener = UnixListener::bind(&live_sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let Line::Text(line) = lines::read(&mut reader, MAX_REQUEST_LINE).await.unwrap() else {
                panic!("expected speak request");
            };
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(
                req,
                Request::Speak {
                    action: SpeakAction::On
                }
            );
            let mut reply = protocol::encode(&Reply::SpeakStatus {
                enabled: true,
                speaking: true,
            });
            reply.push('\n');
            write.write_all(reply.as_bytes()).await.unwrap();
        });

        let code = speak_cmd(Some(&cfg_path), &live_sock, SpeakAction::On, true)
            .await
            .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        server.await.unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn viewer_args_forward_config_socket_turn_id_focus_and_file() {
        let args = build_viewer_args(&ViewerLaunchArgs {
            config_path: Some(Path::new("/tmp/cfg.toml")),
            socket_path: Some(Path::new("/tmp/hc.sock")),
            file: Some(Path::new("/tmp/ans.md")),
            turn_id: Some(19),
            focus_composer: true,
            ..ViewerLaunchArgs::default()
        });
        let strings: Vec<String> = args
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strings,
            vec![
                "--config",
                "/tmp/cfg.toml",
                "--socket",
                "/tmp/hc.sock",
                "--turn-id",
                "19",
                "--focus-composer",
                "/tmp/ans.md"
            ]
        );

        let sched_args = build_viewer_args(&ViewerLaunchArgs {
            schedule: Some("morning-digest"),
            ..ViewerLaunchArgs::default()
        });
        assert_eq!(
            sched_args
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--schedule", "morning-digest"]
        );

        let (file, tid) = resolve_view_target(Some("#42"), None, false);
        assert!(file.is_none());
        assert_eq!(tid, Some(42));
        let (file2, tid2) = resolve_view_target(Some("123"), None, false);
        assert!(file2.is_none());
        assert_eq!(tid2, Some(123));
        let (file3, tid3) = resolve_view_target(Some("/tmp/notes.md"), None, false);
        assert_eq!(file3, Some(PathBuf::from("/tmp/notes.md")));
        assert!(tid3.is_none());
    }
}

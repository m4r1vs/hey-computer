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
use hey_computer::protocol::{self, Attachment, AttachmentKind, MAX_REQUEST_LINE, Reply, Request};
use hey_computer::{daemon, debug, mock, store};

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
        /// Use a named profile from `[profiles.<name>]` (or `default`).
        #[arg(short = 'p', long, value_name = "NAME")]
        profile: Option<String>,
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
    /// Cancel the running request.
    Cancel,
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
            profile,
            files,
            images,
            text,
        } => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            ask(&config, &socket, text, print, new, profile, files, images).await
        }
        Command::Cancel => {
            let socket = ipc::socket_path(cli.socket.as_deref())?;
            match ipc::request(&socket, &Request::Cancel).await? {
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
async fn ask(
    config: &Config,
    socket: &Path,
    words: Vec<String>,
    print: bool,
    new: bool,
    profile: Option<String>,
    files: Vec<PathBuf>,
    images: Vec<PathBuf>,
) -> Result<ExitCode> {
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
                attachments,
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
        return Ok(ExitCode::SUCCESS);
    }

    match client.next(None).await? {
        Reply::Answer { text, denied, .. } => {
            println!("{text}");
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
            vec!["hey-computer", "ask", "--profile", "default", "hi"],
            vec!["hey-computer", "ask", "--print"],
            vec!["hey-computer", "--socket", "/tmp/s", "cancel"],
            vec!["hey-computer", "status", "--json"],
            vec!["hey-computer", "last"],
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

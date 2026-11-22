//! hey-computer: a voice assistant daemon that keeps one ACP agent warm.
//!
//! One binary, two roles: `daemon` owns the agent and the socket, every other
//! subcommand is a client that talks to it over the socket and exits.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;

use hey_computer::config::Config;
use hey_computer::ipc::{self, NotRunning};
use hey_computer::present::{self, Urgency};
use hey_computer::protocol::{Reply, Request};
use hey_computer::{daemon, debug, mock};

/// A request read from stdin is cut off here; speech never comes close.
const MAX_STDIN: u64 = 1024 * 1024;

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
    /// Check one part of the program on its own.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        what: DebugCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Check the configuration and print it with the defaults filled in.
    Config,
    /// Print the instructions the agent gets in its AGENTS.md.
    Instructions,
    /// Prepare the agent's directory and print where everything is.
    Prepare,
    /// Say whether an answer would go to a notification or the viewer.
    Classify { text: Vec<String> },
    /// Ask the agent directly, without a daemon, and print the answer.
    Prompt { text: Vec<String> },
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
    let socket = ipc::socket_path(cli.socket.as_deref());

    match cli.command {
        Command::Daemon => {
            daemon::run(config, &socket).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ask { print, new, text } => ask(&config, &socket, text, print, new).await,
        Command::Cancel => match ipc::request(&socket, &Request::Cancel).await? {
            Reply::Cancelled { id: Some(id) } => {
                println!("cancelled request {id}");
                Ok(ExitCode::SUCCESS)
            }
            Reply::Cancelled { id: None } => {
                println!("nothing to cancel");
                Ok(ExitCode::SUCCESS)
            }
            other => unexpected(other),
        },
        Command::Status { json } => match ipc::request(&socket, &Request::Status).await? {
            Reply::Status(status) => {
                if json {
                    println!("{}", serde_json::to_string(&status)?);
                } else {
                    println!("{status}");
                }
                Ok(ExitCode::SUCCESS)
            }
            other => unexpected(other),
        },
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
                DebugCommand::Prepare => {
                    debug::prepare(&config)?;
                    true
                }
                DebugCommand::Classify { text } => {
                    debug::classify(&config, &text_or_stdin(text).await?);
                    true
                }
                DebugCommand::Prompt { text } => {
                    debug::prompt(config, text_or_stdin(text).await?).await?
                }
                DebugCommand::Present { text } => {
                    debug::present(&config, text_or_stdin(text).await?).await;
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

async fn ask(
    config: &Config,
    socket: &Path,
    words: Vec<String>,
    print: bool,
    new: bool,
) -> Result<ExitCode> {
    let text = text_or_stdin(words).await?;
    if text.trim().is_empty() {
        eprintln!("hey-computer: the request is empty; nothing was asked");
        return Ok(ExitCode::from(2));
    }

    let (id, mut client) = match ipc::ask(socket, &text, print, new).await {
        Ok(accepted) => accepted,
        Err(e) => {
            // Usually started by a voice command, where nobody reads stderr.
            if e.downcast_ref::<NotRunning>().is_some() {
                present::notify(
                    &config.present,
                    "The assistant is not running",
                    &present::escape_markup("Start it with: systemctl --user start hey-computer"),
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

/// The words joined with spaces, or all of stdin when there are none.
async fn text_or_stdin(words: Vec<String>) -> Result<String> {
    if !words.is_empty() {
        return Ok(words.join(" "));
    }
    let mut text = String::new();
    tokio::io::stdin()
        .take(MAX_STDIN)
        .read_to_string(&mut text)
        .await
        .context("reading the request from stdin")?;
    Ok(text)
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
            vec!["hey-computer", "ask", "--print"],
            vec!["hey-computer", "--socket", "/tmp/s", "cancel"],
            vec!["hey-computer", "status", "--json"],
            vec!["hey-computer", "debug", "config"],
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
}

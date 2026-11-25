//! `hey-computer-bridge`: strictly read-only WhatsApp & Signal messenger bridge.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use hey_computer::bridge;
use hey_computer::bridge::config::{BridgeConfig, default_socket_path};
use hey_computer::bridge::protocol::{BridgeEvent, BridgeRequest};

#[derive(Debug, Parser)]
#[command(
    name = "hey-computer-bridge",
    about = "Strictly read-only WhatsApp and Signal bridge for hey-computer"
)]
struct Cli {
    /// Path to bridge.toml (default: ~/.config/hey-computer/bridge.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Path to the bridge UNIX socket (default: $XDG_RUNTIME_DIR/hey-computer-bridge.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the read-only messenger bridge supervisor daemon.
    Daemon,
    /// Print bridge status from the running daemon.
    Status {
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Interactively link a read-only WhatsApp or Signal device.
    Pair {
        #[command(subcommand)]
        platform: PairPlatform,
    },
}

#[derive(Debug, Subcommand)]
enum PairPlatform {
    /// Link a read-only WhatsApp Web session via terminal QR code.
    Whatsapp,
    /// Link a read-only Signal secondary device via terminal QR code.
    Signal {
        /// Device label shown in Signal -> Linked Devices.
        #[arg(long, default_value = "hey-computer")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("hey_computer=info,hey_computer_bridge=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Daemon => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            let socket = default_socket_path(cli.socket.as_deref())?;
            bridge::run_daemon(config, &socket).await
        }
        Command::Status { json } => {
            let socket = default_socket_path(cli.socket.as_deref())?;
            let reply = bridge::query(&socket, &BridgeRequest::Status).await?;
            if json {
                println!("{}", serde_json::to_string(&reply)?);
            } else {
                match reply {
                    BridgeEvent::Status {
                        read_only,
                        whatsapp,
                        signal,
                        whatsapp_detail,
                        signal_detail,
                        tracked_chats,
                        ..
                    } => {
                        println!("read_only: {read_only}");
                        match whatsapp_detail {
                            Some(d) => println!("whatsapp: {whatsapp} ({d})"),
                            None => println!("whatsapp: {whatsapp}"),
                        }
                        match signal_detail {
                            Some(d) => println!("signal: {signal} ({d})"),
                            None => println!("signal: {signal}"),
                        }
                        println!("tracked_chats: {tracked_chats}");
                    }
                    other => println!("{}", serde_json::to_string(&other)?),
                }
            }
            Ok(())
        }
        Command::Pair { platform } => {
            let config = BridgeConfig::load(cli.config.as_deref())?;
            match platform {
                PairPlatform::Whatsapp => bridge::whatsapp::pair_whatsapp(&config).await,
                PairPlatform::Signal { name } => bridge::signal::pair_signal(&config, &name).await,
            }
        }
    }
}

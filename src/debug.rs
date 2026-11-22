//! Hidden `debug …` commands, each checking one part on its own.

use std::sync::Arc;

use anyhow::Result;

use crate::conductor::{self, Outcome};
use crate::config::Config;
use crate::environment;
use crate::ipc;
use crate::present::{self, Desktop, Limits, Presentation, Presenter};

/// Check the configuration and print it with every default filled in.
pub fn config(config: &Config) -> Result<()> {
    config.validate()?;
    print!("{}", toml::to_string(config)?);
    Ok(())
}

/// Print the instructions the agent gets.
pub fn instructions(config: &Config) {
    print!("{}", environment::instructions(config));
}

/// Prepare the agent's directory, as the daemon does before starting it.
pub fn prepare(config: &Config) -> Result<()> {
    let paths = environment::prepare(config)?;
    println!("agent directory: {}", paths.agent_dir.display());
    println!("workspace:       {}", paths.workspace.display());
    println!("agent command:   {:?}", paths.agent_command(config));
    println!(
        "settings:        {}",
        serde_json::to_string(&environment::settings_for_display(config)?)?
    );
    Ok(())
}

/// How an answer would be shown.
pub fn classify(config: &Config, text: &str) {
    if present::is_short(text, Limits::from(&config.present)) {
        println!("notification");
        println!("{}", present::to_markup(text));
    } else {
        println!("viewer");
        println!("{}", present::headline(text, 120));
    }
}

/// One request straight to the agent, without a daemon. With `-v` the log
/// shows when each tool call and permission request arrived.
pub async fn prompt(config: Config, text: String) -> Result<bool> {
    match conductor::ask_once(config, text).await? {
        Outcome::Answer { text, denied } => {
            println!("{text}");
            for command in denied {
                eprintln!("refused: {command}");
            }
            Ok(true)
        }
        Outcome::Failed { reason, message } => {
            eprintln!("{reason:?}: {message}");
            Ok(false)
        }
    }
}

/// Show `text` the way the daemon would show an answer to it.
pub async fn present(config: &Config, text: String) {
    let answers = ipc::runtime_dir().join("hey-computer");
    let desktop = Desktop::spawn(&config.present, answers);
    let presenter: Arc<Desktop> = Arc::clone(&desktop);
    presenter.present(Presentation::Progress {
        id: 0,
        request: "debug present".to_string(),
    });
    presenter.present(Presentation::Answer {
        id: 0,
        text,
        denied: Vec::new(),
    });
    desktop.finish().await;
}

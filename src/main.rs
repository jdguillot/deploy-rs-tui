//! deploy-rs-tui — terminal UI for serokell/deploy-rs.
//!
//! Discovers `deploy.nodes` from a Nix flake, shows host status, and runs
//! `deploy` for the selected node/profile.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use deploy_rs_tui::{app::App, flake, ui};

/// CLI arguments. The TUI runs against a single flake reference.
#[derive(Debug, Parser)]
#[command(name = "deptui", version, about)]
struct Cli {
    /// Path to the flake containing `deploy.nodes` (defaults to the current
    /// directory). Anything `nix` accepts as a flakeref works here too.
    #[arg(default_value = ".")]
    flake: String,

    /// Optional log file. When set, tracing logs are written here instead of
    /// stderr (which would corrupt the TUI).
    #[arg(long)]
    log_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.log_file.as_deref())?;

    let nodes = flake::discover(&cli.flake)
        .await
        .with_context(|| format!("discovering deploy.nodes in `{}`", cli.flake))?;

    if nodes.is_empty() {
        eprintln!(
            "no deploy.nodes found in `{}` — nothing to show.",
            cli.flake
        );
        return Ok(());
    }

    let mut terminal = ui::init()?;
    let result = App::new(cli.flake.clone(), nodes).run(&mut terminal).await;
    ui::restore()?;
    result
}

fn init_tracing(log_file: Option<&std::path::Path>) -> Result<()> {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_ansi(false)
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;
    } else {
        // No log file: stay silent so we don't garble the TUI.
        fmt()
            .with_env_filter(filter)
            .with_writer(std::io::sink)
            .try_init()
            .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;
    }
    Ok(())
}

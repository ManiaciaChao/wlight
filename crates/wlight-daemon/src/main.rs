mod config;
mod service;
mod state;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config::Config;
use service::ManagerService;
use state::ManagerState;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wlight_dbus::{OBJECT_PATH, SERVICE_NAME};

#[derive(Debug, Parser)]
#[command(version, about = "DDC and Wayland gamma brightness service")]
struct Cli {
    /// Use a non-default configuration file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the DDC floor used by unified brightness, as a percentage.
    #[arg(long, value_name = "0..100")]
    hardware_floor: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let cli = Cli::parse();
    let config_path = match cli.config {
        Some(path) => path,
        None => config::default_path()?,
    };
    let config = Config::load(&config_path)?;
    config.validate()?;
    let hardware_floor = match cli.hardware_floor {
        Some(percent) if percent.is_finite() && (0.0..=100.0).contains(&percent) => percent / 100.0,
        Some(_) => bail!("--hardware-floor must be finite and between 0 and 100"),
        None => config.hardware_floor,
    };

    let state =
        tokio::task::spawn_blocking(move || ManagerState::new(config, config_path, hardware_floor))
            .await
            .context("backend initialization worker stopped")??;
    let display_count = state.displays().len();
    let service = ManagerService::new(state);
    let connection = zbus::connection::Builder::session()
        .context("failed to connect to the user D-Bus")?
        .name(SERVICE_NAME)
        .context("failed to request the wlight D-Bus name")?
        .serve_at(OBJECT_PATH, service.clone())
        .context("failed to register the wlight D-Bus object")?
        .build()
        .await
        .context("failed to finish the D-Bus connection")?;

    info!(display_count, "wlightd is ready");
    if display_count == 0 {
        warn!("no controllable displays found; use wlightctl refresh after checking permissions");
    }

    wait_for_shutdown().await?;
    info!("shutting down");
    service.shutdown().await;
    drop(connection);
    Ok(())
}

async fn wait_for_shutdown() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate =
        signal(SignalKind::terminate()).context("failed to install the SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.context("failed to install the Ctrl-C handler")?;
        }
        _ = terminate.recv() => {}
    }
    Ok(())
}

fn init_tracing() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("wlightd=info,wlight_backend=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    Ok(())
}

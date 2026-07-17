use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use wlight_core::DisplayInfo;
use wlight_dbus::ManagerProxy;

#[derive(Debug, Parser)]
#[command(version, about = "Control monitor brightness through wlightd")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the cached monitor state.
    List {
        /// Emit a JSON array instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Re-enumerate DDC and Wayland outputs.
    Refresh {
        /// Emit a JSON array instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Set unified effective brightness (DDC first, gamma below the floor).
    Set {
        /// Display ID from `wlightctl list`.
        id: String,
        /// Effective brightness percentage, from 0 to 100.
        percent: f64,
    },
    /// Set hardware brightness directly through DDC/CI.
    SetDdc {
        id: String,
        /// Hardware brightness percentage, from 0 to 100.
        percent: u16,
    },
    /// Set the software gamma-LUT multiplier directly.
    SetGamma {
        id: String,
        /// Gamma brightness percentage, from 0 to 100.
        percent: f64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_command(&cli.command)?;
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to the user D-Bus")?;
    let proxy = ManagerProxy::new(&connection)
        .await
        .context("failed to create the wlight D-Bus proxy")?;

    match cli.command {
        Command::List { json } => print_displays(&proxy.list_displays().await?, json)?,
        Command::Refresh { json } => print_displays(&proxy.refresh().await?, json)?,
        Command::Set { id, percent } => {
            let value = percentage_fraction(percent)?;
            print_display(&proxy.set_brightness(&id, value).await?);
        }
        Command::SetDdc { id, percent } => {
            print_display(&proxy.set_ddc_brightness(&id, percent).await?);
        }
        Command::SetGamma { id, percent } => {
            let value = percentage_fraction(percent)?;
            print_display(&proxy.set_gamma_brightness(&id, value).await?);
        }
    }

    Ok(())
}

fn validate_command(command: &Command) -> Result<()> {
    match command {
        Command::Set { percent, .. } | Command::SetGamma { percent, .. } => {
            let _fraction = percentage_fraction(*percent)?;
        }
        Command::SetDdc { percent, .. } if *percent > 100 => {
            bail!("DDC percentage must be between 0 and 100");
        }
        Command::List { .. } | Command::Refresh { .. } | Command::SetDdc { .. } => {}
    }
    Ok(())
}

fn percentage_fraction(percent: f64) -> Result<f64> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        bail!("percentage must be finite and between 0 and 100");
    }
    Ok(percent / 100.0)
}

fn print_displays(displays: &[DisplayInfo], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(displays).context("failed to encode display state")?
        );
        return Ok(());
    }

    if displays.is_empty() {
        println!("No controllable displays found.");
        return Ok(());
    }

    println!(
        "{:<20} {:<18} {:<12} {:>7} {:>7} {:>9}",
        "ID", "NAME", "CONNECTOR", "DDC", "GAMMA", "EFFECTIVE"
    );
    for display in displays {
        print_display(display);
    }
    Ok(())
}

fn print_display(display: &DisplayInfo) {
    let ddc = if display.ddc_supported {
        format!("{}%", display.ddc_brightness)
    } else {
        "—".to_owned()
    };
    let gamma = if display.gamma_supported {
        format!("{:.0}%", display.gamma_brightness * 100.0)
    } else {
        "—".to_owned()
    };
    println!(
        "{:<20} {:<18} {:<12} {:>7} {:>7} {:>8.0}%",
        display.id,
        display.name,
        display.connector,
        ddc,
        gamma,
        display.effective_percent()
    );
    if !display.last_error.is_empty() {
        eprintln!("  warning: {}", display.last_error);
    }
}

#[cfg(test)]
mod tests {
    use super::percentage_fraction;

    #[test]
    fn percentage_validation() {
        assert_eq!(percentage_fraction(25.0).expect("valid percentage"), 0.25);
        assert!(percentage_fraction(-1.0).is_err());
        assert!(percentage_fraction(101.0).is_err());
        assert!(percentage_fraction(f64::NAN).is_err());
    }
}

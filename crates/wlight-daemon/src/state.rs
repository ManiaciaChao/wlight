use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, warn};
use wlight_backend::{DdcBackend, GammaBackend};
use wlight_core::{BrightnessPlan, DisplayInfo};

use crate::config::{Config, SavedDisplay};

pub struct ManagerState {
    ddc: DdcBackend,
    gamma: Option<GammaBackend>,
    displays: Vec<DisplayInfo>,
    errors: HashMap<String, String>,
    config: Config,
    config_path: PathBuf,
    hardware_floor: f64,
}

#[derive(Debug, Clone, Copy)]
struct RestoreControls {
    ddc: bool,
    gamma: bool,
}

impl RestoreControls {
    const fn all() -> Self {
        Self {
            ddc: true,
            gamma: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Adjustment {
    Ddc { from: u16, to: u16 },
    Gamma { from: f64, to: f64 },
}

impl Adjustment {
    fn order_key(self) -> u8 {
        match self {
            Self::Gamma { from, to } if to < from => 0,
            Self::Ddc { from, to } if to < from => 1,
            Self::Ddc { .. } => 2,
            Self::Gamma { .. } => 3,
        }
    }

    fn rollback(self) -> Self {
        match self {
            Self::Ddc { from, to } => Self::Ddc { from: to, to: from },
            Self::Gamma { from, to } => Self::Gamma { from: to, to: from },
        }
    }
}

impl ManagerState {
    pub fn new(config: Config, config_path: PathBuf, hardware_floor: f64) -> Result<Self> {
        let ddc = DdcBackend::enumerate().context("failed to initialize the DDC backend")?;
        let gamma = match GammaBackend::connect() {
            Ok(gamma) => Some(gamma),
            Err(error) => {
                warn!(%error, "Wayland gamma control is unavailable; continuing with DDC only");
                None
            }
        };
        let mut state = Self {
            ddc,
            gamma,
            displays: Vec::new(),
            errors: HashMap::new(),
            config,
            config_path,
            hardware_floor,
        };
        state.rebuild_snapshot();
        let controls = state
            .displays
            .iter()
            .map(|display| (display.id.clone(), RestoreControls::all()))
            .collect();
        state.restore_saved_settings(&controls);
        state.rebuild_snapshot();
        Ok(state)
    }

    #[must_use]
    pub fn displays(&self) -> Vec<DisplayInfo> {
        self.displays.clone()
    }

    pub fn refresh(&mut self) -> Result<Vec<DisplayInfo>> {
        self.ddc
            .refresh()
            .context("failed to refresh DDC devices")?;
        self.refresh_gamma();
        self.rebuild_snapshot();

        // Treat the persisted settings as desired state. Reconciliation is
        // deliberately based on the values read from the new control objects,
        // rather than only on display IDs: a monitor can disconnect and return
        // between two refreshes with the same ID but reset DDC/gamma values.
        // `apply_plan` skips controls that already match, so ordinary refreshes
        // do not produce redundant hardware writes.
        let controls = self
            .displays
            .iter()
            .filter(|display| self.config.displays.contains_key(&display.id))
            .map(|display| {
                (
                    display.id.clone(),
                    RestoreControls {
                        ddc: display.ddc_supported,
                        gamma: display.gamma_supported,
                    },
                )
            })
            .collect();
        self.restore_saved_settings(&controls);
        self.rebuild_snapshot();
        Ok(self.displays())
    }

    pub fn set_brightness(&mut self, id: &str, target: f64) -> Result<DisplayInfo> {
        let current = self.find(id)?.clone();
        let plan = BrightnessPlan::for_target(
            target,
            self.hardware_floor,
            current.ddc_supported,
            current.gamma_supported,
        )?;

        self.errors.remove(id);
        let result = self.apply_plan(id, &current, plan);
        self.rebuild_snapshot();
        match result {
            Ok(()) => {
                let saved = self.saved_mut(id);
                if let Some(percent) = plan.hardware_percent {
                    saved.ddc_brightness = Some(percent);
                }
                if let Some(brightness) = plan.gamma_brightness {
                    saved.gamma_brightness = Some(brightness);
                }
                self.save_config_for(id)?;
                self.find(id).cloned()
            }
            Err(error) => {
                self.record_error(id, &error);
                Err(error)
            }
        }
    }

    pub fn set_ddc_brightness(&mut self, id: &str, percent: u16) -> Result<DisplayInfo> {
        if percent > 100 {
            bail!("DDC brightness must be between 0 and 100");
        }
        let current = self.find(id)?.clone();
        if !current.ddc_supported {
            bail!("display {id} does not support DDC brightness");
        }

        self.errors.remove(id);
        if current.ddc_brightness != percent
            && let Err(error) = self.ddc.set_brightness(id, percent)
        {
            let error = error.context("DDC write failed");
            self.record_error(id, &error);
            return Err(error);
        }
        self.rebuild_snapshot();
        self.saved_mut(id).ddc_brightness = Some(percent);
        self.save_config_for(id)?;
        self.find(id).cloned()
    }

    pub fn set_gamma_brightness(&mut self, id: &str, brightness: f64) -> Result<DisplayInfo> {
        if !brightness.is_finite() || !(0.0..=1.0).contains(&brightness) {
            bail!("gamma brightness must be finite and between 0.0 and 1.0");
        }
        let current = self.find(id)?.clone();
        if !current.gamma_supported {
            bail!("display {id} does not support Wayland gamma control");
        }

        self.errors.remove(id);
        if (current.gamma_brightness - brightness).abs() > f64::EPSILON
            && let Err(error) = self.apply_gamma(&current.connector, brightness)
        {
            let error = error.context("gamma update failed");
            self.record_error(id, &error);
            return Err(error);
        }
        self.rebuild_snapshot();
        self.saved_mut(id).gamma_brightness = Some(brightness);
        self.save_config_for(id)?;
        self.find(id).cloned()
    }

    pub fn shutdown(&mut self) {
        if let Some(mut gamma) = self.gamma.take()
            && let Err(error) = gamma.shutdown()
        {
            debug!(%error, "error while shutting down the gamma backend");
        }
    }

    fn apply_plan(&mut self, id: &str, current: &DisplayInfo, plan: BrightnessPlan) -> Result<()> {
        let adjustments = ordered_adjustments(current, plan);
        let mut applied: Vec<Adjustment> = Vec::with_capacity(adjustments.len());

        for adjustment in adjustments {
            if let Err(error) = self.apply_adjustment(id, &current.connector, adjustment) {
                let mut rollback_failures = Vec::new();
                for previous in applied.into_iter().rev() {
                    if let Err(rollback_error) =
                        self.apply_adjustment(id, &current.connector, previous.rollback())
                    {
                        rollback_failures.push(format!("{rollback_error:#}"));
                    }
                }
                if rollback_failures.is_empty() {
                    return Err(error);
                }
                return Err(anyhow!(
                    "{error:#}; rollback also failed: {}",
                    rollback_failures.join("; ")
                ));
            }
            applied.push(adjustment);
        }
        Ok(())
    }

    fn apply_adjustment(
        &mut self,
        id: &str,
        connector: &str,
        adjustment: Adjustment,
    ) -> Result<()> {
        match adjustment {
            Adjustment::Ddc { to, .. } => {
                self.ddc
                    .set_brightness(id, to)
                    .context("DDC write failed")?;
            }
            Adjustment::Gamma { to, .. } => self.apply_gamma(connector, to)?,
        }
        Ok(())
    }

    fn apply_gamma(&mut self, connector: &str, brightness: f64) -> Result<()> {
        if connector.is_empty() {
            bail!("the display is not associated with a Wayland connector");
        }
        self.gamma
            .as_mut()
            .context("Wayland gamma backend is unavailable")?
            .set_brightness(connector, brightness)
            .with_context(|| format!("failed to update gamma for {connector}"))?;
        Ok(())
    }

    fn refresh_gamma(&mut self) {
        let needs_reconnect = self
            .gamma
            .as_ref()
            .is_none_or(GammaBackend::needs_reconnect);
        if !needs_reconnect {
            if let Some(gamma) = &self.gamma {
                let _outputs = gamma.refresh();
            }
            return;
        }

        if let Some(mut stale) = self.gamma.take()
            && let Err(error) = stale.shutdown()
        {
            debug!(%error, "failed to join stale Wayland gamma backend");
        }
        match GammaBackend::connect() {
            Ok(gamma) => {
                self.gamma = Some(gamma);
            }
            Err(error) => {
                warn!(%error, "Wayland gamma control is still unavailable");
            }
        }
    }

    fn restore_saved_settings(&mut self, controls: &HashMap<String, RestoreControls>) {
        let current = self.displays.clone();
        for display in current {
            let Some(controls) = controls.get(&display.id).copied() else {
                continue;
            };
            let Some(saved) = self.config.displays.get(&display.id).cloned() else {
                continue;
            };
            let plan = BrightnessPlan {
                hardware_percent: (controls.ddc && display.ddc_supported)
                    .then_some(saved.ddc_brightness)
                    .flatten(),
                gamma_brightness: (controls.gamma && display.gamma_supported)
                    .then_some(saved.gamma_brightness)
                    .flatten(),
            };
            if plan.hardware_percent.is_none() && plan.gamma_brightness.is_none() {
                continue;
            }
            match self.apply_plan(&display.id, &display, plan) {
                Ok(()) => {
                    self.errors.remove(&display.id);
                }
                Err(error) => {
                    self.errors
                        .insert(display.id, format!("settings restore failed: {error:#}"));
                }
            }
        }
    }

    fn rebuild_snapshot(&mut self) {
        let gamma_outputs = self
            .gamma
            .as_ref()
            .map_or_else(Vec::new, GammaBackend::outputs);
        let mut used_outputs = HashSet::new();
        let mut displays = Vec::new();

        for ddc in self.ddc.displays() {
            let ddc_brightness = ddc.percent();
            let gamma = gamma_outputs
                .iter()
                .find(|output| !ddc.connector.is_empty() && output.name == ddc.connector);
            if let Some(output) = gamma {
                used_outputs.insert(output.name.clone());
            }
            displays.push(DisplayInfo {
                id: ddc.id.clone(),
                name: ddc.name,
                connector: ddc.connector,
                ddc_brightness,
                ddc_supported: true,
                gamma_brightness: gamma.map_or(1.0, |output| output.brightness),
                gamma_supported: gamma.is_some(),
                last_error: self.errors.get(&ddc.id).cloned().unwrap_or_default(),
            });
        }

        for output in gamma_outputs {
            if used_outputs.contains(&output.name) {
                continue;
            }
            let id = format!("wayland:{}", output.name);
            displays.push(DisplayInfo {
                id: id.clone(),
                name: output.name.clone(),
                connector: output.name,
                ddc_brightness: 100,
                ddc_supported: false,
                gamma_brightness: output.brightness,
                gamma_supported: true,
                last_error: self.errors.get(&id).cloned().unwrap_or_default(),
            });
        }

        displays.sort_by(|left, right| {
            left.connector
                .cmp(&right.connector)
                .then_with(|| left.id.cmp(&right.id))
        });
        self.displays = displays;
    }

    fn find(&self, id: &str) -> Result<&DisplayInfo> {
        self.displays
            .iter()
            .find(|display| display.id == id)
            .with_context(|| format!("unknown display {id}"))
    }

    fn saved_mut(&mut self, id: &str) -> &mut SavedDisplay {
        self.config.displays.entry(id.to_owned()).or_default()
    }

    fn save_config_for(&mut self, id: &str) -> Result<()> {
        if let Err(error) = self.config.save(&self.config_path) {
            let error = error.context("hardware changed, but configuration could not be saved");
            self.record_error(id, &error);
            return Err(error);
        }
        Ok(())
    }

    fn record_error(&mut self, id: &str, error: &anyhow::Error) {
        self.errors.insert(id.to_owned(), format!("{error:#}"));
        self.rebuild_snapshot();
    }
}

fn ordered_adjustments(current: &DisplayInfo, plan: BrightnessPlan) -> Vec<Adjustment> {
    let mut adjustments = Vec::with_capacity(2);
    if let Some(to) = plan.hardware_percent
        && to != current.ddc_brightness
    {
        adjustments.push(Adjustment::Ddc {
            from: current.ddc_brightness,
            to,
        });
    }
    if let Some(to) = plan.gamma_brightness
        && (to - current.gamma_brightness).abs() > f64::EPSILON
    {
        adjustments.push(Adjustment::Gamma {
            from: current.gamma_brightness,
            to,
        });
    }
    adjustments.sort_by_key(|adjustment| adjustment.order_key());
    adjustments
}

impl Drop for ManagerState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(ddc: u16, gamma: f64) -> DisplayInfo {
        DisplayInfo {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            connector: "DP-1".to_owned(),
            ddc_brightness: ddc,
            ddc_supported: true,
            gamma_brightness: gamma,
            gamma_supported: true,
            last_error: String::new(),
        }
    }

    #[test]
    fn lowers_hardware_before_raising_gamma() {
        let adjustments = ordered_adjustments(
            &display(100, 0.1),
            BrightnessPlan {
                hardware_percent: Some(20),
                gamma_brightness: Some(0.75),
            },
        );
        assert!(matches!(adjustments[0], Adjustment::Ddc { .. }));
        assert!(matches!(adjustments[1], Adjustment::Gamma { .. }));
    }

    #[test]
    fn lowers_gamma_before_raising_hardware() {
        let adjustments = ordered_adjustments(
            &display(20, 0.75),
            BrightnessPlan {
                hardware_percent: Some(100),
                gamma_brightness: Some(0.1),
            },
        );
        assert!(matches!(adjustments[0], Adjustment::Gamma { .. }));
        assert!(matches!(adjustments[1], Adjustment::Ddc { .. }));
    }

    #[test]
    fn skips_unchanged_controls() {
        let adjustments = ordered_adjustments(
            &display(40, 0.5),
            BrightnessPlan {
                hardware_percent: Some(40),
                gamma_brightness: Some(0.5),
            },
        );
        assert!(adjustments.is_empty());
    }
}

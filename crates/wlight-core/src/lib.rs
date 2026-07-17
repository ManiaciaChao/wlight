//! Hardware-independent brightness policy and public monitor model.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zvariant::Type;

/// Default hardware level below which unified control switches to a gamma LUT.
pub const DEFAULT_HARDWARE_FLOOR: f64 = 0.20;

/// A monitor as exposed over D-Bus.
///
/// Empty `connector` and `last_error` strings represent an unavailable value.
/// Concrete fields keep the wire format simple and friendly to `busctl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DisplayInfo {
    pub id: String,
    pub name: String,
    pub connector: String,
    pub ddc_brightness: u16,
    pub ddc_supported: bool,
    pub gamma_brightness: f64,
    pub gamma_supported: bool,
    pub last_error: String,
}

impl DisplayInfo {
    /// Returns the estimated emitted-light percentage after both controls.
    #[must_use]
    pub fn effective_percent(&self) -> f64 {
        let hardware = if self.ddc_supported {
            f64::from(self.ddc_brightness) / 100.0
        } else {
            1.0
        };
        let gamma = if self.gamma_supported {
            self.gamma_brightness
        } else {
            1.0
        };
        (hardware * gamma * 100.0).clamp(0.0, 100.0)
    }
}

/// Values that the unified brightness control should apply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrightnessPlan {
    pub hardware_percent: Option<u16>,
    pub gamma_brightness: Option<f64>,
}

/// Input validation and capability errors from the brightness policy.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum BrightnessError {
    #[error("brightness must be finite and between 0.0 and 1.0")]
    InvalidTarget,
    #[error("hardware floor must be finite and between 0.0 and 1.0")]
    InvalidFloor,
    #[error("the display exposes neither DDC nor gamma control")]
    Unsupported,
    #[error("gamma ramp size must be at least 2")]
    InvalidRampSize,
}

impl BrightnessPlan {
    /// Split an effective target between DDC and a software gamma multiplier.
    ///
    /// With both mechanisms available, DDC is used down to `hardware_floor`.
    /// Targets below it keep that hardware level and attenuate the gamma LUT.
    pub fn for_target(
        target: f64,
        hardware_floor: f64,
        ddc_available: bool,
        gamma_available: bool,
    ) -> Result<Self, BrightnessError> {
        if !target.is_finite() || !(0.0..=1.0).contains(&target) {
            return Err(BrightnessError::InvalidTarget);
        }
        if !hardware_floor.is_finite() || !(0.0..=1.0).contains(&hardware_floor) {
            return Err(BrightnessError::InvalidFloor);
        }

        match (ddc_available, gamma_available) {
            (true, true) => {
                // DDC is exposed as an integer percentage. Quantize upward so the
                // hardware factor never undershoots the requested effective level,
                // then let gamma provide the exact fractional remainder.
                let floor_percent = if hardware_floor == 0.0 {
                    0
                } else {
                    (hardware_floor * 100.0).ceil().clamp(1.0, 100.0) as u16
                };
                let target_percent = (target * 100.0).ceil().clamp(0.0, 100.0) as u16;
                let hardware_percent = floor_percent.max(target_percent);
                let gamma_brightness = if hardware_percent == 0 {
                    1.0
                } else {
                    (target / (f64::from(hardware_percent) / 100.0)).clamp(0.0, 1.0)
                };
                Ok(Self {
                    hardware_percent: Some(hardware_percent),
                    gamma_brightness: Some(gamma_brightness),
                })
            }
            (true, false) => Ok(Self {
                hardware_percent: Some((target * 100.0).round() as u16),
                gamma_brightness: None,
            }),
            (false, true) => Ok(Self {
                hardware_percent: None,
                gamma_brightness: Some(target),
            }),
            (false, false) => Err(BrightnessError::Unsupported),
        }
    }
}

/// Build the native-endian `RRR…GGG…BBB…` table required by
/// `wlr-gamma-control-unstable-v1`.
pub fn gamma_table(size: usize, brightness: f64) -> Result<Vec<u16>, BrightnessError> {
    if size < 2 {
        return Err(BrightnessError::InvalidRampSize);
    }
    if !brightness.is_finite() || !(0.0..=1.0).contains(&brightness) {
        return Err(BrightnessError::InvalidTarget);
    }

    let channel: Vec<u16> = (0..size)
        .map(|index| {
            let normalized = index as f64 / (size - 1) as f64;
            (normalized * brightness * f64::from(u16::MAX)).round() as u16
        })
        .collect();

    let mut table = Vec::with_capacity(size * 3);
    table.extend_from_slice(&channel);
    table.extend_from_slice(&channel);
    table.extend_from_slice(&channel);
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_above_floor_uses_ddc_only() {
        let plan =
            BrightnessPlan::for_target(0.63, 0.2, true, true).expect("valid brightness plan");
        assert_eq!(plan.hardware_percent, Some(63));
        assert_eq!(plan.gamma_brightness, Some(1.0));
    }

    #[test]
    fn target_below_floor_uses_gamma() {
        let plan =
            BrightnessPlan::for_target(0.08, 0.2, true, true).expect("valid brightness plan");
        assert_eq!(plan.hardware_percent, Some(20));
        assert!((plan.gamma_brightness.expect("gamma value") - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn gamma_only_display_is_supported() {
        let plan =
            BrightnessPlan::for_target(0.4, 0.2, false, true).expect("valid brightness plan");
        assert_eq!(plan.hardware_percent, None);
        assert_eq!(plan.gamma_brightness, Some(0.4));
    }

    #[test]
    fn zero_target_is_exact() {
        let combined =
            BrightnessPlan::for_target(0.0, 0.2, true, true).expect("combined brightness plan");
        assert_eq!(combined.hardware_percent, Some(20));
        assert_eq!(combined.gamma_brightness, Some(0.0));

        let gamma_only =
            BrightnessPlan::for_target(0.0, 0.2, false, true).expect("gamma-only brightness plan");
        assert_eq!(gamma_only.gamma_brightness, Some(0.0));
    }

    #[test]
    fn quantized_floor_is_used_for_gamma_ratio() {
        let plan =
            BrightnessPlan::for_target(0.002, 0.004, true, true).expect("small brightness plan");
        assert_eq!(plan.hardware_percent, Some(1));
        assert!((plan.gamma_brightness.expect("gamma") - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn gamma_corrects_fractional_hardware_percent() {
        let plan =
            BrightnessPlan::for_target(0.632, 0.2, true, true).expect("fractional brightness plan");
        assert_eq!(plan.hardware_percent, Some(64));
        assert!((plan.gamma_brightness.expect("gamma") - 0.9875).abs() < f64::EPSILON);
    }

    #[test]
    fn gamma_table_has_three_identical_channels() {
        let table = gamma_table(4, 0.5).expect("valid table");
        assert_eq!(table.len(), 12);
        assert_eq!(&table[0..4], &table[4..8]);
        assert_eq!(&table[4..8], &table[8..12]);
        assert_eq!(table[0], 0);
        assert_eq!(table[3], 32768);
    }

    #[test]
    fn rejects_non_finite_target() {
        assert_eq!(
            BrightnessPlan::for_target(f64::NAN, 0.2, true, true),
            Err(BrightnessError::InvalidTarget)
        );
    }
}

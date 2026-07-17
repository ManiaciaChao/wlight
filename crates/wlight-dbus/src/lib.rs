//! Shared D-Bus contract used by the daemon and all frontends.

pub use wlight_core::DisplayInfo;

pub const SERVICE_NAME: &str = "io.github.wlight";
pub const OBJECT_PATH: &str = "/io/github/wlight";
pub const INTERFACE_NAME: &str = "io.github.wlight.Manager1";

#[zbus::proxy(
    interface = "io.github.wlight.Manager1",
    default_service = "io.github.wlight",
    default_path = "/io/github/wlight"
)]
pub trait Manager {
    /// Return the daemon's current snapshot without touching hardware.
    fn list_displays(&self) -> zbus::Result<Vec<DisplayInfo>>;

    /// Re-enumerate DDC devices and Wayland outputs.
    fn refresh(&self) -> zbus::Result<Vec<DisplayInfo>>;

    /// Set an effective brightness in the inclusive range 0.0..=1.0.
    fn set_brightness(&self, id: &str, brightness: f64) -> zbus::Result<DisplayInfo>;

    /// Set the hardware DDC brightness percentage.
    fn set_ddc_brightness(&self, id: &str, brightness: u16) -> zbus::Result<DisplayInfo>;

    /// Set the gamma-LUT brightness multiplier in the range 0.0..=1.0.
    fn set_gamma_brightness(&self, id: &str, brightness: f64) -> zbus::Result<DisplayInfo>;
}

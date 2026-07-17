//! Linux display-control backends used by the wlight daemon.

pub mod ddc;
pub mod gamma;

pub use ddc::{DdcBackend, DdcDisplay};
pub use gamma::{GammaBackend, GammaOutput};

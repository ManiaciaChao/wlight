use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use wlight_core::DisplayInfo;

use crate::state::ManagerState;

#[derive(Clone)]
pub struct ManagerService {
    state: Arc<Mutex<ManagerState>>,
    snapshot: Arc<RwLock<Vec<DisplayInfo>>>,
    operations: Arc<tokio::sync::Mutex<()>>,
}

impl ManagerService {
    pub fn new(state: ManagerState) -> Self {
        let snapshot = state.displays();
        Self {
            state: Arc::new(Mutex::new(state)),
            snapshot: Arc::new(RwLock::new(snapshot)),
            operations: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn mutate<T, F>(&self, operation: F) -> zbus::fdo::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut ManagerState) -> Result<T> + Send + 'static,
    {
        // Tokio's mutex is FIFO, so hardware requests execute in D-Bus arrival order.
        // The actual blocking call runs on the blocking pool after acquiring this gate.
        let _operation = Arc::clone(&self.operations).lock_owned().await;
        let state = Arc::clone(&self.state);
        let snapshot = Arc::clone(&self.snapshot);
        tokio::task::spawn_blocking(move || {
            let mut state = state
                .lock()
                .map_err(|_| anyhow::anyhow!("manager state lock is poisoned"))?;
            let result = operation(&mut state);
            let latest = state.displays();
            match snapshot.write() {
                Ok(mut current) => *current = latest,
                Err(poisoned) => *poisoned.into_inner() = latest,
            }
            result
        })
        .await
        .map_err(|error| dbus_error(anyhow::Error::new(error).context("hardware worker stopped")))?
        .map_err(dbus_error)
    }

    pub async fn shutdown(&self) {
        let _result = self
            .mutate(|state| {
                state.shutdown();
                Ok(())
            })
            .await;
    }
}

#[zbus::interface(name = "io.github.wlight.Manager1")]
impl ManagerService {
    fn list_displays(&self) -> zbus::fdo::Result<Vec<DisplayInfo>> {
        Ok(match self.snapshot.read() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        })
    }

    async fn refresh(&self) -> zbus::fdo::Result<Vec<DisplayInfo>> {
        self.mutate(ManagerState::refresh).await
    }

    async fn set_brightness(&self, id: &str, brightness: f64) -> zbus::fdo::Result<DisplayInfo> {
        validate_fraction(brightness, "brightness")?;
        let id = id.to_owned();
        self.mutate(move |state| state.set_brightness(&id, brightness))
            .await
    }

    async fn set_ddc_brightness(
        &self,
        id: &str,
        brightness: u16,
    ) -> zbus::fdo::Result<DisplayInfo> {
        if brightness > 100 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "DDC brightness must be between 0 and 100".to_owned(),
            ));
        }
        let id = id.to_owned();
        self.mutate(move |state| state.set_ddc_brightness(&id, brightness))
            .await
    }

    async fn set_gamma_brightness(
        &self,
        id: &str,
        brightness: f64,
    ) -> zbus::fdo::Result<DisplayInfo> {
        validate_fraction(brightness, "gamma brightness")?;
        let id = id.to_owned();
        self.mutate(move |state| state.set_gamma_brightness(&id, brightness))
            .await
    }
}

fn dbus_error(error: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!("{error:#}"))
}

fn validate_fraction(value: f64, name: &str) -> zbus::fdo::Result<()> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(zbus::fdo::Error::InvalidArgs(format!(
            "{name} must be finite and between 0.0 and 1.0"
        )))
    }
}

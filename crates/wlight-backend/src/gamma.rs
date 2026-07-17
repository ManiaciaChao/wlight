//! Wayland gamma-control backend.

use std::fs::File;
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use wayrs_client::global::{Global, GlobalExt};
use wayrs_client::protocol::wl_output::{self, WlOutput};
use wayrs_client::protocol::wl_registry;
use wayrs_client::{Connection, EventCtx, IoMode};
use wayrs_protocols::wlr_gamma_control_unstable_v1::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use wlight_core::gamma_table;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A Wayland output with an active gamma-control object.
#[derive(Debug, Clone, PartialEq)]
pub struct GammaOutput {
    /// Name advertised by `wl_output`, commonly a DRM connector such as `DP-1`.
    pub name: String,
    /// Number of entries in each of the red, green, and blue ramps.
    pub ramp_size: u32,
    /// Last brightness multiplier requested through this backend.
    pub brightness: f64,
}

enum Command {
    Set {
        name: String,
        brightness: f64,
        reply: Sender<std::result::Result<GammaOutput, String>>,
    },
    Shutdown,
}

/// Client handle for the dedicated Wayland gamma-control thread.
pub struct GammaBackend {
    commands: Sender<Command>,
    outputs: Arc<RwLock<Vec<GammaOutput>>>,
    failed_outputs: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl GammaBackend {
    /// Connect to the current Wayland compositor and acquire gamma controls.
    ///
    /// Failure to connect or absence of the wlr gamma-control global is returned
    /// as a regular error, allowing callers to run with DDC only.
    pub fn connect() -> Result<Self> {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (startup_tx, startup_rx) = bounded(1);
        let outputs = Arc::new(RwLock::new(Vec::new()));
        let failed_outputs = Arc::new(AtomicBool::new(false));
        let thread_outputs = Arc::clone(&outputs);
        let thread_failed_outputs = Arc::clone(&failed_outputs);
        let thread = thread::Builder::new()
            .name("wlight-wayland-gamma".to_owned())
            .spawn(move || {
                let failure_outputs = Arc::clone(&thread_outputs);
                if let Err(error) = run_wayland_thread(
                    command_rx,
                    thread_outputs,
                    thread_failed_outputs,
                    &startup_tx,
                ) {
                    replace_output_cache(&failure_outputs, Vec::new());
                    let message = format!("{error:#}");
                    let _ = startup_tx.try_send(Err(message.clone()));
                    tracing::debug!(%message, "Wayland gamma thread stopped");
                }
            })
            .context("failed to spawn Wayland gamma thread")?;

        match startup_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands: command_tx,
                outputs,
                failed_outputs,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(anyhow!(message))
            }
            Err(error) => {
                let _ = command_tx.send(Command::Shutdown);
                Err(anyhow!("Wayland gamma thread did not initialize: {error}"))
            }
        }
    }

    /// Return the latest output snapshot without Wayland I/O.
    #[must_use]
    pub fn outputs(&self) -> Vec<GammaOutput> {
        read_output_cache(&self.outputs)
    }

    /// Return whether the Wayland worker thread is still alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Return whether the worker or one of its per-output controls must be
    /// recreated. A compositor reports `failed` when another client already
    /// owns an output; reconnecting after that client exits is the only retry.
    #[must_use]
    pub fn needs_reconnect(&self) -> bool {
        !self.is_running() || self.failed_outputs.load(Ordering::Acquire)
    }

    /// Return the latest automatically maintained output snapshot.
    ///
    /// Hotplug is driven by registry events on the worker thread, so refresh
    /// itself never waits for a compositor roundtrip.
    #[must_use]
    pub fn refresh(&self) -> Vec<GammaOutput> {
        self.outputs()
    }

    /// Apply a software brightness multiplier to one named Wayland output.
    pub fn set_brightness(&self, name: &str, brightness: f64) -> Result<GammaOutput> {
        if !brightness.is_finite() || !(0.0..=1.0).contains(&brightness) {
            bail!("gamma brightness must be finite and between 0.0 and 1.0");
        }

        let (reply_tx, reply_rx) = bounded(1);
        self.commands
            .send(Command::Set {
                name: name.to_owned(),
                brightness,
                reply: reply_tx,
            })
            .context("Wayland gamma thread is not running")?;
        let result = reply_rx
            .recv_timeout(COMMAND_TIMEOUT)
            .context("timed out waiting for Wayland gamma control")?;
        result.map_err(anyhow::Error::msg)
    }

    /// Stop the worker and release all gamma controls, restoring normal ramps.
    pub fn shutdown(&mut self) -> Result<()> {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("Wayland gamma thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for GammaBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct ThreadState {
    manager: ZwlrGammaControlManagerV1,
    outputs: Vec<ManagedOutput>,
    cache: Arc<RwLock<Vec<GammaOutput>>>,
    failed_outputs: Arc<AtomicBool>,
    dirty: bool,
}

struct ManagedOutput {
    registry_name: u32,
    wl_output: WlOutput,
    gamma_control: ZwlrGammaControlV1,
    name: Option<String>,
    ramp_size: Option<u32>,
    brightness: f64,
    active: bool,
}

impl ManagedOutput {
    fn bind(
        conn: &mut Connection<ThreadState>,
        manager: ZwlrGammaControlManagerV1,
        global: &Global,
    ) -> Result<Self> {
        let wl_output = global
            .bind_with_cb(conn, 4, output_event)
            .context("wl_output version 4 is required for output names")?;
        let gamma_control = manager.get_gamma_control_with_cb(conn, wl_output, gamma_event);
        Ok(Self {
            registry_name: global.name,
            wl_output,
            gamma_control,
            name: None,
            ramp_size: None,
            brightness: 1.0,
            active: true,
        })
    }

    fn snapshot(&self) -> Option<GammaOutput> {
        if !self.active {
            return None;
        }
        Some(GammaOutput {
            name: self.name.clone()?,
            ramp_size: self.ramp_size?,
            brightness: self.brightness,
        })
    }

    fn destroy(self, conn: &mut Connection<ThreadState>) {
        if self.active {
            self.gamma_control.destroy(conn);
        }
        self.wl_output.release(conn);
    }
}

fn run_wayland_thread(
    commands: Receiver<Command>,
    cache: Arc<RwLock<Vec<GammaOutput>>>,
    failed_outputs: Arc<AtomicBool>,
    startup: &Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut conn = Connection::<ThreadState>::connect().context("failed to connect to Wayland")?;
    conn.blocking_roundtrip()
        .context("failed to collect Wayland globals")?;
    let manager = conn
        .bind_singleton::<ZwlrGammaControlManagerV1>(1)
        .context("compositor does not advertise wlr gamma control")?;
    let globals: Vec<Global> = conn
        .globals()
        .iter()
        .filter(|global| global.is::<WlOutput>())
        .cloned()
        .collect();
    let mut state = ThreadState {
        manager,
        outputs: Vec::new(),
        cache,
        failed_outputs,
        dirty: true,
    };

    // The first roundtrip's registry messages have already populated globals;
    // drain them before installing the hotplug callback to avoid duplicate binds.
    conn.dispatch_events(&mut state);
    for global in &globals {
        match ManagedOutput::bind(&mut conn, manager, global) {
            Ok(output) => state.outputs.push(output),
            Err(error) => tracing::debug!(%error, "skipping unnamed Wayland output"),
        }
    }
    conn.add_registry_cb(registry_event);
    conn.blocking_roundtrip()
        .context("failed to initialize Wayland gamma controls")?;
    conn.dispatch_events(&mut state);
    publish_outputs(&mut state);
    startup
        .send(Ok(()))
        .context("gamma backend was dropped during startup")?;

    let mut running = true;
    while running {
        match commands.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(command) => running = process_command(command, &mut conn, &mut state),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => running = false,
        }
        if !running {
            break;
        }
        pump_wayland(&mut conn, &mut state)?;
    }

    for output in state.outputs.drain(..) {
        output.destroy(&mut conn);
    }
    state.manager.destroy(&mut conn);
    conn.flush(IoMode::Blocking)
        .context("failed to release Wayland gamma controls")?;
    Ok(())
}

fn process_command(
    command: Command,
    conn: &mut Connection<ThreadState>,
    state: &mut ThreadState,
) -> bool {
    match command {
        Command::Set {
            name,
            brightness,
            reply,
        } => {
            let result =
                apply_gamma(conn, state, &name, brightness).map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
            true
        }
        Command::Shutdown => false,
    }
}

fn apply_gamma(
    conn: &mut Connection<ThreadState>,
    state: &mut ThreadState,
    name: &str,
    brightness: f64,
) -> Result<GammaOutput> {
    let output = state
        .outputs
        .iter_mut()
        .find(|output| output.active && output.name.as_deref() == Some(name))
        .with_context(|| format!("unknown or unsupported Wayland output {name}"))?;
    let size = output
        .ramp_size
        .with_context(|| format!("gamma ramp size is not available for {name}"))?;
    let table_size = usize::try_from(size).context("gamma ramp size does not fit in usize")?;
    let table = gamma_table(table_size, brightness).context("invalid gamma table")?;
    let file = gamma_file(&table)?;
    output.gamma_control.set_gamma(conn, file.into());
    conn.flush(IoMode::Blocking)
        .with_context(|| format!("failed to send gamma table for {name}"))?;
    output.brightness = brightness;
    let snapshot = output
        .snapshot()
        .with_context(|| format!("Wayland output {name} became unavailable"))?;
    state.dirty = true;
    publish_outputs(state);
    Ok(snapshot)
}

fn gamma_file(table: &[u16]) -> Result<File> {
    let mut file = tempfile::tempfile().context("failed to create gamma shared file")?;
    file.write_all(bytemuck::cast_slice(table))
        .context("failed to write gamma table")?;
    file.flush().context("failed to flush gamma table")?;
    file.seek(SeekFrom::Start(0))
        .context("failed to rewind gamma table")?;
    Ok(file)
}

fn pump_wayland(conn: &mut Connection<ThreadState>, state: &mut ThreadState) -> Result<()> {
    match conn.flush(IoMode::NonBlocking) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Err(error) => return Err(error).context("failed to flush Wayland requests"),
    }
    match conn.recv_events(IoMode::NonBlocking) {
        Ok(()) => conn.dispatch_events(state),
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Err(error) => return Err(error).context("failed to receive Wayland events"),
    }
    publish_outputs(state);
    Ok(())
}

fn output_event(ctx: EventCtx<ThreadState, WlOutput>) {
    if let wl_output::Event::Name(name) = ctx.event
        && let Some(output) = ctx
            .state
            .outputs
            .iter_mut()
            .find(|output| output.wl_output == ctx.proxy)
    {
        output.name = Some(name.to_string_lossy().into_owned());
        ctx.state.dirty = true;
    }
}

fn gamma_event(ctx: EventCtx<ThreadState, ZwlrGammaControlV1>) {
    let Some(output) = ctx
        .state
        .outputs
        .iter_mut()
        .find(|output| output.gamma_control == ctx.proxy)
    else {
        return;
    };
    match ctx.event {
        zwlr_gamma_control_v1::Event::GammaSize(size) => {
            output.ramp_size = Some(size);
            ctx.state.dirty = true;
        }
        zwlr_gamma_control_v1::Event::Failed => {
            let name = output.name.as_deref().unwrap_or("unknown output");
            tracing::debug!(%name, "Wayland gamma control was rejected or is already owned");
            ctx.proxy.destroy(ctx.conn);
            output.active = false;
            output.ramp_size = None;
            ctx.state.failed_outputs.store(true, Ordering::Release);
            ctx.state.dirty = true;
        }
        _ => {}
    }
}

fn registry_event(
    conn: &mut Connection<ThreadState>,
    state: &mut ThreadState,
    event: &wl_registry::Event,
) {
    match event {
        wl_registry::Event::Global(global) if global.is::<WlOutput>() => {
            match ManagedOutput::bind(conn, state.manager, global) {
                Ok(output) => {
                    state.outputs.push(output);
                    state.dirty = true;
                }
                Err(error) => tracing::debug!(%error, "skipping unnamed Wayland output"),
            }
        }
        wl_registry::Event::GlobalRemove(name) => {
            if let Some(index) = state
                .outputs
                .iter()
                .position(|output| output.registry_name == *name)
            {
                let output = state.outputs.swap_remove(index);
                output.destroy(conn);
                state.dirty = true;
            }
        }
        _ => {}
    }
}

fn publish_outputs(state: &mut ThreadState) {
    if !state.dirty {
        return;
    }
    let mut snapshot: Vec<_> = state
        .outputs
        .iter()
        .filter_map(ManagedOutput::snapshot)
        .collect();
    snapshot.sort_by(|left, right| left.name.cmp(&right.name));
    replace_output_cache(&state.cache, snapshot);
    state.dirty = false;
}

fn replace_output_cache(cache: &RwLock<Vec<GammaOutput>>, snapshot: Vec<GammaOutput>) {
    let mut cache = match cache.write() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    *cache = snapshot;
}

fn read_output_cache(cache: &RwLock<Vec<GammaOutput>>) -> Vec<GammaOutput> {
    match cache.read() {
        Ok(outputs) => outputs.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_file_has_exact_native_endian_bytes() {
        let table = [0x1234_u16, 0xabcd];
        let mut file = gamma_file(&table).expect("gamma file");
        let mut bytes = Vec::new();
        use std::io::Read;
        file.read_to_end(&mut bytes).expect("read gamma file");
        assert_eq!(
            bytes,
            [0x1234_u16.to_ne_bytes(), 0xabcd_u16.to_ne_bytes()].concat()
        );
    }
}

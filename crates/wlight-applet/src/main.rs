use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, unbounded};
use eframe::egui;
use wlight_dbus::{DisplayInfo, ManagerProxy};

const WRITE_DEBOUNCE: Duration = Duration::from_millis(180);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(12);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("io.github.wlight")
            .with_title("wlight")
            .with_inner_size([420.0, 520.0])
            .with_min_inner_size([340.0, 260.0]),
        ..Default::default()
    };

    eframe::run_native(
        "wlight",
        options,
        Box::new(|creation_context| Ok(Box::new(WlightApplet::new(creation_context)))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WriteKind {
    Unified,
    Ddc,
    Gamma,
}

#[derive(Debug, Clone, Copy)]
enum WriteValue {
    Unified(f64),
    Ddc(u16),
    Gamma(f64),
}

#[derive(Debug)]
enum WorkerCommand {
    Refresh {
        request_id: u64,
        rescan: bool,
    },
    Write {
        request_id: u64,
        display_id: String,
        value: WriteValue,
    },
    Shutdown,
}

impl WorkerCommand {
    fn request_context(&self) -> Option<(u64, Option<String>)> {
        match self {
            Self::Refresh { request_id, .. } => Some((*request_id, None)),
            Self::Write {
                request_id,
                display_id,
                ..
            } => Some((*request_id, Some(display_id.clone()))),
            Self::Shutdown => None,
        }
    }
}

#[derive(Debug)]
enum WorkerEvent {
    Snapshot {
        request_id: u64,
        displays: Vec<DisplayInfo>,
    },
    DisplayUpdated {
        request_id: u64,
        display: DisplayInfo,
    },
    Failed {
        request_id: u64,
        display_id: Option<String>,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingKey {
    display_id: String,
    kind: WriteKind,
}

#[derive(Debug, Clone, Copy)]
struct PendingWrite {
    request_id: u64,
    value: WriteValue,
    due_at: Instant,
}

impl PendingWrite {
    fn command(self, display_id: String) -> WorkerCommand {
        WorkerCommand::Write {
            request_id: self.request_id,
            display_id,
            value: self.value,
        }
    }
}

#[derive(Debug)]
struct DisplayDraft {
    info: DisplayInfo,
    effective_percent: f64,
    ddc_percent: u16,
    gamma_percent: f64,
    latest_request_id: u64,
}

impl DisplayDraft {
    fn new(info: DisplayInfo, request_id: u64) -> Self {
        let effective_percent = info.effective_percent();
        let ddc_percent = info.ddc_brightness.min(100);
        let gamma_percent = (info.gamma_brightness * 100.0).clamp(0.0, 100.0);
        Self {
            info,
            effective_percent,
            ddc_percent,
            gamma_percent,
            latest_request_id: request_id,
        }
    }

    fn synchronize(&mut self, info: DisplayInfo, request_id: u64) {
        self.effective_percent = info.effective_percent();
        self.ddc_percent = info.ddc_brightness.min(100);
        self.gamma_percent = (info.gamma_brightness * 100.0).clamp(0.0, 100.0);
        self.info = info;
        self.latest_request_id = request_id;
    }

    fn recompute_effective(&mut self) {
        let hardware = if self.info.ddc_supported {
            f64::from(self.ddc_percent) / 100.0
        } else {
            1.0
        };
        let gamma = if self.info.gamma_supported {
            self.gamma_percent / 100.0
        } else {
            1.0
        };
        self.effective_percent = (hardware * gamma * 100.0).clamp(0.0, 100.0);
    }
}

struct WlightApplet {
    command_tx: Sender<WorkerCommand>,
    event_rx: Receiver<WorkerEvent>,
    displays: BTreeMap<String, DisplayDraft>,
    display_order: Vec<String>,
    pending_writes: BTreeMap<PendingKey, PendingWrite>,
    in_flight: HashMap<u64, String>,
    next_request_id: u64,
    refresh_request_id: Option<u64>,
    loading: bool,
    worker_running: bool,
    last_error: Option<String>,
}

impl WlightApplet {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let (command_tx, command_rx) = bounded(64);
        let (event_tx, event_rx) = unbounded();
        let repaint_context = creation_context.egui_ctx.clone();

        let startup_error = thread::Builder::new()
            .name("wlight-dbus".to_owned())
            .spawn(move || worker_entry(command_rx, event_tx, repaint_context))
            .err()
            .map(|error| format!("Could not start the D-Bus worker: {error}"));

        Self {
            command_tx,
            event_rx,
            displays: BTreeMap::new(),
            display_order: Vec::new(),
            pending_writes: BTreeMap::new(),
            in_flight: HashMap::new(),
            next_request_id: 1,
            refresh_request_id: None,
            loading: startup_error.is_none(),
            worker_running: startup_error.is_none(),
            last_error: startup_error,
        }
    }

    fn allocate_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        request_id
    }

    fn drain_worker_events(&mut self) {
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => self.apply_worker_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.worker_running {
                        self.worker_running = false;
                        self.loading = false;
                        self.refresh_request_id = None;
                        self.pending_writes.clear();
                        self.in_flight.clear();
                        self.last_error = Some("The D-Bus worker stopped unexpectedly.".to_owned());
                    }
                    break;
                }
            }
        }
    }

    fn apply_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Snapshot {
                request_id,
                displays,
            } => {
                self.loading = false;
                if self.refresh_request_id == Some(request_id) {
                    self.refresh_request_id = None;
                }
                self.last_error = None;
                self.apply_snapshot(request_id, displays);
            }
            WorkerEvent::DisplayUpdated {
                request_id,
                display,
            } => {
                self.in_flight.remove(&request_id);
                let display_id = display.id.clone();
                match self.displays.get_mut(&display_id) {
                    Some(draft) if request_id >= draft.latest_request_id => {
                        draft.synchronize(display, request_id);
                    }
                    Some(_) => {}
                    None => {
                        self.displays
                            .insert(display_id, DisplayDraft::new(display, request_id));
                        self.rebuild_display_order();
                    }
                }
                self.last_error = None;
            }
            WorkerEvent::Failed {
                request_id,
                display_id,
                message,
            } => {
                self.loading = false;
                self.in_flight.remove(&request_id);
                if self.refresh_request_id == Some(request_id) {
                    self.refresh_request_id = None;
                }
                if let Some(display_id) = display_id
                    && let Some(draft) = self.displays.get_mut(&display_id)
                    && request_id >= draft.latest_request_id
                {
                    draft.info.last_error = message.clone();
                }
                self.last_error = Some(message);
            }
        }
    }

    fn apply_snapshot(&mut self, request_id: u64, displays: Vec<DisplayInfo>) {
        let mut seen = HashSet::with_capacity(displays.len());
        for display in displays {
            let display_id = display.id.clone();
            seen.insert(display_id.clone());
            match self.displays.get_mut(&display_id) {
                Some(draft) if request_id >= draft.latest_request_id => {
                    draft.synchronize(display, request_id);
                }
                Some(_) => {}
                None => {
                    self.displays
                        .insert(display_id, DisplayDraft::new(display, request_id));
                }
            }
        }

        let active_displays: HashSet<&str> = self
            .pending_writes
            .keys()
            .map(|key| key.display_id.as_str())
            .chain(self.in_flight.values().map(String::as_str))
            .collect();
        self.displays.retain(|display_id, draft| {
            seen.contains(display_id)
                || active_displays.contains(display_id.as_str())
                || draft.latest_request_id > request_id
        });
        self.rebuild_display_order();
    }

    fn rebuild_display_order(&mut self) {
        self.display_order = self.displays.keys().cloned().collect();
        self.display_order.sort_by(|left, right| {
            let left_name = self
                .displays
                .get(left)
                .map_or(left.as_str(), |draft| draft.info.name.as_str());
            let right_name = self
                .displays
                .get(right)
                .map_or(right.as_str(), |draft| draft.info.name.as_str());
            left_name
                .to_lowercase()
                .cmp(&right_name.to_lowercase())
                .then_with(|| left.cmp(right))
        });
    }

    fn queue_write(
        &mut self,
        display_id: &str,
        kind: WriteKind,
        value: WriteValue,
        context: &egui::Context,
    ) {
        if !self.worker_running {
            return;
        }

        let now = Instant::now();
        for (key, pending) in &mut self.pending_writes {
            if key.display_id == display_id && key.kind != kind {
                pending.due_at = now;
            }
        }

        let request_id = self.allocate_request_id();
        let key = PendingKey {
            display_id: display_id.to_owned(),
            kind,
        };
        self.pending_writes.insert(
            key,
            PendingWrite {
                request_id,
                value,
                due_at: now + WRITE_DEBOUNCE,
            },
        );
        if let Some(draft) = self.displays.get_mut(display_id) {
            draft.latest_request_id = request_id;
        }

        context.request_repaint_after(WRITE_DEBOUNCE);
    }

    fn flush_pending_writes(&mut self, context: &egui::Context) {
        if !self.worker_running || self.pending_writes.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut due: Vec<(PendingKey, u64)> = self
            .pending_writes
            .iter()
            .filter(|(_, pending)| pending.due_at <= now)
            .map(|(key, pending)| (key.clone(), pending.request_id))
            .collect();
        due.sort_by_key(|(_, request_id)| *request_id);

        for (key, _) in due {
            let Some(pending) = self.pending_writes.remove(&key) else {
                continue;
            };
            let command = pending.command(key.display_id.clone());
            match self.command_tx.try_send(command) {
                Ok(()) => {
                    self.in_flight
                        .insert(pending.request_id, key.display_id.clone());
                }
                Err(TrySendError::Full(_)) => {
                    self.pending_writes.insert(
                        key,
                        PendingWrite {
                            due_at: now + Duration::from_millis(40),
                            ..pending
                        },
                    );
                    context.request_repaint_after(Duration::from_millis(40));
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.worker_running = false;
                    self.pending_writes.clear();
                    self.in_flight.clear();
                    self.last_error = Some("The D-Bus worker is no longer available.".to_owned());
                    break;
                }
            }
        }

        if let Some(next_due) = self
            .pending_writes
            .values()
            .map(|pending| pending.due_at)
            .min()
        {
            context.request_repaint_after(next_due.saturating_duration_since(Instant::now()));
        }
    }

    fn request_refresh(&mut self) {
        if !self.worker_running || self.refresh_request_id.is_some() {
            return;
        }

        let request_id = self.allocate_request_id();
        let command = WorkerCommand::Refresh {
            request_id,
            rescan: true,
        };
        match self.command_tx.try_send(command) {
            Ok(()) => {
                self.refresh_request_id = Some(request_id);
                self.last_error = None;
            }
            Err(TrySendError::Full(_)) => {
                self.last_error =
                    Some("The D-Bus worker is busy; try refreshing again.".to_owned());
            }
            Err(TrySendError::Disconnected(_)) => {
                self.worker_running = false;
                self.last_error = Some("The D-Bus worker is no longer available.".to_owned());
            }
        }
    }

    fn display_is_busy(&self, display_id: &str) -> bool {
        self.pending_writes
            .keys()
            .any(|key| key.display_id == display_id)
            || self
                .in_flight
                .values()
                .any(|in_flight_id| in_flight_id == display_id)
    }

    fn render_display(
        ui: &mut egui::Ui,
        draft: &mut DisplayDraft,
        busy: bool,
        worker_running: bool,
    ) -> Vec<(WriteKind, WriteValue)> {
        let mut writes = Vec::new();
        let display_id = draft.info.id.clone();
        let display_name = if draft.info.name.trim().is_empty() {
            display_id.clone()
        } else {
            draft.info.name.clone()
        };

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.strong(&display_name);
                if busy {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });

            if !draft.info.connector.is_empty() {
                ui.label(egui::RichText::new(&draft.info.connector).small().weak())
                    .on_hover_text(format!("Display ID: {display_id}"));
            }

            ui.add_space(5.0);
            let supported = draft.info.ddc_supported || draft.info.gamma_supported;
            let brightness_label = ui
                .horizontal(|ui| {
                    let label = ui.label("Brightness");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("{:.0}%", draft.effective_percent));
                    });
                    label
                })
                .inner;
            let slider_width = ui.available_width();
            let unified_response = ui
                .scope(|ui| {
                    ui.spacing_mut().slider_width = slider_width;
                    ui.add_enabled(
                        supported && worker_running,
                        egui::Slider::new(&mut draft.effective_percent, 0.0..=100.0)
                            .show_value(false)
                            .trailing_fill(true)
                            .integer(),
                    )
                })
                .inner
                .labelled_by(brightness_label.id);
            if unified_response.changed() {
                writes.push((
                    WriteKind::Unified,
                    WriteValue::Unified((draft.effective_percent / 100.0).clamp(0.0, 1.0)),
                ));
            }
            unified_response.on_hover_text(
                "Unified control uses DDC first, then gamma below the hardware floor.",
            );
            if !supported {
                ui.label(egui::RichText::new("No brightness control available").weak());
            }

            egui::CollapsingHeader::new("Advanced")
                .id_salt(("advanced", &display_id))
                .show(ui, |ui| {
                    if draft.info.ddc_supported {
                        let ddc_response = ui.add_enabled(
                            worker_running,
                            egui::Slider::new(&mut draft.ddc_percent, 0..=100)
                                .text("DDC")
                                .suffix("%"),
                        );
                        if ddc_response.changed() {
                            draft.recompute_effective();
                            writes.push((WriteKind::Ddc, WriteValue::Ddc(draft.ddc_percent)));
                        }
                        ddc_response.on_hover_text("Hardware backlight level reported by DDC/CI.");
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("DDC");
                            ui.label(egui::RichText::new("Unavailable").weak());
                        });
                    }

                    if draft.info.gamma_supported {
                        let gamma_response = ui.add_enabled(
                            worker_running,
                            egui::Slider::new(&mut draft.gamma_percent, 0.0..=100.0)
                                .text("Gamma")
                                .suffix("%")
                                .integer(),
                        );
                        if gamma_response.changed() {
                            draft.recompute_effective();
                            writes.push((
                                WriteKind::Gamma,
                                WriteValue::Gamma((draft.gamma_percent / 100.0).clamp(0.0, 1.0)),
                            ));
                        }
                        gamma_response.on_hover_text(
                            "Software attenuation through the Wayland gamma lookup table.",
                        );
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Gamma");
                            ui.label(egui::RichText::new("Unavailable").weak());
                        });
                    }
                });

            if !draft.info.last_error.is_empty() {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, &draft.info.last_error);
            }
        });
        writes
    }
}

impl eframe::App for WlightApplet {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_worker_events();
        self.flush_pending_writes(context);
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("wlight");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let refresh_enabled = self.worker_running
                        && self.refresh_request_id.is_none()
                        && self.pending_writes.is_empty();
                    if ui
                        .add_enabled(refresh_enabled, egui::Button::new("Refresh"))
                        .clicked()
                    {
                        self.request_refresh();
                    }
                    if self.loading || self.refresh_request_id.is_some() {
                        ui.add(egui::Spinner::new());
                    }
                });
            });
            ui.label(
                egui::RichText::new("Per-display DDC and Wayland gamma control")
                    .small()
                    .weak(),
            );

            if let Some(error) = &self.last_error {
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                });
            }

            ui.add_space(8.0);
            if self.loading && self.displays.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.add(egui::Spinner::new());
                    ui.label("Connecting to the wlight daemon…");
                });
            } else if self.displays.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label("No displays found.");
                    ui.label(
                        egui::RichText::new("Start the daemon, then refresh the display list.")
                            .small()
                            .weak(),
                    );
                });
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for display_id in self.display_order.clone() {
                            let busy = self.display_is_busy(&display_id);
                            let writes = self.displays.get_mut(&display_id).map(|draft| {
                                Self::render_display(ui, draft, busy, self.worker_running)
                            });
                            if let Some(writes) = writes {
                                for (kind, value) in writes {
                                    self.queue_write(&display_id, kind, value, ui.ctx());
                                }
                            }
                            ui.add_space(8.0);
                        }
                    });
            }
        });
    }
}

impl Drop for WlightApplet {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(WorkerCommand::Shutdown);
    }
}

fn worker_entry(
    command_rx: Receiver<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
    repaint_context: egui::Context,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = event_tx.send(WorkerEvent::Failed {
                request_id: 0,
                display_id: None,
                message: format!("Could not create the D-Bus runtime: {error}"),
            });
            repaint_context.request_repaint();
            return;
        }
    };

    runtime.block_on(worker_loop(command_rx, event_tx, repaint_context));
}

async fn worker_loop(
    command_rx: Receiver<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
    repaint_context: egui::Context,
) {
    let mut connection = None;
    if !execute_worker_command(
        &WorkerCommand::Refresh {
            request_id: 0,
            rescan: false,
        },
        &mut connection,
        &event_tx,
        &repaint_context,
    )
    .await
    {
        return;
    }

    let mut poll = tokio::time::interval(WORKER_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        loop {
            match command_rx.try_recv() {
                Ok(WorkerCommand::Shutdown) => return,
                Ok(command) => {
                    if !execute_worker_command(
                        &command,
                        &mut connection,
                        &event_tx,
                        &repaint_context,
                    )
                    .await
                    {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

async fn execute_worker_command(
    command: &WorkerCommand,
    connection: &mut Option<zbus::Connection>,
    event_tx: &Sender<WorkerEvent>,
    repaint_context: &egui::Context,
) -> bool {
    let Some((request_id, display_id)) = command.request_context() else {
        return true;
    };

    let result = call_daemon(command, connection).await;
    let event = match result {
        Ok(event) => event,
        Err(message) => {
            *connection = None;
            WorkerEvent::Failed {
                request_id,
                display_id,
                message,
            }
        }
    };
    let sent = event_tx.send(event).is_ok();
    repaint_context.request_repaint();
    sent
}

async fn call_daemon(
    command: &WorkerCommand,
    connection: &mut Option<zbus::Connection>,
) -> Result<WorkerEvent, String> {
    if connection.is_none() {
        let session = zbus::Connection::session()
            .await
            .map_err(|error| format!("Could not connect to the session D-Bus: {error}"))?;
        *connection = Some(session);
    }

    let Some(connection) = connection.as_ref() else {
        return Err("The session D-Bus connection is unavailable.".to_owned());
    };
    let proxy = ManagerProxy::new(connection)
        .await
        .map_err(|error| format!("Could not create the wlight D-Bus proxy: {error}"))?;

    match command {
        WorkerCommand::Refresh { request_id, rescan } => {
            let displays = if *rescan {
                proxy
                    .refresh()
                    .await
                    .map_err(|error| format!("Could not refresh displays: {error}"))?
            } else {
                proxy
                    .list_displays()
                    .await
                    .map_err(|error| format!("Could not list displays: {error}"))?
            };
            Ok(WorkerEvent::Snapshot {
                request_id: *request_id,
                displays,
            })
        }
        WorkerCommand::Write {
            request_id,
            display_id,
            value,
        } => {
            let display = match value {
                WriteValue::Unified(brightness) => proxy
                    .set_brightness(display_id, *brightness)
                    .await
                    .map_err(|error| {
                        format!("Could not set brightness for {display_id}: {error}")
                    })?,
                WriteValue::Ddc(brightness) => proxy
                    .set_ddc_brightness(display_id, *brightness)
                    .await
                    .map_err(|error| {
                        format!("Could not set DDC brightness for {display_id}: {error}")
                    })?,
                WriteValue::Gamma(brightness) => proxy
                    .set_gamma_brightness(display_id, *brightness)
                    .await
                    .map_err(|error| {
                        format!("Could not set gamma brightness for {display_id}: {error}")
                    })?,
            };
            Ok(WorkerEvent::DisplayUpdated {
                request_id: *request_id,
                display,
            })
        }
        WorkerCommand::Shutdown => Err("The D-Bus worker is shutting down.".to_owned()),
    }
}

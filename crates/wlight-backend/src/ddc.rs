//! DDC/CI hardware brightness backend.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ddc_hi::{Ddc, Display, VcpValue};

const BRIGHTNESS_VCP_CODE: u8 = 0x10;
const DRM_CLASS_PATH: &str = "/sys/class/drm";
const DDC_ATTEMPTS: usize = 3;
const DDC_RETRY_DELAY: Duration = Duration::from_millis(50);

/// A snapshot of a monitor controlled through DDC/CI.
///
/// `current` and `maximum` are the raw values reported by VCP feature `0x10`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdcDisplay {
    /// Stable identifier derived from the monitor EDID.
    ///
    /// Identical EDIDs on different physical outputs receive a connector or I²C
    /// suffix. Multiple transport handles for one output are hidden as aliases.
    pub id: String,
    /// Human-readable monitor model, when the EDID supplies one.
    pub name: String,
    /// Wayland/DRM connector name, for example `DP-1`.
    pub connector: String,
    /// Linux I²C adapter number, when it can be determined from sysfs.
    pub i2c_bus: Option<u32>,
    /// Current raw VCP value.
    pub current: u16,
    /// Maximum raw VCP value.
    pub maximum: u16,
}

impl DdcDisplay {
    /// Return the raw VCP value as a rounded percentage.
    #[must_use]
    pub fn percent(&self) -> u16 {
        if self.maximum == 0 {
            return 0;
        }

        let current = u32::from(self.current.min(self.maximum));
        let maximum = u32::from(self.maximum);
        ((current * 100 + maximum / 2) / maximum) as u16
    }
}

struct ManagedHandle {
    display: Display,
    i2c_bus: Option<u32>,
    maximum: u16,
}

struct ManagedDisplay {
    handles: Vec<ManagedHandle>,
    active: usize,
    snapshot: DdcDisplay,
}

struct DdcCandidate {
    handle: ManagedHandle,
    fingerprint: blake3::Hash,
    connector: Option<String>,
    preference: u8,
    name: String,
    current: u16,
}

/// Persistent collection of open DDC handles.
///
/// Enumeration is deliberately tolerant: a machine without I²C devices,
/// permissions, or DDC-capable monitors produces an empty backend.
pub struct DdcBackend {
    displays: Vec<ManagedDisplay>,
}

impl DdcBackend {
    /// Enumerate DDC-capable monitors and keep their handles open.
    pub fn enumerate() -> Result<Self> {
        Self::enumerate_from(Path::new(DRM_CLASS_PATH))
    }

    /// Return the most recent snapshot without performing I/O.
    #[must_use]
    pub fn displays(&self) -> Vec<DdcDisplay> {
        self.displays
            .iter()
            .map(|managed| managed.snapshot.clone())
            .collect()
    }

    /// Re-enumerate monitors, replacing the currently open handles.
    pub fn refresh(&mut self) -> Result<Vec<DdcDisplay>> {
        *self = Self::enumerate()?;
        Ok(self.displays())
    }

    /// Re-read VCP feature `0x10` and update the cached snapshot.
    pub fn get_brightness(&mut self, id: &str) -> Result<DdcDisplay> {
        let managed = self.find_mut(id)?;
        let mut failures = Vec::new();
        for index in handle_order(managed.active, managed.handles.len()) {
            let bus = managed.handles[index].i2c_bus;
            match get_vcp_with_retry(&mut managed.handles[index].display) {
                Ok(value) => {
                    let current = u16::from_be_bytes([value.sh, value.sl]);
                    let maximum = u16::from_be_bytes([value.mh, value.ml]);
                    if maximum == 0 {
                        failures.push(format!("{} reported maximum 0", bus_label(bus)));
                        continue;
                    }
                    managed.handles[index].maximum = maximum;
                    managed.active = index;
                    managed.snapshot.i2c_bus = bus;
                    managed.snapshot.current = current;
                    managed.snapshot.maximum = maximum;
                    return Ok(managed.snapshot.clone());
                }
                Err(error) => failures.push(format!("{}: {error:#}", bus_label(bus))),
            }
        }
        Err(anyhow!(
            "failed to read DDC brightness for {id}: {}",
            failures.join("; ")
        ))
    }

    /// Set brightness as a percentage in the inclusive range `0..=100`.
    ///
    /// The percentage is scaled to the monitor's raw VCP maximum.
    pub fn set_brightness(&mut self, id: &str, percent: u16) -> Result<DdcDisplay> {
        if percent > 100 {
            bail!("DDC brightness must be between 0 and 100, got {percent}");
        }

        let managed = self.find_mut(id)?;
        let mut failures = Vec::new();
        for index in handle_order(managed.active, managed.handles.len()) {
            let bus = managed.handles[index].i2c_bus;
            let maximum = managed.handles[index].maximum;
            if maximum == 0 {
                failures.push(format!("{} reported maximum 0", bus_label(bus)));
                continue;
            }
            let scaled = (u32::from(percent) * u32::from(maximum) + 50) / 100;
            let value = u16::try_from(scaled).context("scaled DDC value does not fit in u16")?;
            match set_vcp_with_retry(&mut managed.handles[index].display, value) {
                Ok(()) => {
                    managed.active = index;
                    managed.snapshot.i2c_bus = bus;
                    managed.snapshot.current = value;
                    managed.snapshot.maximum = maximum;
                    return Ok(managed.snapshot.clone());
                }
                Err(error) => failures.push(format!("{}: {error:#}", bus_label(bus))),
            }
        }
        Err(anyhow!(
            "failed to set DDC brightness for {id}: {}",
            failures.join("; ")
        ))
    }

    fn enumerate_from(drm_root: &Path) -> Result<Self> {
        let connectors = scan_drm_connectors(drm_root);
        let mut candidates = Vec::new();

        for mut display in Display::enumerate() {
            let Some(edid) = display.info.edid_data.as_deref() else {
                continue;
            };
            let Some(fingerprint) = edid_fingerprint(edid) else {
                continue;
            };
            let i2c_bus = i2c_bus_from_ddc_id(&display.info.id);
            let connector = match_connector(&connectors, fingerprint, i2c_bus);
            let value = match get_vcp_with_retry(&mut display) {
                Ok(value) => value,
                Err(error) => {
                    let monitor_info = &display.info;
                    tracing::debug!(
                        monitor = %monitor_info,
                        %error,
                        "monitor does not expose readable DDC brightness"
                    );
                    continue;
                }
            };
            let current = u16::from_be_bytes([value.sh, value.sl]);
            let maximum = u16::from_be_bytes([value.mh, value.ml]);
            if maximum == 0 {
                let monitor_info = &display.info;
                tracing::debug!(monitor = %monitor_info, "monitor reports a zero brightness maximum");
                continue;
            }

            let base_id = format!("edid-{fingerprint}");
            let name = display_name(&display.info, &base_id);
            candidates.push(DdcCandidate {
                handle: ManagedHandle {
                    display,
                    i2c_bus,
                    maximum,
                },
                fingerprint,
                connector: connector.map(|item| item.name.clone()),
                preference: handle_preference(&connectors, connector, i2c_bus),
                name,
                current,
            });
        }

        let displays = group_candidates(candidates);
        Ok(Self { displays })
    }

    fn find_mut(&mut self, id: &str) -> Result<&mut ManagedDisplay> {
        self.displays
            .iter_mut()
            .find(|managed| managed.snapshot.id == id)
            .with_context(|| format!("unknown DDC display {id}"))
    }
}

fn group_candidates(mut candidates: Vec<DdcCandidate>) -> Vec<ManagedDisplay> {
    candidates.sort_by(|left, right| {
        left.connector
            .cmp(&right.connector)
            .then_with(|| {
                left.fingerprint
                    .as_bytes()
                    .cmp(right.fingerprint.as_bytes())
            })
            .then_with(|| left.preference.cmp(&right.preference))
            .then_with(|| left.handle.i2c_bus.cmp(&right.handle.i2c_bus))
    });

    let mut groups: Vec<Vec<DdcCandidate>> = Vec::new();
    let mut group_indices: HashMap<String, usize> = HashMap::new();
    for candidate in candidates {
        let key = physical_group_key(&candidate);
        if let Some(index) = group_indices.get(&key).copied() {
            groups[index].push(candidate);
        } else {
            group_indices.insert(key, groups.len());
            groups.push(vec![candidate]);
        }
    }

    let mut fingerprint_counts: HashMap<String, usize> = HashMap::new();
    for group in &groups {
        let fingerprint = group[0].fingerprint.to_string();
        *fingerprint_counts.entry(fingerprint).or_default() += 1;
    }

    let mut used_ids = HashSet::new();
    let mut displays = Vec::with_capacity(groups.len());
    for mut group in groups {
        group.sort_by(|left, right| {
            left.preference
                .cmp(&right.preference)
                .then_with(|| left.handle.i2c_bus.cmp(&right.handle.i2c_bus))
        });
        let primary = &group[0];
        let fingerprint = primary.fingerprint.to_string();
        let duplicate = fingerprint_counts.get(&fingerprint).copied().unwrap_or(0) > 1;
        let connector = primary.connector.clone().unwrap_or_default();
        let primary_bus = primary.handle.i2c_bus;
        let mut id = format!("edid-{fingerprint}");
        if duplicate {
            id.push('@');
            let suffix = primary
                .connector
                .as_deref()
                .map(sanitize_id_component)
                .unwrap_or_else(|| bus_label(primary_bus));
            id.push_str(&suffix);
        }
        id = ensure_unique_id(id, &mut used_ids);

        if group.len() > 1 {
            let aliases: Vec<_> = group
                .iter()
                .skip(1)
                .map(|candidate| bus_label(candidate.handle.i2c_bus))
                .collect();
            tracing::debug!(
                display = %primary.name,
                connector = %connector,
                active_bus = %bus_label(primary_bus),
                aliases = %aliases.join(","),
                "collapsed duplicate DDC transport handles"
            );
        }

        let snapshot = DdcDisplay {
            id,
            name: primary.name.clone(),
            connector,
            i2c_bus: primary_bus,
            current: primary.current,
            maximum: primary.handle.maximum,
        };
        let handles = group
            .into_iter()
            .map(|candidate| candidate.handle)
            .collect();
        displays.push(ManagedDisplay {
            handles,
            active: 0,
            snapshot,
        });
    }
    displays.sort_by(|left, right| {
        left.snapshot
            .connector
            .cmp(&right.snapshot.connector)
            .then_with(|| left.snapshot.id.cmp(&right.snapshot.id))
    });
    displays
}

fn physical_group_key(candidate: &DdcCandidate) -> String {
    match &candidate.connector {
        Some(connector) => format!("connector:{connector}:{}", candidate.fingerprint),
        None => format!(
            "bus:{}:{}",
            candidate
                .handle
                .i2c_bus
                .map_or_else(|| "unknown".to_owned(), |bus| bus.to_string()),
            candidate.fingerprint
        ),
    }
}

fn sanitize_id_component(component: &str) -> String {
    component.replace(|character: char| !character.is_ascii_alphanumeric(), "-")
}

fn ensure_unique_id(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    for sequence in 2_u32.. {
        let candidate = format!("{base}-{sequence}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("an unbounded sequence always contains an unused display id")
}

fn handle_order(active: usize, count: usize) -> impl Iterator<Item = usize> {
    (0..count).map(move |offset| (active + offset) % count)
}

fn bus_label(bus: Option<u32>) -> String {
    bus.map_or_else(|| "unknown-bus".to_owned(), |bus| format!("i2c-{bus}"))
}

fn get_vcp_with_retry(display: &mut Display) -> Result<VcpValue> {
    let mut last_error = None;
    for attempt in 0..DDC_ATTEMPTS {
        match display.handle.get_vcp_feature(BRIGHTNESS_VCP_CODE) {
            Ok(value) => return Ok(value),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < DDC_ATTEMPTS {
            thread::sleep(DDC_RETRY_DELAY);
        }
    }
    Err(anyhow!(
        "DDC VCP read failed after {DDC_ATTEMPTS} attempts: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_owned())
    ))
}

fn set_vcp_with_retry(display: &mut Display, value: u16) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..DDC_ATTEMPTS {
        match display.handle.set_vcp_feature(BRIGHTNESS_VCP_CODE, value) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < DDC_ATTEMPTS {
            thread::sleep(DDC_RETRY_DELAY);
        }
    }
    Err(anyhow!(
        "DDC VCP write failed after {DDC_ATTEMPTS} attempts: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_owned())
    ))
}

fn display_name(info: &ddc_hi::DisplayInfo, id: &str) -> String {
    if let Some(name) = info
        .model_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_owned();
    }

    match (&info.manufacturer_id, info.model_id) {
        (Some(manufacturer), Some(model)) => format!("{manufacturer} {model:04x}"),
        (Some(manufacturer), None) => manufacturer.clone(),
        (None, Some(model)) => format!("Display {model:04x}"),
        (None, None) => format!("Display {}", id.get(..13).unwrap_or(id)),
    }
}

#[derive(Debug)]
struct DrmConnector {
    name: String,
    connected: bool,
    fingerprint: Option<blake3::Hash>,
    direct_buses: HashSet<u32>,
    ddc_buses: HashSet<u32>,
}

impl DrmConnector {
    fn contains_bus(&self, bus: u32) -> bool {
        self.direct_buses.contains(&bus) || self.ddc_buses.contains(&bus)
    }
}

fn scan_drm_connectors(root: &Path) -> Vec<DrmConnector> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let sysfs_name = entry.file_name().to_string_lossy().into_owned();
            let name = drm_connector_name(&sysfs_name)?;
            let path = entry.path();
            let connected = fs::read_to_string(path.join("status"))
                .is_ok_and(|status| status.trim() == "connected");
            let fingerprint = fs::read(path.join("edid"))
                .ok()
                .filter(|edid| !edid.is_empty())
                .and_then(|edid| edid_fingerprint(&edid));
            let (direct_buses, ddc_buses) = find_i2c_buses(&path);
            Some(DrmConnector {
                name,
                connected,
                fingerprint,
                direct_buses,
                ddc_buses,
            })
        })
        .collect()
}

fn edid_fingerprint(edid: &[u8]) -> Option<blake3::Hash> {
    const BASE_BLOCK_SIZE: usize = 128;
    (edid.len() >= BASE_BLOCK_SIZE).then(|| blake3::hash(&edid[..BASE_BLOCK_SIZE]))
}

fn drm_connector_name(sysfs_name: &str) -> Option<String> {
    let after_card = sysfs_name.strip_prefix("card")?;
    let (_, connector) = after_card.split_once('-')?;
    if connector.is_empty() {
        None
    } else {
        Some(connector.to_owned())
    }
}

fn find_i2c_buses(connector: &Path) -> (HashSet<u32>, HashSet<u32>) {
    let direct_buses = i2c_buses_in_directory(connector);
    let ddc = connector.join("ddc");
    let mut ddc_buses = HashSet::new();
    if let Ok(target) = fs::canonicalize(&ddc)
        && let Some(bus) = i2c_bus_in_path(&target)
    {
        ddc_buses.insert(bus);
    }
    ddc_buses.extend(i2c_buses_in_directory(&ddc));
    ddc_buses.extend(i2c_buses_in_directory(&ddc.join("i2c-dev")));
    (direct_buses, ddc_buses)
}

fn i2c_buses_in_directory(directory: &Path) -> HashSet<u32> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| parse_i2c_component(&entry.file_name().to_string_lossy()))
        .collect()
}

fn i2c_bus_in_path(path: &Path) -> Option<u32> {
    path.components()
        .rev()
        .find_map(|component| component.as_os_str().to_str().and_then(parse_i2c_component))
}

fn parse_i2c_component(component: &str) -> Option<u32> {
    component.strip_prefix("i2c-")?.parse().ok()
}

fn i2c_bus_from_ddc_id(id: &str) -> Option<u32> {
    // ddc-hi 0.4.x identifies Linux devices with `MetadataExt::rdev()`.
    // This is the libc `minor()` layout expressed with safe integer operations.
    let device = id.parse::<u64>().ok()?;
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    u32::try_from(minor).ok()
}

fn match_connector(
    connectors: &[DrmConnector],
    fingerprint: blake3::Hash,
    i2c_bus: Option<u32>,
) -> Option<&DrmConnector> {
    let exact = connectors.iter().filter(|connector| {
        connector.connected
            && connector.fingerprint == Some(fingerprint)
            && i2c_bus.is_some_and(|bus| connector.contains_bus(bus))
    });
    if let Some(connector) = exactly_one(exact) {
        return Some(connector);
    }

    let edid_matches = connectors
        .iter()
        .filter(|connector| connector.connected && connector.fingerprint == Some(fingerprint));
    if let Some(connector) = exactly_one(edid_matches) {
        return Some(connector);
    }

    let bus_matches = connectors.iter().filter(|connector| {
        connector.connected
            && connector.fingerprint.is_none()
            && i2c_bus.is_some_and(|bus| connector.contains_bus(bus))
    });
    exactly_one(bus_matches)
}

fn exactly_one<'a>(
    mut matches: impl Iterator<Item = &'a DrmConnector>,
) -> Option<&'a DrmConnector> {
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn handle_preference(
    connectors: &[DrmConnector],
    matched: Option<&DrmConnector>,
    i2c_bus: Option<u32>,
) -> u8 {
    let Some(bus) = i2c_bus else {
        return 5;
    };
    if matched.is_some_and(|connector| connector.direct_buses.contains(&bus)) {
        return 0;
    }
    if matched.is_some_and(|connector| connector.ddc_buses.contains(&bus)) {
        return 1;
    }
    if connectors.iter().any(|connector| {
        matched.is_none_or(|matched| connector.name != matched.name) && connector.contains_bus(bus)
    }) {
        return 3;
    }
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(
        name: &str,
        connected: bool,
        fingerprint: Option<blake3::Hash>,
        direct_buses: &[u32],
        ddc_buses: &[u32],
    ) -> DrmConnector {
        DrmConnector {
            name: name.to_owned(),
            connected,
            fingerprint,
            direct_buses: direct_buses.iter().copied().collect(),
            ddc_buses: ddc_buses.iter().copied().collect(),
        }
    }

    #[test]
    fn extracts_connector_name() {
        assert_eq!(drm_connector_name("card0-DP-3").as_deref(), Some("DP-3"));
        assert_eq!(drm_connector_name("card12-eDP-1").as_deref(), Some("eDP-1"));
        assert_eq!(drm_connector_name("card0"), None);
        assert_eq!(drm_connector_name("renderD128"), None);
    }

    #[test]
    fn extracts_i2c_bus() {
        assert_eq!(parse_i2c_component("i2c-17"), Some(17));
        assert_eq!(parse_i2c_component("17-0050"), None);
    }

    #[test]
    fn fingerprint_uses_only_the_edid_base_block() {
        let mut short_read = vec![0_u8; 256];
        short_read[0] = 42;
        short_read[126] = 2;
        let mut full_read = short_read.clone();
        full_read.resize(384, 99);
        assert_eq!(edid_fingerprint(&short_read), edid_fingerprint(&full_read));
        assert_eq!(edid_fingerprint(&short_read[..127]), None);
    }

    #[test]
    fn decodes_linux_device_minor() {
        // Old-style Linux dev_t encoding: major 89 (i2c), minor 24.
        assert_eq!(i2c_bus_from_ddc_id(&0x5918_u64.to_string()), Some(24));
    }

    #[test]
    fn percentage_uses_raw_maximum() {
        let display = DdcDisplay {
            id: String::new(),
            name: String::new(),
            connector: String::new(),
            i2c_bus: None,
            current: 128,
            maximum: 255,
        };
        assert_eq!(display.percent(), 50);
    }

    #[test]
    fn connector_collects_direct_and_legacy_ddc_buses() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let connector = directory.path().join("card0-DP-1");
        let legacy = directory.path().join("i2c-6");
        fs::create_dir_all(connector.join("i2c-10")).expect("direct bus");
        fs::create_dir_all(&legacy).expect("legacy bus");
        symlink(&legacy, connector.join("ddc")).expect("DDC symlink");

        let (direct, ddc) = find_i2c_buses(&connector);
        assert_eq!(direct, HashSet::from([10]));
        assert_eq!(ddc, HashSet::from([6]));
    }

    #[test]
    fn matches_truncated_edid_by_direct_aux_bus() {
        let mut ddc_edid = vec![0_u8; 256];
        ddc_edid[7] = 17;
        let mut sysfs_edid = ddc_edid.clone();
        sysfs_edid.resize(384, 23);
        let fingerprint = edid_fingerprint(&ddc_edid).expect("fingerprint");
        let connectors = vec![connector(
            "DP-2",
            true,
            edid_fingerprint(&sysfs_edid),
            &[10],
            &[6],
        )];

        assert_eq!(
            match_connector(&connectors, fingerprint, Some(10)).map(|item| item.name.as_str()),
            Some("DP-2")
        );
        assert_eq!(
            handle_preference(&connectors, connectors.first(), Some(10)),
            0
        );
    }

    #[test]
    fn ambiguous_identical_edids_are_not_assigned_by_order() {
        let fingerprint = blake3::hash(&[7_u8; 128]);
        let connectors = vec![
            connector("DP-1", true, Some(fingerprint), &[], &[]),
            connector("DP-2", true, Some(fingerprint), &[], &[]),
        ];
        assert!(match_connector(&connectors, fingerprint, None).is_none());
    }

    #[test]
    fn mst_transport_owned_by_another_connector_is_only_a_fallback() {
        let fingerprint = blake3::hash(&[9_u8; 128]);
        let connectors = vec![
            connector("DP-3", false, None, &[11], &[]),
            connector("DP-4", true, Some(fingerprint), &[], &[]),
        ];
        let matched = match_connector(&connectors, fingerprint, Some(12));
        assert_eq!(matched.map(|item| item.name.as_str()), Some("DP-4"));
        assert_eq!(handle_preference(&connectors, matched, Some(12)), 2);
        assert_eq!(handle_preference(&connectors, matched, Some(11)), 3);
    }

    #[test]
    fn makes_colliding_ids_deterministically_unique() {
        let mut used = HashSet::new();
        assert_eq!(ensure_unique_id("edid-a".to_owned(), &mut used), "edid-a");
        assert_eq!(ensure_unique_id("edid-a".to_owned(), &mut used), "edid-a-2");
    }
}

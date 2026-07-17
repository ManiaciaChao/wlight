# Architecture

## Process and ownership model

`wlightd` is the only process that owns DDC handles and Wayland gamma-control
objects. The CLI and GUI are unprivileged D-Bus clients, so they neither
compete for the I²C bus nor request separate, exclusive gamma objects from the
compositor.

The daemon has two serialization boundaries:

- mutation requests pass through an asynchronous FIFO gate, with at most one
  blocking task running at a time; mutexes protect the long-lived
  `ddc_hi::Display` handles;
- the Wayland connection remains on a dedicated thread, and D-Bus handlers
  submit gamma updates through a command channel.

`ListDisplays` reads a separately published snapshot and does not wait for slow
DDC operations. A change that spans DDC and gamma applies dimming steps before
brightening steps. If a later step fails, the daemon makes a best-effort attempt
to roll back the steps already completed.

The egui thread performs no D-Bus I/O. It sends commands to a background worker
and incorporates the latest `DisplayInfo` snapshot through a result channel.
Slider input is debounced to avoid writing DDC at the frame rate while the user
drags it.

## Display association

DDC enumeration reads EDIDs through I²C. The backend also scans
`/sys/class/drm`, collecting each connector's direct AUX `i2c-*` child and
legacy `ddc` target. It prefers bus topology when associating a DDC handle with
a connector and falls back to a unique match on the 128-byte EDID base block.
Base-block fingerprints allow `ddc-hi`'s fixed 256-byte read to match a sysfs
EDID containing additional extension blocks.

One physical output can answer through multiple DDC transports, for example the
root AUX channel and a DPMST adapter in a DisplayPort MST topology. The backend
groups handles by `(connector, EDID fingerprint)`, preferring the connector's
direct AUX transport, followed by the legacy `ddc` target or an unambiguous MST
handle. Remaining handles are retained as aliases and tried automatically after
a transport failure. D-Bus exposes only one logical monitor for the group.

Stable IDs use a BLAKE3 digest of the EDID base block. A gamma-only output
without DDC/EDID uses `wayland:<connector>`, explicitly identifying a connector
profile rather than a physical monitor. The daemon restores this profile when
the connector reappears, even if a different monitor is attached; users should
remove the saved entry before replacing a gamma-only display when the old value
would be inappropriate. Distinct DDC monitors with identical EDIDs and no
unique serial number are disambiguated by connector or I²C bus.

## Brightness policy

Let the requested effective brightness be `T`, the hardware floor be `F`, and
`Q(x) = ceil(100x) / 100`. For a display with both controls, the selected
hardware factor is `H = max(Q(T), Q(F))`. This rounds DDC upward to an integer
percentage so that hardware never undershoots the effective target; gamma then
supplies the exact remainder.

| Available controls | Result |
|---|---|
| DDC + gamma | DDC percentage `100H`, gamma multiplier `T/H` |
| DDC only | DDC percentage `round(100T)` |
| gamma only | `gamma=T` |
| neither | return an explicit capability error |

When `H` is zero, DDC is set to zero and gamma remains at one. With a nonzero
floor, unified control permits a gamma multiplier of zero, so a 0% target
produces a fully black LUT. After setting 0%, use a keyboard shortcut, the CLI,
or switch to a text console to recover. A non-integer hardware floor is rounded
upward to the monitor's exposed integer percentage, with gamma compensating
back to the exact target.

## D-Bus contract

`io.github.wlight.Manager1` exposes five methods:

- `ListDisplays()`: read the cache without touching hardware;
- `Refresh()`: re-enumerate and merge the backends, reconnect a failed gamma
  worker, and idempotently reconcile actual controls with persisted desired
  state;
- `SetBrightness(id, fraction)`: apply the unified brightness policy;
- `SetDdcBrightness(id, percent)`: set VCP feature `0x10` directly;
- `SetGammaBrightness(id, fraction)`: update the LUT for one output directly.

Every mutation returns the complete updated `DisplayInfo`, so clients do not
need to infer clamping or backend state.

## Failure boundaries

- A Wayland gamma initialization failure does not disable DDC.
- DDC reads and writes make a bounded number of retries for transient transport
  errors. If a physical output has alias handles, the backend falls back to
  them automatically; failure on one monitor does not affect another.
- D-Bus input is checked for finite values and valid ranges before it reaches a
  hardware backend.
- Configuration is atomically replaced only after the hardware update succeeds.
- Persisted settings are desired state. At startup and on refresh, the daemon
  compares them with the DDC and gamma values read from the current control
  objects and applies only necessary changes. A device that reconnects with the
  same ID but reset controls is therefore restored, while an ordinary refresh
  produces no redundant writes.
- Shutting down the daemon closes the Wayland connection, after which the
  compositor restores its default gamma tables.

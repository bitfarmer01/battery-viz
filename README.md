# Battery Viz — battery optimization visualizer

A low-latency Rust desktop app that reads live battery telemetry on **macOS**
and **Windows** and scores battery health against a research-backed
optimization benchmark, rendered with egui on **wgpu** (GPU, no idle stall).

## Build

```sh
cargo run            # run the dashboard (simulator on non macOS/Windows hosts)
cargo test --all-targets   # 51 unit tests: parsers, model, benchmark, alerts
```

## Alerts — battery-longevity coaching

The app watches telemetry continuously and nudges rarely, quietly, and only
with one concrete action per nudge (`src/alert.rs`, a pure state machine fed
once per 1 Hz sample):

- **Running hot** — pack ≥ 45 °C sustained for 60 s; stays active until the
  pack cools below 42 °C; at most 3 nudges/day, 15 min apart.
- **High-charge dwell** — parked at ≥ 95 % for 10 minutes: unplug or enable
  optimized charging; at most 1/day, 6 h apart.
- **Charge band** — plugged in above 80 %: one unplug nudge per day.

Quiet hours (22:00–08:00 local) suppress everything. Pause lives in the macOS
tray menu; pausing resets the engine so no stale alert fires on resume.

- **macOS** — menu-bar agent: window hidden at launch (LSUIElement accessory,
  no Dock icon); the tray icon (show dashboard / pause alerts / quit) drives
  the app; notifications via `osascript display notification`.
- **Windows** — main window as before; alerts are WinRT toasts spawned via a
  PowerShell one-liner, so they inherit Focus Assist. No tray agent.

Memory discipline: while hidden, no egui frame is drawn — the 1 Hz sample tick
wakes the engine and tray only; the GPU runs at `LowPower` power preference.
The release profile is size-optimized (`opt-level = "z"`, fat LTO, stripped).

## What it shows

- **Benchmark ring** — 0–100 weighted score with grade (Excellent / Healthy /
  Aging / Stressed) and the coverage: how much of the weight was actually
  scored. Missing telemetry renders as "no data", never as a healthy score.
- **Scorecard** — per-parameter score and a one-line human explanation.
- **Live gauges** — charge, SoH, temperature, power in/out, cycles vs. cycle
  limit, voltage, high-charge dwell.
- **History** — 10-minute temperature and power plots at 1 Hz sampling.

## Benchmark weights

| Parameter          | Weight | Notes                                            |
| ------------------ | ------ | ------------------------------------------------ |
| State of health    | 30%    | full-charge vs. design capacity                  |
| Temperature        | 20%    | 20–30 °C ideal, hard penalty above 45 °C         |
| Charge band        | 15%    | 20–80% longevity band, heavier low-side penalty  |
| High-charge dwell  | 15%    | minutes parked at ≥95%                           |
| Cycle count        | 10%    | against OEM limit (default 1000)                 |
| Charge rate        | 10%    | C-rate while charging; N/A otherwise             |

Unavailable or not-applicable parameters are excluded and the weights
renormalize, with the resulting coverage shown next to the score.

## Sources

- **macOS** — `ioreg -r -n AppleSmartBattery -d 1 -a -x` (XML plist: cycle
  count, design/full-charge capacity, temperature in hundredths of °C, voltage,
  charging state). No root required.
- **Windows** — `Get-CimInstance Win32_Battery` (design/full-charge capacity,
  design voltage, charge remaining, `BatteryStatus` bits). Cycle count and
  temperature need `IOCTL_BATTERY_QUERY_*` (planned upgrade path).
- **Simulator** — deterministic drifting stream for development on any host.

Samples flow through a bounded crossbeam channel (capacity 4): if the UI
falls behind, old samples are dropped rather than queued, so the render loop
never inherits collector lag.

## Layout

```
src/
  main.rs            entry point, window options (menu-bar agent on macOS)
  app.rs             sample ingest, 600-sample history ring, alert ticks
  ui/mod.rs          egui dashboard (ring, gauges, scorecard, plots)
  benchmark/mod.rs   weighted scoring engine
  alert.rs           pure coaching-alert state machine (cooldowns, caps, quiet hours)
  clock.rs           injectable local hour/day for the alert engine
  notify/            platform notifications (macOS osascript, Windows toast)
  collect/
    mod.rs           trait, background thread, platform dispatch
    macos.rs         ioreg plist parser
    windows.rs       WMI/CIM collector
    sim.rs           simulator
    types.rs         telemetry model (every field optional + provenance)
```

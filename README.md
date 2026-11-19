# Battery Viz — battery optimization visualizer

A low-latency Rust desktop app that reads live battery telemetry on **macOS**
and **Windows** and scores battery health against a research-backed
optimization benchmark, rendered with egui on **wgpu** (GPU, no idle stall).

## Build

```sh
cargo run            # run the dashboard (simulator on non macOS/Windows hosts)
cargo test           # 18 unit tests: parsers, model, benchmark
```

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
  count, design/full-charge capacity, temperature in tenths of °C, voltage,
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
  main.rs            entry point, window options
  app.rs             sample ingest, 600-sample history ring
  ui/mod.rs          egui dashboard (ring, gauges, scorecard, plots)
  benchmark/mod.rs   weighted scoring engine
  collect/
    mod.rs           trait, background thread, platform dispatch
    macos.rs         ioreg plist parser
    windows.rs       WMI/CIM collector
    sim.rs           simulator
    types.rs         telemetry model (every field optional + provenance)
```

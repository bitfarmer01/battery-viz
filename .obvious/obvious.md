# battery-viz — agent guidance

Cross-platform Rust desktop app: battery telemetry (macOS ioreg, Windows WMI/CIM, simulator) -> weighted health benchmark -> egui/wgpu dashboard.

## Commands

- Build: `cargo build`
- Merge gate: `scripts/verify.sh` — runs `cargo fmt --check`, `cargo clippy --all-targets`, and `cargo test --all-targets`, then prints a paste-ready summary (test counts + head SHA). Measured 2026-11-20: 51 tests passing; `cargo fmt --check` currently fails (pre-existing, tree-wide adoption of the gate is deferred to a later PR so in-flight feature branches are not reformatted); clippy reports dead-code warnings for fixture-only collector functions. There is no CI (GitHub Actions is disabled at the account level and intentionally unused).
- Run: `cargo run` — opens the dashboard; simulator runs on any host, macOS uses live ioreg.

## Merge policy (project-specific, overrides workspace ki_TMZLlmkE)

- Fully autonomous flow: requirements, build, self-review, PR, merge — **no human approval at any stage**. The PR author merges their own PR.
- Merge only when: the full local gate (`scripts/verify.sh`) is green on the head commit, the diff is self-reviewed against the spec/requirements, and every review comment (bot or human) is fixed or answered. There is no CI gate; the local run is the gate and its output (counts, head SHA) must be cited in the PR.
- Conventional commits (`feat:`, `fix:`, `test:`, `docs:`, `chore:`). One PR per spec; small fixes may skip the spec.

## Environment gotchas

- **git push over HTTPS is blocked** on this account ("Your repository is disabled", 403 — account billing flag). Use the GitHub REST API for all repo writes: create refs (`git/refs`), commit files (`contents` API), open/merge PRs. `gh pr view/merge` and `gh api` work.
- The repo is public.
- Platform collectors (macOS/Windows) are fixture-tested only; real-hardware verification is a known outstanding item — do not claim hardware support as verified.

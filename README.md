# arena-infra-rs

A streamlined Rust rewrite of `arena-infra` — the control plane for ARENA's GPU
pods. Goals: multi-provider machine spin-up, a tidy commit/backup flow, a
performance/GPU/progress dashboard, and robust port forwarding, behind both a CLI
and an interactive TUI.

## Status: first slice

This is the **scaffold + machine-spin-up vertical**. Implemented so far:

- `arena-core` — library
  - `config` — tolerant parser for the existing `config.env` (incl. the
    `MACHINE_NAME_LIST` bash array), so this tooling reads the *same* config as
    the legacy bash/python scripts.
  - `provider::Provider` — the single trait every backend implements.
  - `provider::runpod` — RunPod REST backend: list / create / stop / terminate.
  - `provider::vast` — Vast.ai REST backend against the same trait. Vast rents
    *offers* rather than named pods, so `create_pod` searches the marketplace for
    the cheapest rentable offer matching the spec (GPU type/count, disk) and rents
    it, carrying the machine name as the instance `label`. GPU-name matching is
    normalized so the same `RUNPOD_GPU_TYPE` value works across both providers.
  - `naming` — next-free machine-name allocation, mirroring the legacy logic.
  - `proxy` — port-forwarding planner. Computes a *stable* public-port map (each
    pod's port is anchored to its index in `MACHINE_NAME_LIST`, so tearing down one
    pod never renumbers the others), and renders the nginx `stream` config plus the
    SSH-tunnel commands to apply on the proxy host. Pure/no-I/O — it plans, you apply.
- `arena` (CLI) — `pods list | create | stop | terminate` (`--provider runpod|vast`,
  Vast reads `VAST_API_KEY`) and `proxy plan` (read-only; prints config to apply,
  `--out` saves the nginx config locally).
- `arena-tui` (TUI) — read-only pod dashboard (ratatui).

Not yet built (planned verticals): commit/backup flow, GPU/progress dashboard.

## Safety model (this is developed against live production)

- Runs as the unprivileged `dev` user, which **cannot read `/root`**. It only sees
  a read-only copy of config at `/home/dev/prod-ro/config.env`.
- **Read-only by default.** `pods list` and the TUI only ever issue GET requests.
- **Every mutating command is dry-run by default.** `create`/`stop`/`terminate`
  print what they *would* do and only act when given `--apply`.

## Usage

```bash
# build
cargo build

# list pods (read-only)
cargo run -p arena-cli -- pods list

# preview creating 3 pods (dry-run — no API mutation)
cargo run -p arena-cli -- pods create -n 3

# actually create them
cargo run -p arena-cli -- pods create -n 3 --apply

# interactive dashboard
cargo run -p arena-tui

# point at a different config
cargo run -p arena-cli -- --config ./config.env pods list
```

## Layout

```
crates/
  core/   library: config, provider trait + impls, pod model, naming
  cli/    `arena` binary (clap)
  tui/    `arena-tui` binary (ratatui)
```

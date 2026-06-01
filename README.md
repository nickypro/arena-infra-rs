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
  - `provider::vast` — Vast.ai stub against the same trait (next vertical).
  - `naming` — next-free machine-name allocation, mirroring the legacy logic.
- `arena` (CLI) — `pods list | create | stop | terminate`.
- `arena-tui` (TUI) — read-only pod dashboard (ratatui).

Not yet built (planned verticals): Vast.ai impl, commit/backup flow, GPU/progress
dashboard, port-forwarding/proxy management.

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

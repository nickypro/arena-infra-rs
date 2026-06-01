# arena-infra-rs

A streamlined Rust rewrite of `arena-infra` — the control plane for ARENA's GPU
pods. Goals: multi-provider machine spin-up, a tidy commit/backup flow, a
performance/GPU/progress dashboard, and robust port forwarding, behind both a CLI
and an interactive TUI.

## Status

All four target verticals are implemented (multi-provider spin-up, commit/backup,
GPU/progress dashboard, proxy/port-forwarding), behind a CLI and an interactive TUI:

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
  - `provider::hetzner` — Hetzner Cloud backend for **CPU-only** VMs. Not a GPU
    container host, so it ignores the GPU-centric `PodSpec` fields and takes its
    sizing/OS/location/SSH-keys from `HETZNER_*` config. VMs come up on a real public
    IP with SSH on `:22`, so they use the same proxy/`pods up` flow as GPU providers.
  - `naming` — next-free machine-name allocation, mirroring the legacy logic.
  - `proxy` — port-forwarding planner. Pods are reached over SSH (VS Code
    Remote-SSH), and the provider reassigns a pod's SSH endpoint on restart, so the
    proxy host gives each machine a *stable* public port (`cute.sus.cat:7000`, …)
    that nginx's `stream` module forwards straight to the pod's current SSH endpoint
    — pure nginx, no tunnel process. The public port is anchored to the machine's
    index in `MACHINE_NAME_LIST`, so tearing down one pod never renumbers the others
    and a returning machine reclaims its port. Pure/no-I/O — it plans, you apply.
- `arena` (CLI):
  - `pods list | create | stop | terminate` (`--provider runpod|vast|hetzner`; Vast
    reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY` + `HETZNER_*`).
  - `pods up -n N` — one-command spin-up: create, poll until each pod has an SSH
    endpoint, then print the proxy plan. Dry-run unless `--apply`; `--no-wait` skips
    polling. Polling stops at `--timeout`; nothing runs in the background.
  - Batch create (`create`/`up`) uses **typed provider errors** (`ProviderErrorKind`):
    on **capacity** exhaustion it stops gracefully and keeps the pods it got (e.g.
    "created 6 of 10") rather than erroring — `--keep-trying` instead waits and
    retries; on **auth** failure it aborts immediately. Already-created pods are
    never rolled back.
  - `proxy plan` — read-only; prints the nginx `stream` config to apply (`--out`
    saves it locally; never deploys to the proxy).
  - `backup` — commit + push each pod's ARENA working tree to a per-machine branch
    (`backup/<name>`) over SSH. Dry-run unless `--apply`; clean trees report
    `NO_CHANGES` rather than failing. Repo path / branch / push key via `BACKUP_*`.
- `arena-tui` (TUI) — read-only dashboard (ratatui): pods from the configured
  provider plus, per pod, GPU utilization/mem/temp via `nvidia-smi` over SSH and an
  optional progress signal (`PROGRESS_CMD`). Metrics fetched concurrently across the
  fleet. Provider via `ARENA_PROVIDER` (default `runpod`), config via `ARENA_CONFIG`.

Shared library pieces: `ssh` (non-interactive, fail-fast SSH command build + run),
`metrics` (nvidia-smi parsing + per-pod aggregation), and `provider::build` (the one
factory that constructs a backend by name — used by both the CLI and TUI).

All four target verticals are now in place: multi-provider spin-up (RunPod/Vast/
Hetzner), commit/backup, the GPU/progress dashboard, and proxy/port-forwarding.

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

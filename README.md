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
  - `pods list | create | stop | restart | terminate` (`--provider
    runpod|vast|hetzner`; Vast reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY`
    + `HETZNER_*`). `list` takes `--json`; `stop`/`restart`/`terminate` accept a
    **machine name or id**. `restart` restarts in place (RunPod restart / Hetzner
    reboot / Vast stop+start), preserving the machine where supported.
  - `pods up -n N` — one-command spin-up: create, poll until each pod has an SSH
    endpoint, then print the proxy plan. Dry-run unless `--apply`; `--no-wait` skips
    polling. Polling stops at `--timeout`; nothing runs in the background.
  - Batch create (`create`/`up`) uses **typed provider errors** (`ProviderErrorKind`):
    on **capacity** exhaustion it stops gracefully and keeps the pods it got (e.g.
    "created 6 of 10") rather than erroring — `--keep-trying` instead waits and
    retries; on **auth** failure it aborts immediately. Already-created pods are
    never rolled back. **Transient** failures (429 / 5xx / connect-timeout) are
    retried automatically with exponential backoff (`retry` module) around create
    and list calls — so a throttle or blip doesn't fail the command.
  - `proxy plan` — read-only; prints the nginx `stream` config to apply (`--out`
    saves it locally; never deploys to the proxy).
  - `config check` — validate that the keys the selected provider + proxy + backup
    need are present (never prints secret values; exits non-zero if a required key is
    missing). Copy `config.env.example` to get started.
  - `cron install|remove|show` — manage a crontab schedule for `arena backup`
    (default hourly); edits only arena-managed lines, leaving other entries intact.
  - `backup` — commit + push each pod's ARENA tree to its autocommit branch
    (`autocommit-{prefix}-w{week}d{day}-{machine}`, week/day from `ARENA_START_DATE`,
    `--week`/`--day` to override) over SSH. Dry-run unless `--apply`; clean trees
    report `NO_CHANGES` rather than failing.
  - `setup` — provision pods over SSH: copy the git deploy key, write `~/.name`,
    point the repo at the GitHub SSH URL on the default branch. Dry-run unless
    `--apply`. Uses `GIT_SSH_KEY_LOCAL/REMOTE`, `ARENA_REPO_OWNER/NAME`,
    `DEFAULT_BRANCH`.
- `arena-tui` (TUI) — interactive dashboard (ratatui): pods from the configured
  provider plus, per pod, GPU stats via `nvidia-smi`, git branch, a setup-health check,
  and an optional progress signal (`PROGRESS_CMD`) — all over SSH in **one** probe per
  pod. Fetching runs in a **background task** so the UI never freezes; it auto-refreshes
  (`ARENA_REFRESH_SECS`, default 5) and `f` cycles the cadence live (2/5/10/20/60s).
  Provider via `ARENA_PROVIDER` (default `runpod`), config via `ARENA_CONFIG`.
  - **Columns**: GPU (live from `nvidia-smi`, e.g. `2×RTX A4000` — the provider list
    API omits this), GPU%/MEM/TEMP, `$/HR`, a **SET** health glyph (`✓/✗/·` for
    `~/.name`, the deploy key, git origin→GitHub), and **BRANCH**. The **fleet summary
    bar** shows total GPUs, mean util, memory, and burn as both `$/hr` and `$/day`.
  - **Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU
    breakdown, full branch/origin/health, util/temp **sparklines**). `Ctrl-C`/`q` quit.
  - **Act** on the selected pod with `a` (restart / stop / terminate / backup / setup),
    on the **whole fleet** with `A` (safe ops only: restart / backup / setup), or **add
    pods** with `n`. Every mutation goes through a confirmation modal — the *only* place
    the TUI mutates anything. Lifecycle actions (restart/stop/terminate) require
    **typing the pod's exact name**; fleet actions require typing **ALL**; backup/setup
    show the precise command(s) and take a single `y`; add-pod previews the names it
    will allocate before `enter`. The dashboard's reads stay reads.

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

# interactive dashboard (or `arena tui`, which inherits --provider/--config)
cargo run -p arena-tui

# point at a different config
cargo run -p arena-cli -- --config ./config.env pods list
```

### Command shorthands

Subcommands accept any **unambiguous prefix** (Cisco-style), at every level:

```bash
arena po l          # == arena pods list
arena tui           # launch the dashboard
arena co c          # == arena config check
arena b --apply     # == arena backup --apply
```

An ambiguous prefix errors and lists the candidates — e.g. `arena p` is rejected
because it matches both `pods` and `proxy` (use `po`/`pr`); likewise `c` →
`config`/`cron` (use `co`/`cr`).

### Overriding config without editing it

Any key in `config.env` can be overridden by an **environment variable of the same
name** (env wins; it can't introduce brand-new keys). This is how you point at an SSH
key the current user can actually read, without editing the shared, read-only prod
config — e.g. when the dashboard's metrics show `ssh connect failed … key unreadable`
because the configured key lives under `/root`:

```bash
# use a readable copy of the shared key for nvidia-smi over SSH
SHARED_SSH_KEY_PATH=~/.ssh/arena8_key arena tui

# same idea for the git deploy key used by `setup`
GIT_SSH_KEY_LOCAL=~/.ssh/arena_infra_key arena setup --apply
```

## Layout

```
crates/
  core/   library: config, provider trait + impls, pod model, naming
  cli/    `arena` binary (clap)
  tui/    `arena-tui` binary (ratatui)
```

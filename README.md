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
  - `pods list | create | stop | restart | terminate | kill` (`--provider
    runpod|vast|hetzner`; Vast reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY`
    + `HETZNER_*`). `list` takes `--json`; `stop`/`restart`/`terminate` accept a
    **machine name or id**. `list --probe` fills the GPU column from `nvidia-smi` over
    SSH (the provider list API omits GPU type). `restart` restarts in place (RunPod restart / Hetzner
    reboot / Vast stop+start), preserving the machine where supported. `terminate
    --all` tears down the **whole fleet** (confirms first; `--dry-run` lists every
    pod without touching them) — for end-of-program teardown. `stop --all`
    (with `--include`/`--exclude`) stops many at once; `kill` is the stop→wait-for-
    EXITED→delete flow (`--timeout`; one target or `--all`).
  - `create` also takes **explicit names** (`pods create apple bloom`) and an
    **`--image`** override, alongside `-n`(target total) / `-a`(add). Bare names get
    the configured prefix; names already present are skipped.
  - `create`/`up` take `--retry-mins <M>` (`--retry-secs`, default 60) to **keep
    topping up to the target** while capacity is short — one round per interval for up
    to M minutes, **Ctrl+C** stops early keeping what was made. A `no instances
    available` capacity error is recognized as such (it waits), not treated as fatal.
    The confirm prompt lists the exact pod names about to be created.
  - `pods up -n N` — one-command spin-up: create, poll until each pod has an SSH
    endpoint, then **wire the proxy**: if nginx is set up on the proxy host it deploys +
    reloads it; otherwise it just says to run `arena proxy plan` (no config dump).
    `--setup` also provisions each pod over SSH. So `pods up -n 28 --gpu A40 --cloud
    SECURE --disk 200 --retry-mins 60 --setup` is a full start-of-iteration spin-up that
    waits for capacity then wires everything. Confirms first (`--dry-run` previews);
    `--no-wait` skips polling.
  - Batch create (`create`/`up`) uses **typed provider errors** (`ProviderErrorKind`):
    on **capacity** exhaustion it stops gracefully and keeps the pods it got (e.g.
    "created 6 of 10") rather than erroring — `--keep-trying` instead waits and
    retries; on **auth** failure it aborts immediately. Already-created pods are
    never rolled back. **Transient** failures (429 / 5xx / connect-timeout) are
    retried automatically with exponential backoff (`retry` module) around create
    and list calls — so a throttle or blip doesn't fail the command.
  - `proxy plan` — read-only; prints the nginx `stream` config (`--out` saves it
    locally; never connects to the proxy). `proxy apply` **deploys** that config to the
    proxy host over SSH and reloads nginx (`nginx -t && nginx -s reload`); `--dry-run`
    shows the exact scp + reload without doing it. This is the one place the tool touches
    the proxy host.
  - `plan check | show` — a scheduled provisioning plan (`arena-plan.json`, see
    `arena-plan.example.json`): per-day target fleets with **GPU-first fallback chains**
    (e.g. `A4000` across community→secure→vast, then `3090`, then `A5000`) and a night
    **window** + caps. `check` validates + prints the detected local time/timezone and
    every day's resolved date; `show [--date]` previews the fallback order and
    fill-to-target vs the live fleet. Read-only today; the timed executor + `arm`/`disarm`
    (which only fires inside the night window, in system-local time) are the next step.
  - `config check` — validate that the keys the selected provider + proxy + backup
    need are present (never prints secret values; exits non-zero if a required key is
    missing). Copy `config.env.example` to get started.
  - `cron install|remove|show` — manage a crontab schedule for `arena pods backup`
    (default every 15 min; `--start-date` bakes `ARENA_START_DATE` into the line); edits only
    arena-managed lines, leaving other entries intact.
  - `pods backup` — commit + push each pod's ARENA tree over SSH **on whatever branch
    the pod is currently on** (never switches/creates a branch, so bespoke branches are
    respected), and **skips `main`/`master`** (won't push the protected branch).
    `--message` overrides the commit message. Confirms first (`--dry-run` previews); clean
    trees report `NO_CHANGES` rather than failing. To stage onto a dated autocommit
    branch, run `pods init-branches` first.
  - `pods set-branch <branch> [target|--all] [--hard]` — switch pods' ARENA checkout to a
    branch. Gentle by default (fetch + checkout + ff-only pull — fails on a diverged/dirty
    tree rather than clobbering work). **`--hard` is destructive**: force the branch to
    match `origin/<branch>`, discarding local commits/changes (untracked files survive) —
    e.g. `set-branch main --all --hard` resets the fleet to `main`. Confirms first;
    `--dry-run` previews.
  - `pods init-branches` — create each pod's `autocommit-…-wNdM-…` branch and push it
    upstream **without committing** (legacy `init_branches`), so a new day's branch
    exists before `backup` runs. `--week`/`--day` override; `--dry-run` previews.
  - `pods run <cmd>` / `pods test` — run an arbitrary command on every pod (concurrent,
    confirms first) / the read-only torch-version health check.
  - `pods pull [label]` — the **file** backup (complementing the git `backup`): rsyncs
    each pod's home into `<dir>/<label>/<pod>/` (`--dir`, `--max-size`, `--remote-path`),
    size-capped with dotfile/`site-packages` excludes. Label defaults to the `wNdM`
    iteration. Confirms first; `--dry-run` prints the exact rsync commands.
  - `pods copy-keys` — distribute API keys into each pod's `~/.bashrc`/`~/.zshrc`
    (idempotent): per-host keys from `<keys-dir>/<provider>_api_keys.csv`
    (openai/anthropic/openrouter) **plus a broadcast Hugging Face token** (`--hf-token`
    or config `HF_TOKEN`) so the cohort can pull **gated repos** we're approved for
    (Llama 3, …) — it sets both `HF_TOKEN` and `HUGGING_FACE_HUB_TOKEN`. `--include`/
    `--exclude` (name or id) scope it to specific pods. Confirms first; `--dry-run` lists
    what would be set (values redacted).
  - `pods copy <file> [dest]` — scp a local file to every pod (concurrent; `--include`/
    `--exclude` to scope). With no `dest` it **mirrors the path under the ARENA repo**
    (a local `…/ARENA_3.0/foo/bar.py` → `/root/ARENA_3.0/foo/bar.py`); otherwise `dest`
    is the remote path (trailing `/` = into that dir). Creates the remote parent dir;
    confirms first; `--dry-run` previews.
  - `ssh-config [--proxy] [--out]` — emit the **participant-facing `~/.ssh/config`**:
    direct pod endpoints by default, or stable proxy ports (`--proxy`) anchored to each
    machine's `MACHINE_NAME_LIST` index. Read-only.
  - `pods setup` — provision pods over SSH (ordered steps in `--help`): copy the git
    deploy key, write `~/.ssh/config` + `authorized_keys`, point the repo at GitHub,
    update submodules, write `~/.name`, and — **if `HF_TOKEN` is set** — export it
    (`HF_TOKEN` + `HUGGING_FACE_HUB_TOKEN`) for gated-repo access (else that step is
    skipped, and it says so). Confirms first (`--dry-run` previews, with the token
    redacted). Uses `GIT_SSH_KEY_LOCAL/REMOTE`, `ARENA_REPO_OWNER/NAME`, `DEFAULT_BRANCH`.
  - `config check | set | which` — `check` is the read-only doctor (keys + setup
    readiness); `config set KEY VALUE` writes a key (e.g. an API key) into config.env, or
    with no args prompts interactively (the picker shows which keys are already set, and
    includes `HF_TOKEN`); `config which` shows the active config file (path,
    readable/**writable**), what parsed, and any keys coming from the environment.
- `arena-tui` (TUI) — interactive dashboard (ratatui): pods from the configured
  provider plus, per pod, GPU stats via `nvidia-smi`, git branch, a setup-health check,
  and an optional progress signal (`PROGRESS_CMD`) — all over SSH in **one** probe per
  pod. Fetching runs in a **background task** so the UI never freezes; it auto-refreshes
  (`ARENA_REFRESH_SECS`, default 5) and `f` cycles the cadence live (2/5/10/20/60s).
  Provider via `ARENA_PROVIDER` (default `runpod`), config via `ARENA_CONFIG`.
  - **Columns**: a provider badge (`R`/`V`/`H`), NAME (short `apple` by default;
    `s` toggles full `arena8-apple` and the choice persists to
    `~/.config/arena-tui/prefs`), STATUS, a **SET** health glyph (`✓/✗/·` for `~/.name`,
    the deploy key, git origin→GitHub), GPU (live from `nvidia-smi`, e.g. `2×RTX A4000`
    — the provider list API omits this), GPU%/MEM/TEMP, `$/HR`, **BRANCH** (autocommit
    branches shortened to their `w1d2` label; `main`/others shown as-is), and progress.
    The **fleet summary bar** shows total GPUs, mean util, memory, and burn as both
    `$/hr` and `$/day`.
  - **Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU
    breakdown, full branch/origin/health, util/temp **sparklines**). `Ctrl-C`/`q` quit.
  - **Act** on the selected pod with `a` (restart / stop / terminate / backup / setup /
    test / run / set-branch), on the **whole fleet** with `A` (restart / backup / setup /
    test / run / set-branch), or **add pods** with `n`. `run` and `set-branch` pop a
    text-input modal to type the command / branch; `test` is read-only. Every mutation
    goes through a confirmation modal — the *only* place the TUI mutates anything.
    Lifecycle actions (restart/stop/terminate) require **typing the pod's exact name**;
    fleet mutations require typing **ALL**; backup/setup/test show the precise
    command(s) and take a single `y`. The dashboard's reads stay reads.
  - **Add-pod (`n`)** is an interactive form: `↑↓` moves between fields, `←→` changes
    the value. Pick the provider (unavailable ones — no API key — are greyed out),
    cloud type (RunPod only), GPU type (full option list shown) and count, and how many
    pods; it previews the names it will allocate, then creates on `enter`.

Shared library pieces: `ssh` (non-interactive, fail-fast SSH command build + run),
`metrics` (nvidia-smi parsing + per-pod aggregation), and `provider::build` (the one
factory that constructs a backend by name — used by both the CLI and TUI).

All four target verticals are now in place: multi-provider spin-up (RunPod/Vast/
Hetzner), commit/backup, the GPU/progress dashboard, and proxy/port-forwarding.

## Safety model (this is developed against live production)

- Runs as the unprivileged `dev` user, which **cannot read `/root`**. It only sees
  a read-only copy of config at `/home/dev/prod-ro/config.env`.
- **Read-only by default.** `pods list` and the TUI only ever issue GET requests.
- **Mutating commands act, but confirm first.** `create`/`stop`/`terminate`/… print
  what they'll do and prompt `Proceed? [y/N]` at a terminal. `-y`/`--yes` skips the
  prompt. With **no terminal** (cron, pipes) they *refuse* unless `--yes` is given — so
  nothing mutates non-interactively by accident. (`cron install` bakes `--yes` in.)
- **`--dry-run` previews** any mutating command (also `--dry`/`--dryrun`): prints exactly
  what would happen and changes nothing.

## Usage

```bash
# build
cargo build

# list pods (read-only)
cargo run -p arena-cli -- pods list

# preview creating 3 pods (no API mutation)
cargo run -p arena-cli -- pods create -n 3 --dry-run

# actually create them (prompts to confirm; -y to skip)
cargo run -p arena-cli -- pods create -n 3

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
arena po ba          # == arena pods backup (prompts to confirm)
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
GIT_SSH_KEY_LOCAL=~/.ssh/arena_infra_key arena pods setup
```

## Layout

```
crates/
  core/   library: config, provider trait + impls, pod model, naming
  cli/    `arena` binary (clap)
  tui/    `arena-tui` binary (ratatui)
```

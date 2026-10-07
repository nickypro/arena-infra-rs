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
  - `provider::runpod` — RunPod REST **v1** backend: list / create / stop / terminate.
    RunPod retires REST v1 on **2026-11-15**.
  - `provider::runpod_v2` — RunPod REST **v2** backend (`api.runpod.io/v2`), opt-in via
    `RUNPOD_API=v2` (default `v1`; any other value is an error; `config check` shows which
    is active). Same provider name and commands. Differences: real lifecycle statuses
    (`PROVISIONING`/`STARTING`/`RUNNING`/`EXITED`/`ERROR`); the SSH endpoint comes from
    `ssh.direct` only; create always sends `cloud` (v2 defaults to SECURE) and merges the
    account's registered SSH keys into `PUBLIC_KEY` (v2 skips them when it's set);
    `replace` recovers GPU type + cloud tier. Rename and maintenance still use GraphQL.
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
    The plan is a **sticky merge**: the previous forwards are parsed back from the
    config it rendered last time (each block carries a `# arena-forward name=… port=…
    target=… provider=… pod_id=…` line; the older `# <name>` format is read too) and
    merged with a **per-provider** listing (`Provider::list_by_provider`). A forward is
    only removed once its pod is confirmed gone — absent from a provider whose listing
    *succeeded*; a pod listed without an endpoint, or one whose provider failed to list
    (e.g. a Vast 429), keeps its forward, and if no provider answers nothing is written.
    Provider list responses with an unexpected shape are errors, never "zero pods".
- `arena` (CLI):
  - `pods list | create | stop | restart | terminate | kill` (`--provider
    runpod|vast|hetzner`; Vast reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY`
    + `HETZNER_*`). `stop`/`restart`/`terminate` accept a **machine name or id**.
    `list` shows NAME PROVIDER ID STATUS GPU (`count×type`) $/H ENDPOINT MAINT (the
    host's RunPod maintenance window, e.g. `maint 10-09 02:00→06:00 UTC`; the host's
    free-text note is flattened onto one line) and a footer
    `fleet: $X/h across N running pod(s)` summing RUNNING pods (Hetzner's € shown
    separately, unpriced pods counted). GPU/$/maintenance come from one extra read-only
    RunPod GraphQL query per `list` (best-effort: if it fails you get one warning line and
    the list still renders); the `nvidia-smi` probe over SSH (default for the table,
    `--probe`/`--no-probe`) overrides the GPU when a pod answers. `list --json` emits the
    pods incl. `gpu_count`/`cost_per_hr`/`maintenance`. `restart` restarts in place (RunPod restart / Hetzner
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
  - **Multi-option placement** (`create`/`up`): `--gpu A4000,4000Ada,3090 --cloud
    community,secure --max-price 0.5 [--order cheapest|listed]`. The gpu × cloud options
    are priced (RunPod's live catalog — v2 `/catalog/gpus` per tier on `RUNPOD_API=v2`, else
    GraphQL — falling back to preset estimates, shown `~`), capped per pod (price × `--gpus`;
    with a cap an option with no known price is dropped, not risked) and ordered: `cheapest`
    (default; a price tie goes to the better stock hint, then the listed order) or `listed`
    (GPU-major). Stock never filters — creating is the truth. Per name the options are tried
    **one create at a time** (never two creates for one name); a capacity error skips that
    option for the rest of the round, auth aborts, any other error stops as before.
    `--retry-mins` re-runs rounds (blocks cleared, fleet re-listed first so a name that
    appeared meanwhile — or a met `-n` target — isn't created again); Ctrl+C between rounds
    keeps what was made. Ends with a per-name table: `created on 1×RTX 3090 COMMUNITY
    ($0.22/h) (after 1×RTX A4000 COMMUNITY: capacity)` or `not placed (tried: …)`. Cloud
    tiers exist only on RunPod (Vast/Hetzner collapse them, and Hetzner the GPU list too,
    with a note); Vast/Hetzner quote no price before create, so their options are unpriced.
    `--keep-trying` stays single-option (use `--retry-mins`). One `--gpu`, one `--cloud` and
    no `--max-price` is exactly the old single-spec path. `--dry-run` prints the option
    table and the names it would attempt; the option order is fixed once confirmed.
  - `offers [--gpu …] [--cloud …] [--max-price …] [--gpus N] [--order …] [--json]` —
    read-only: the same option table (OPTION, CLOUD, $/H/POD, PRICE source, STOCK, plus what
    the cap dropped and why), i.e. what `create`/`up` would try. `--json` = the plan.
  - `pods up -n N` — one-command spin-up: create, poll until each pod has an SSH
    endpoint, then **wire the proxy**: if nginx is set up on the proxy host (or the proxy
    is write-only) it deploys as endpoints appear, and ends with the same one-line sync
    as the other lifecycle commands (below) — or a note saying why it skipped.
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
  - `proxy plan` — read-only; shows the merge against the current config (`+` added,
    `~` changed — including a kept entry whose port moved with the list —, `-` removed,
    `=` kept-stale, plus a `+N added, ~N changed, …` summary) and prints the nginx
    `stream` config (`--out` saves it locally; never connects to the proxy). Without the
    current config (remote proxy, unreadable file) it's labelled a **listing-only
    preview** and `--out` is refused — deployed by hand it would drop every kept forward.
    `proxy apply` prints the same changes, confirms, then **deploys** the config (locally,
    or over SSH when `PROXY_LOCAL=false`) and runs `SSH_PROXY_RELOAD_CMD` — default
    `nginx -t && nginx -s reload`; set it **empty** (in the file, or exported empty by a
    wrapper) for write-only (never reloads nginx). Writes are serialized (`flock`), refuse
    to overwrite a config another run changed since it was read (the merge is redone), and
    are atomic locally (temp + rename). A **failed reload puts the previous config back**,
    so the next sync retries instead of reporting "unchanged". `--dry-run` shows the
    write/upload + reload without doing it. This is the one place the tool touches the
    proxy host.
  - **Auto proxy sync.** `pods create`, `up`, `rename`, `reimage`, `terminate` (one or
    `--all`), `replace` and `migrate cutover/revert` end by re-syncing the proxy through the
    same merge, from a fleet-wide listing, and print one line — e.g. `[proxy] after
    terminate: +0 ~0 -1 =0 (deployed; removed arena8-apple)` or `… (unchanged)`. It's
    best-effort: no proxy configured / no nginx to deploy to → a one-line skip note; a
    listing or write error → a warning, never a failed command. `--skip-proxy` (create,
    terminate, rename, reimage, replace, migrate) opts out. A just-terminated pod can still
    be listed briefly, so its forward may survive that sync; the next one removes it. A
    `create`/`up` that fails part-way still syncs for the pods it made. A partly failed
    `rename` batch doesn't sync: with `--from-prefix`, re-run the rename *before* any
    `proxy apply`/lifecycle command/proxy cron tick, which would drop the not-yet-renamed
    pods' forwards. `migrate cutover` still treats a sync that didn't land as a failure and
    auto-reverts; `replace` only terminates the old pod once the sync routed the name to
    the new one (else it keeps it and says why). Provider list calls are bounded (60s),
    so a stalled API reads as "failed to list" (forwards kept), not a hang.
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
    (default every 15 min, git-only; `--pull` runs the full backup — git + rsync file
    backup — each tick; `--start-date` bakes `ARENA_START_DATE` into the line); edits only
    arena-managed lines, leaving other entries intact. `--proxy` adds a `*/5 … proxy apply
    --yes` line (log: `~/arena-proxy-cron.log`; sets a `PATH` with `/usr/sbin` so cron
    finds nginx, and `flock -n` so a slow tick never piles up) that catches changes made outside the CLI
    (dashboard terminates, restarts that move an endpoint); re-running `install` without
    it removes that line. Remove it before changing `MACHINE_NAME_PREFIX`/`_LIST`: a name
    that leaves the list loses its forward on the next tick.
  - `pods backup [target]` — the **full save**: git-push the ARENA tree **and** rsync the
    home to the local backups folder (`pull`). The git push is on **whatever branch the
    pod is on** (never switches/creates one, so bespoke branches are respected) and
    **skips `main`/`master`**; clean trees report `NO_CHANGES`. One pod (name/id) or all.
    `--no-pull` = git only; `--message` overrides the commit message. Confirms first
    (`--dry-run` previews both). Each pod's git push has a 5-min budget. To stage onto a
    dated autocommit branch, run `pods init-branches` first.
  - `pods set-branch <branch> [target|--all] [--hard]` — switch pods' ARENA checkout to a
    branch. Gentle by default (fetch + checkout + ff-only pull — fails on a diverged/dirty
    tree rather than clobbering work). **`--hard` is destructive**: force the branch to
    match `origin/<branch>`, discarding local commits/changes (untracked files survive) —
    e.g. `set-branch main --all --hard` resets the fleet to `main`. Confirms first;
    `--dry-run` previews. Pods switch concurrently, 2-min budget each.
  - `pods init-branches` — create each pod's `autocommit-…-wNdM-…` branch and push it
    upstream **without committing** (legacy `init_branches`), so a new day's branch
    exists before `backup` runs. `--week`/`--day` override; `--dry-run` previews. 2-min
    budget per pod.
  - `pods run [--timeout <secs>] <cmd>` / `pods test` — run an arbitrary command on every
    pod (concurrent, confirms first; each pod gets `--timeout`, default 1800s = 30 min —
    flags go *before* the command; on a timeout the local ssh is killed, and the remote
    command dies at its next write to the closed connection) / the read-only
    torch-version health check (90s per pod).
  - `pods test --deep [names…] [--json] [-v]` — the **is-this-pod-usable** check (read-only),
    for what a plain `import torch` misses on a bad host. One embedded script per pod
    (one SSH exec, 150s budget, inside the conda env) measures: nvidia-smi GPUs + driver +
    CUDA version; torch import, `cuda.is_available()`, device count vs nvidia-smi; a small
    tensor op on **every GPU** (catches `cuInit` 999 / `CUDA error: unknown error`); with >1
    GPU a GPU→GPU copy that must arrive intact and an NCCL `all_reduce` across the GPUs
    (else `skipped (1 GPU)`); a 32 MiB Hugging Face download (no token sent); free disk on
    `/` and `/workspace`; host load + uptime. The provider's maintenance window comes from
    the API. **FAIL**: any CUDA/tensor/copy/NCCL error, count mismatch, missing
    torch/nvidia-smi, driver below the floor, unreachable/timed out. **WARN**: download
    < 2 MB/s or unreachable, < 10 GB free, host load above max(32, host CPUs), a
    maintenance window. Driver floor: `MIN_DRIVER_VERSION` (`none` = off), else derived from
    `ALLOWED_CUDA_VERSIONS` (13.x → 580, 12.8 → 570, 12.4 → 550, …; the lowest listed
    version wins), else no driver check. Hetzner CPU VMs skip the GPU checks. Output: a
    `NAME RESULT GPUS DRIVER CUDA NET NOTES` table, `-v` lists every check, and a `same
    host?` line when ≥2 failing pods share a public IP (a bad host breaks every pod on it).
    `--json` prints per-pod `{name, provider, status, checks, facts}` (no IPs). Exits
    non-zero if any pod FAILs; warnings don't. Names scope the run (a named pod with no SSH
    endpoint is a FAIL; a typo is an error).
  - `pods pull [label]` — the **file** backup (complementing the git `backup`): rsyncs
    each pod's home into `<dir>/<label>/<pod>/`, reporting files/bytes moved per pod.
    **Keeps `.git`** (so the backup is a usable repo; `--no-git` to skip), size-caps with
    `--max-size`, excludes other dotfile dirs + `site-packages`. Knobs come from flags
    else config: `LOCAL_BACKUP_DIR` (`--dir`), `BACKUP_MAX_SIZE` (`--max-size`),
    `BACKUP_REMOTE_PATH` (`--remote-path`, default `~/`). Label defaults to the `wNdM`
    iteration. Confirms first; `--dry-run` prints the exact rsync commands.
  - `pods copy-keys` — distribute API keys into each pod's `~/.bashrc`/`~/.zshrc`
    (idempotent): per-host keys from `<keys-dir>/<provider>_api_keys.csv`
    (openai/anthropic/openrouter) **plus broadcast tokens** — a Hugging Face token
    (`--hf-token`/`HF_TOKEN`, sets `HF_TOKEN` + `HUGGING_FACE_HUB_TOKEN`) for **gated
    repos** (Llama 3, …) and a **Claude Code token** (`--cc-token`/`CLAUDE_CODE_OAUTH_TOKEN`).
    `--include`/`--exclude` (name or id) scope it to specific pods. Confirms first;
    `--dry-run` lists what would be set (values redacted). Both broadcast tokens are
    env-introducible (e.g. `CLAUDE_CODE_OAUTH_TOKEN=… arena pods copy-keys`). Each pod's
    write has a 60s budget, so a wedged pod reports `✗ <name>: … timed out` instead of
    hanging the command.
  - `pods copy <file> [dest]` — scp a local file to every pod (concurrent; `--include`/
    `--exclude` to scope). With no `dest` it **mirrors the path under the ARENA repo**
    (a local `…/ARENA_3.0/foo/bar.py` → `/root/ARENA_3.0/foo/bar.py`); otherwise `dest`
    is the remote path (trailing `/` = into that dir). Creates the remote parent dir and
    **verifies the file landed** (size check; flags a silent scp non-write or a
    misplacement when the dest is actually a directory) rather than trusting scp's exit
    code. Per pod it runs mkdir → scp → check and stops at the first failure; the scp
    has a 10-min budget, the mkdir/check 20s each. Confirms first; `--dry-run` previews.
  - `keys gen|list|rotate|revoke` — manage **OpenRouter** runtime keys via the
    provisioning API (needs `OPENROUTER_PROVISIONING_KEY`). `gen [machines|--all]` mints
    one key per machine (named `<prefix>-<machine>`) with a USD cap (`--limit`, default
    `OPENROUTER_KEY_LIMIT` or $5) and writes `keys/openrouter_api_keys.csv`; `--copy` also
    pushes them out via `copy-keys`. `rotate <machine|--all>` deletes + re-mints (leak
    recovery), `revoke` deletes only, `list` shows names/limits/usage for **this
    iteration's** `<prefix>-*` keys (noting how many others were hidden; `--all` shows
    every key on the account). Keys are found by
    name, so no local hash bookkeeping. `keys which` shows the local keys file. (`copy-keys`
    is the *distributor*; `keys` is the *generator*.)
  - `gpus` — list the GPU types for `--gpu`: RunPod's **full live catalog** (via GraphQL;
    on `RUNPOD_API=v2` via `GET /v2/catalog/gpus`, STOCK then for the configured cloud,
    falling back to GraphQL) when on RunPod with a key, else the local presets. Shows
    VRAM, **live** community/secure $/hr and RunPod's 1-GPU stock hint (`~` marks a preset estimate). If RunPod rejects
    the priced query (HTTP 200 with errors, or a non-auth 4xx/5xx), the plain catalog
    query is tried before falling back to presets. `--json` emits rows
    `{id, display_name, memory_gb, community_price, secure_price, stock_status, creatable,
    source, price_source}` (incl. catalog entries the create API rejects, `creatable: false`).
  - `ssh-config [--proxy] [--out]` — emit the **participant-facing `~/.ssh/config`**:
    direct pod endpoints by default, or stable proxy ports (`--proxy`) anchored to each
    machine's `MACHINE_NAME_LIST` index. Read-only.
  - `pods setup` — provision pods over SSH (ordered steps in `--help`): copy the git
    deploy key, write `~/.ssh/config` + `authorized_keys`, point the repo at GitHub,
    update submodules, write `~/.name`, and export any set **broadcast tokens** (Hugging
    Face for gated-repo access, Claude Code; via config or `--hf-token`/`--cc-token`) —
    else those steps are skipped and it says so. It also **auto-distributes per-host API
    keys** if any
    `keys/*_api_keys.csv` exist (reporting what it added, or that none are set up) — to
    exactly the pods that just provisioned successfully, never one that failed or timed
    out — so a `setup` (or `up --setup`) makes pods fully ready. Confirms first (`--dry-run` previews,
    token redacted). Uses `GIT_SSH_KEY_LOCAL/REMOTE`, `ARENA_REPO_OWNER/NAME`, `DEFAULT_BRANCH`.
    The repo update fetches **only the default branch, without tags** (a bare `git fetch`
    would pull every participant's autocommit branch); a tracked non-default branch pulls
    just its own upstream. Pods run in parallel and **every step has a time budget** —
    copies 60s, the image config step 300s, the hetzner bare-VM script 1800s; `--timeout
    <secs>` (or config `SETUP_TIMEOUT_SECS`, 1..86400; `up`/`replace`/`migrate copy` use the
    config value and reject a bad one *before* creating anything) overrides the
    main-step budget. A wedged pod prints `✗ <name> (timed out at <step> after Ns)` and the
    others finish normally; a timed-out `ssh`/`scp` is stopped (SIGTERM, so scp also stops
    its ssh transport; SIGKILL 2s later). Connection refusals right after create are still
    retried for ~150s (sshd booting); an auth failure (`Permission denied (publickey)`)
    fails at once.
  - `config check | set | which` — `check` is the read-only doctor (keys + setup
    readiness); `config set KEY VALUE` writes a key (e.g. an API key) into config.env —
    or give just `KEY` and **pipe the value on stdin** (`printf %s "$TOK" | arena config
    set HF_TOKEN`, script-friendly, keeps secrets out of `argv`/`ps`), or with no args
    prompt interactively (the picker shows which keys are already set); `config which`
    shows the active config file (path,
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
`remote` (the `Remote` trait every pod-SSH path goes through: `SshRemote` for real, a
scripted `FakeRemote` in tests), `metrics` (nvidia-smi parsing + per-pod aggregation), and
`provider::build` (the one factory that constructs a backend by name — used by both the
CLI and TUI).

All four target verticals are now in place: multi-provider spin-up (RunPod/Vast/
Hetzner), commit/backup, the GPU/progress dashboard, and proxy/port-forwarding.

## Safety model (this is developed against live production)

- Runs as the unprivileged `dev` user, which **cannot read `/root`**. It only sees
  a read-only copy of config at `/home/dev/prod-ro/config.env`.
- **Read-only by default.** `pods list`, `gpus` and the TUI only ever read: REST GETs plus
  read-only GraphQL queries (a POST, but a query, never a mutation). The RunPod key is sent
  only as an `Authorization: Bearer` header — never in a URL, which errors would print.
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

### Targeting & concurrency (current state)

Target-selection syntax is **not yet uniform** across `pods` subcommands (a cleanup is
planned). Until then, here's exactly how each picks pods and whether it fans out
concurrently or runs one pod at a time:

| Command | How to target pods | Execution |
| --- | --- | --- |
| `run`, `test` | **always all** (no scoping flag) | parallel |
| `test --deep` | positional names (default all) | parallel |
| `pull`, `setup`, `init-branches` | **always all** (no scoping flag) | parallel |
| `backup` | one `[target]` **or** `--all` | parallel |
| `cp`, `copy-keys` | `--include`/`--exclude` (default all) | parallel |
| `list --probe` | all | parallel |
| `set-branch` | one `[target]` **or** `--all` | parallel |
| `stop` | one `[target]` **or** `--all` + `--include`/`--exclude` | serial (provider API) |
| `terminate` | one `[target]` **or** `--all` | serial (provider API) |
| `restart` | one `[target]` only (**no `--all`**) | n/a |
| `create` | positional `names` + `-n`/`-a` | serial (capacity backoff) |

Notes / sharp edges to know:
- `run`/`test`/`pull`/`setup`/`init-branches` can't be scoped to a subset — it's the
  whole fleet or nothing.
- `restart` can't target the fleet (single pod only).
- **Every pod-SSH call has a time budget**, so one wedged pod can't hang a fleet command:
  it reports `✗ <name>: timed out after Ns`, counts as a failure (non-zero exit) and the
  other pods carry on. Budgets: quick probes 20s (`list` GPU probe, `cp` mkdir/check,
  replace/migrate identity/marker checks), `test` 90s, `test --deep` 150s,
  `set-branch`/`init-branches` 2 min,
  `backup` git push 5 min, `cp` scp 10 min, `run` `--timeout` (default 30 min), the
  replace/migrate direct pod-to-pod copy 2 h (a copy that runs out stops the replace —
  nothing swapped, re-run to continue — rather than redo it via local staging);
  `setup`/`copy-keys` as described above. rsync transfers (`pull`, the replace/migrate
  via-local copy) have no budget yet.
- A non-empty `--include` that matches nothing now **errors** (not a silent no-op);
  `copy-keys` warns by name about any reachable pod that matched no per-host key.

### Overriding config without editing it

Any key in `config.env` can be overridden by an **environment variable of the same
name** (env wins; it can't introduce brand-new keys — except a short allowlist such as
`ARENA_START_DATE`, `SSH_PROXY_RELOAD_CMD` and `RUNPOD_API`, e.g. `RUNPOD_API=v2 arena pods
list` to try the RunPod v2 backend for one command). This is how you point at an SSH
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

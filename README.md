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
    `RUNPOD_API=v2` (default `v1`; any other value is an error, which quotes the value only
    if it's short and version-like — never a pasted key; `config check` shows which is
    active). Same provider name and commands. Differences: real lifecycle statuses
    (`PROVISIONING`/`STARTING`/`RUNNING`/`EXITED`/`ERROR`); the SSH endpoint comes from
    `ssh.direct` only; create always sends `cloud` (v2 defaults to SECURE) and merges the
    account's registered SSH keys into `PUBLIC_KEY` (v2 skips them when it's set);
    `replace` recovers GPU type + cloud tier. Rename and maintenance still use GraphQL.
    Neither API reports a pod's CUDA constraint, so `replace`/`migrate copy` re-apply the
    configured `ALLOWED_CUDA_VERSIONS` to the replacement (as `create` does).
  - `provider::vast` — Vast.ai REST backend against the same trait. Vast rents
    *offers* rather than named pods, so `create_pod` searches the marketplace
    (`PUT /search/asks/`, filters wrapped in `{"q": …}`) for the cheapest rentable offer
    matching the spec — exact GPU count, disk, CUDA floor from `ALLOWED_CUDA_VERSIONS`, open
    ports, **verified hosts only** (the vast CLI's base filters; `VAST_ALLOW_UNVERIFIED=1`
    admits unverified/deverified hosts — the verified market for a card can be empty), and
    `--max-price` (checked again at create: offers move) — and rents it with runtype
    `ssh_direc ssh_proxy` (what the vast CLI sends for `--ssh --direct`), carrying the machine
    name as the instance `label`. An offer's price is what it bills with *our* disk: the
    larger of `dph_total` and `dph_base` + `storage_cost` for `DISK_GB`. GPU types map
    to Vast's spaced display names (`NVIDIA GeForce RTX 3090` → `RTX 3090`, `RTX 4000 Ada
    Generation` → `RTX 4000Ada`; `RTX_3090` finds nothing). Vast's SSH launcher replaces
    the image's entrypoint; the cohort keys are attached to **each instance**
    (`POST /instances/{id}/ssh/`, idempotent) and written by its `onstart` (which also
    turns off Vast's auto-tmux) — never registered on the Vast account. Pods are listed
    with the **direct** SSH endpoint (host IP + mapped 22), falling back to Vast's SSH proxy
    only for a running instance without one, and carry Vast's `machine_id`. A taken offer
    (`no_such_ask`, 410, "not your own") rents nothing, so the next fitting offer is tried
    (3 at most), then placement moves on; any other refusal stops, and so does a rent whose
    outcome is unclear (any 5xx, no instance id, the connection lost after sending) — rather
    than risk a second instance. The key attach after a rent is bounded (60s).
  - `provider::hetzner` — Hetzner Cloud backend for **CPU-only** VMs. Not a GPU
    container host, so it ignores the GPU-centric `PodSpec` fields and takes its
    sizing/OS/location/SSH-keys from `HETZNER_*` config. VMs come up on a real public
    IP with SSH on `:22`, so they use the same proxy/`pods up` flow as GPU providers.
  - `naming` — next-free machine-name allocation, mirroring the legacy logic.
  - `snapshot` — one read-only picture of the fleet (`FleetSnapshot`, built by one pure
    `build` that the CLI and the TUI share), the **health cache** of the last deep check per
    pod, and the allowlisted `PublicSnapshot` for the web page.
  - `teardown` — the pure judge behind `teardown --check`: listings, volumes, keys, cron/`at`
    and proxy inputs in, a `✓ ✗ ? –` checklist with fix commands out (unknown ≠ empty).
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
  - `pods list | create | stop | restart | terminate` (`--provider
    runpod|vast|hetzner`; Vast reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY`
    + `HETZNER_*`). `restart`/`terminate` take **one** pod (name, bare name, `@name` or id —
    a name two pods share is refused: pass the id); `stop` takes the shared
    [target selection](#targeting--concurrency).
    `list` shows NAME PROVIDER ID STATUS GPU (`count×type`) $/H ENDPOINT MAINT (the
    host's RunPod maintenance window, e.g. `maint 10-09 02:00→06:00 UTC`; the host's
    free-text note is flattened onto one line) and a footer
    `fleet: $X/h across N billing pod(s)` summing the **billing** pods (Hetzner's € shown
    separately, unpriced pods counted). "Billing" is one rule (`status::is_billing`):
    running or on its way up — RunPod v2 `PROVISIONING`/`STARTING`/`ERROR` too, Hetzner
    `initializing`, Vast `loading` — not `EXITED`/`STOPPED`/`TERMINATED`/`off`; a Hetzner
    server bills while it exists, powered off included. A non-billing pod's `$/H` shows `-`
    (`--json` keeps the raw `cost_per_hr`). GPU/$/maintenance come from one extra read-only
    RunPod GraphQL query per `list` (best-effort: if it fails you get one warning line and
    the list still renders); the `nvidia-smi` probe over SSH (default for the table,
    `--probe`/`--no-probe`) overrides the GPU when a pod answers. `list --json` emits the
    pods incl. `gpu_count`/`cost_per_hr`/`maintenance`. **`restart` WIPES a RunPod pod's
    container disk** (live finding: the container is reset to its image — everything
    outside a `/workspace` volume, `~/.name`, setup's git remote and keys are gone; Vast's
    stop+start is treated the same, unverified; a Hetzner hard reset keeps the VM disk). The
    prompt says what's lost and what survives (the volume size comes from the pod's spec);
    unless the ARENA repo sits on a confirmed `/workspace` volume, the restart is **refused
    unless `--wipe-ok`** (even with `--yes`): a volume elsewhere doesn't save the
    participants' work. With a volume, `setup` puts the repo on it (see `pods setup`:
    `BACKUP_REPO_PATH`, default `/root/<ARENA_REPO_NAME>`, becomes a link to
    `/workspace/<repo>`), so the gate asks the pod (read-only, 20s) where its repo **really**
    is — `readlink -f` + is `/workspace` mounted — and lets a repo linked onto the mounted
    volume through without `--wipe-ok`; no answer → judged by the configured path, as before
    (refused). Pods without a volume are never asked. Afterwards it waits for the endpoint to settle, **re-runs `setup`** on the pod
    (`--no-setup` skips) and syncs the proxy (`--skip-proxy`), so it comes back usable; if
    that setup fails the error says to run `pods setup <name>`, not another restart. `stop`
    gets the same gate (`--wipe-ok`): a stopped RunPod pod keeps no data. `terminate
    --all` tears down the **whole fleet** (confirms first; `--dry-run` lists every
    pod without touching them) — for end-of-program teardown (`terminate <name> --all` is
    refused). `terminate … --revoke-key` also deletes each terminated machine's OpenRouter
    key(s) (found by the name `keys gen` gave it, over the full paginated key listing; needs
    `OPENROUTER_PROVISIONING_KEY`) and drops its row from `keys/openrouter_api_keys.csv` —
    only for pods that actually terminated, and never for a name another pod still holds (a
    double create's twin removed by id keeps the survivor's key); a failed revoke keeps the
    row, is named, and makes the exit non-zero without stopping any terminate.
    `stop apple..mayor` / `stop --all --exclude bloom` stops many at once (needs targets or
    `--all`): every selected pod in a billing state (running, starting/provisioning/…, or
    ERROR) is stopped, others are skipped with a note. (The legacy stop→wait→delete
    `kill` is gone: `terminate` deletes directly.)
  - `rename <old> <new>` / `rename --from-prefix <p>` renames the pod (metadata only, no
    restart) and then brings along what's keyed by the name: rewrites `~/.name` over SSH
    (setup's exact `export MACHINE_NAME='<short>'` line; an unreachable pod is reported with
    a `pods setup <name>` hint, never fatal), moves the machine's row in the keys CSV (the
    per-cohort file the symlink points at — or, after a prefix change, *into* the current
    prefix's file, repointing the symlink and carrying the other live pods' rows, so the next
    `keys gen` doesn't strand them) and renames its key on OpenRouter in place
    (`PATCH /keys/{hash}` `name`; skipped quietly with no CSV / no provisioning key; a new
    name that already has its own row or key is left alone and warned about), then syncs
    the proxy. The pod's `MACHINE_NAME` env var keeps the old name (env only changes with a
    reimage) — the output says so. `--dry-run` lists every step.
  - `create` also takes **explicit names** (`pods create apple bloom`) and an
    **`--image`** override, alongside `-n`(target total) / `-a`(add). Bare names get
    the configured prefix; names already present are skipped.
  - `create`/`up` take `--retry-mins <M>` (`--retry-secs`, default 60) to **keep
    topping up to the target** while capacity is short — one round per interval for up
    to M minutes, **Ctrl+C** stops early keeping what was made. A `no instances
    available` capacity error is recognized as such (it waits), not treated as fatal.
    A round starts only while it still fits in the window (no create after it closes).
    The confirm prompt lists the exact pod names about to be created.
  - **`--gpu` is checked** (`create`/`up`/`offers`/`replace`/`migrate copy`, RunPod only):
    each token — an alias (`3070`, `4080super`, `L4`, `2000ada`, `A4500`, …), an exact id
    (any case) or a unique catalog short name (`RTX 3070`, `H100 SXM`) — must be in RunPod's
    live GPU catalog, else the command stops before anything is created: ``--gpu: `3070x`
    isn't a RunPod GPU type — did you mean `NVIDIA GeForce RTX 3070` (RTX 3070)?``. If the
    catalog can't be fetched it warns and passes the flag through unchecked. Vast/Hetzner:
    passed through as before.
  - **Multi-option placement** (`create`/`up`): `--gpu A4000,4000Ada,3090 --cloud
    community,secure --max-price 0.5 [--order cheapest|listed]`. The gpu × cloud options
    are priced (RunPod's live catalog — v2 `/catalog/gpus` per tier on `RUNPOD_API=v2`, else
    GraphQL — falling back to preset estimates, shown `~`), capped per pod (price × `--gpus`;
    with a cap an option with no known price is dropped, not risked) and ordered: `cheapest`
    (default; a price tie goes to the better stock hint, then the listed order) or `listed`
    (GPU-major). Stock never filters — creating is the truth. Per name the options are tried
    **one create at a time** (never two creates for one name); a capacity error skips that
    option for the rest of the round; a v2 create `403` ("no access to the requested pool")
    skips it for the whole run (`[no access]`), and if *every* option is refused the run
    aborts as an access problem; auth aborts, any other error stops as before.
    `--retry-mins` re-runs rounds (blocks cleared, fleet re-listed first so a name that
    appeared meanwhile — or a met `-n` target — isn't created again) only while the next
    round would still start inside the window; Ctrl+C between rounds keeps what was made. Ends with a per-name table: `created on 1×RTX 3090 COMMUNITY
    ($0.22/h) (after 1×RTX A4000 COMMUNITY: capacity)` or `not placed (tried: …)`. Cloud
    tiers exist only on RunPod (Vast/Hetzner collapse them, and Hetzner the GPU list too,
    with a note). Vast options are priced **live**: one marketplace search per GPU (needs
    `VAST_API_KEY`), the cheapest fitting offer's price for the disk asked for — a
    snapshot, since each create searches again (never above `--max-price`); a GPU with no
    offer right now is unpriced, with a note. Hetzner's option is unpriced.
    `--keep-trying` stays single-option (use `--retry-mins`). One `--gpu`, one `--cloud` and
    no `--max-price` is exactly the old single-spec path; a list that collapses to one
    option (`--gpu A4000,a4000`, `--cloud community,`) takes it too, creating the parsed
    option (never the raw list text). `--dry-run` prints the option
    table and the names it would attempt; the option order is fixed once confirmed.
  - `offers [--gpu …] [--cloud …] [--max-price …] [--gpus N] [--order …] [--json]` —
    read-only: the same option table (OPTION, CLOUD, $/H/POD, PRICE source, STOCK, plus what
    the cap dropped and why), i.e. what `create`/`up` would try. `--json` = the plan.
  - `pods up -n N` — one-command spin-up: create, then **one independent pipeline per
    pod** (never a batch): wait for *its* SSH endpoint and sshd answering (`--timeout`,
    default 600s, per pod)
    → sync the proxy (one writer at a time) → provision it (`--no-setup` skips) →
    [`--check`: deep check] → copy its API keys (when `keys/*_api_keys.csv` exist) →
    `[name] READY after 4m10s — …` or `[name] FAILED <stage>: …`, printed the moment that
    pod is done — a slow pod never holds up another. One fleet listing per `--interval`
    serves every pod's endpoint wait. Ends with the usual one-line proxy sync and a `NAME
    GPU $/H PROXY PORT HEALTH STATUS READY AFTER` table (READY AFTER = first create →
    confirmed ready, replacements included: start participants' clocks at READY), plus a
    line per name that failed, warned or was replaced — a requested name that got no pod is
    a `FAILED create` row; exits non-zero unless every requested name is READY (and when
    nothing was created at all). A pod that fails is left running — one is only ever
    terminated by `--check`. Without a proxy layout, a READY pod's `~/.ssh/config` fleet map
    is rewritten at the end if pods came up after it.
    **`--check`** deep-checks each pod after setup (`pods test --deep`). A FAIL is a bad
    host: the pod is terminated, confirmed gone from its own provider's listing (never two
    pods per name), and the name recreated from the same `--gpu/--cloud/--max-price` options
    (or the one configured spec) — options that haven't failed first, waiting for capacity
    per `--retry-mins` or `--keep-trying` — and run through the pipeline again; a replacement
    that lands on a machine IP that already failed (any name's) is rejected unseen and
    terminated (even with no attempt left). Up to `--check-attempts N` placements per name
    (default 2); the last pod that FAILed its check is left running for a look. Only a check
    whose script ran can condemn a host: one that couldn't run (SSH dropped, timed out) is
    rerun once SSH answers, and if it still can't run the name FAILs with the pod left
    running. A WARN counts as ready (shown in HEALTH). **Ctrl+C** stops every pipeline where
    it is, starts nothing new, terminates nothing, and reports — also when pressed during
    the create's retry wait (the pipelines then start stopped).
    So `pods up -n 28 --gpu A40 --cloud SECURE --disk 200 --retry-mins 60 --check` is a full
    start-of-iteration spin-up. Confirms first (`--dry-run` previews the pipeline);
    `--no-wait` skips everything after the create (not with `--check`).
  - Batch create (`create`/`up`) uses **typed provider errors** (`ProviderErrorKind`):
    on **capacity** exhaustion it stops gracefully and keeps the pods it got (e.g.
    "created 6 of 10") rather than erroring — `--keep-trying` instead waits and
    retries; on **auth** failure it aborts immediately. Already-created pods are
    never rolled back. **Transient** failures (429 / 5xx / connect-timeout) are
    retried automatically with exponential backoff (`retry` module) around create
    and list calls — so a throttle or blip doesn't fail the command. Every backend reads a
    response's **status before its body** (`http` module), so a bad key's `401` with an HTML
    or empty body is `Auth` (`… HTTP 401 Unauthorized: …`), never "error decoding response body".
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
    that leaves the list loses its forward on the next tick. `--snapshot <DIR>` adds a
    `*/2 … snapshot --public --out <DIR>/fleet.json` line (`flock -n`, log
    `~/arena-snapshot-cron.log`) for the [fleet page](#publishing-the-fleet-page).
  - `pods backup [targets]` — the **full save**: git-push the ARENA tree **and** rsync the
    home to the local backups folder (`pull`). The git push is on **whatever branch the
    pod is on** (never switches/creates one, so bespoke branches are respected) and
    **skips `main`/`master`**; clean trees report `NO_CHANGES`. Selected pods, or all.
    `--no-pull` = git only; `--message` overrides the commit message. Confirms first
    (`--dry-run` previews both). Each pod's git push has a 5-min budget. To stage onto a
    dated autocommit branch, run `pods init-branches` first.
  - `pods set-branch <branch> <targets|--all> [--hard]` — switch pods' ARENA checkout to a
    branch. Gentle by default (fetch + checkout + ff-only pull — fails on a diverged/dirty
    tree rather than clobbering work). **`--hard` is destructive**: force the branch to
    match `origin/<branch>`, discarding local commits/changes (untracked files survive) —
    e.g. `set-branch main --all --hard` resets the fleet to `main`. Confirms first;
    `--dry-run` previews. Pods switch concurrently, 2-min budget each.
  - `pods init-branches` — create each pod's `autocommit-…-wNdM-…` branch and push it
    upstream **without committing** (legacy `init_branches`), so a new day's branch
    exists before `backup` runs. `--week`/`--day` override; `--dry-run` previews. 2-min
    budget per pod.
  - `pods run [-t <targets>] [--timeout <secs>] <cmd>` / `pods test [targets]` — run an
    arbitrary command on every (selected) pod (concurrent, confirms first naming the pods;
    each pod gets `--timeout`, default 1800s = 30 min — flags go *before* the command: a bare
    `-t`/`--exclude`/`--on`/`--gpus`/`--dry-run` after it is refused rather than run on every
    pod, so quote a command that takes one, `run 'tmux kill-session -t lab'`; on a timeout the local ssh is killed, and the remote
    command dies at its next write to the closed connection) / the read-only
    torch-version health check (90s per pod).
  - `pods run --background [-t <targets>] <cmd>` / `pods jobs [targets] [JOB] [--kill JOB]` /
    `pods logs [targets] [JOB] [-n N] [-f]` — **detached course-test runs**. `--background`
    (confirms first; `--dry-run` shows the wrapper) starts the command on each pod with
    `setsid -f nohup` (not `&`, which would start it with SIGINT/SIGQUIT ignored — no
    `KeyboardInterrupt`; only a `setsid` without `-f` falls back to that), under `pods
    run`'s shell + conda env (plus `PYTHONUNBUFFERED=1`), and returns at once: `[n/N] ✓
    <pod>: job <id> (pid …)`. A job that can't record its pid doesn't run (`exit 125`); no
    pid within 5s is reported as "may have started anyway — check `arena pods jobs` before
    retrying", never as a clean failure. One id per run, e.g.
    `20261008-142301-pytest-x` (UTC start + command slug); everything stays on the pod
    under `~/.arena/jobs/<id>/` (`cmd`, `run`, `log` = stdout+stderr, `pid`, `started_at`,
    `exit`), so it survives your SSH session and any operator can look — but not a pod
    restart (container disk), and logs are never rotated or pruned. No time limit;
    `--timeout` is refused with it. `pods jobs`: a table of each pod's jobs, newest first
    (`running (pid N)` — the pid must still be that job's wrapper, not a recycled one — /
    `exit N` / `lost` = ended without an exit code, or still no pid after a minute;
    `starting` before that). `pods logs`: per pod, the job's status
    and its last `-n` lines (default 20, of at most the last 1 MiB; JOB defaults to each
    pod's newest; a JOB-shaped word among the targets is the job). `-f` re-reads every 3s
    from where it left off (exact byte offsets, lines printed once, `[pod]`-prefixed with
    several pods) until every job has ended, then exits non-zero unless all were `exit 0`;
    a pod that fails 5 reads in a row is given up on; Ctrl+C stops following, never a job.
    `pods jobs --kill JOB` (confirms) sends SIGTERM to the job's process group where it is
    running (→ `exit 143 (SIGTERM)`); no SIGKILL follow-up. Log text is stripped of
    escape sequences/control characters before printing. Calls are bounded (30s per pod;
    the start may have happened if it timed out — the report says so).
  - `pods test --deep [targets] [--json] [-v]` — the **is-this-pod-usable** check (read-only),
    for what a plain `import torch` misses on a bad host. One embedded script per pod
    (one SSH exec, 150s budget, inside the conda env) measures: nvidia-smi GPUs + driver +
    CUDA version; torch import, `cuda.is_available()`, device count vs nvidia-smi; a small
    tensor op on **every GPU** (catches `cuInit` 999 / `CUDA error: unknown error`); with >1
    GPU a GPU→GPU copy that must arrive intact and an NCCL `all_reduce` across the GPUs
    (else `skipped (1 GPU)`); a 32 MiB Hugging Face download (no token sent); free disk on
    `/` and `/workspace`; host load + uptime; what setup's VS Code warm-up left (newest
    server, extensions, default interpreter — an informational `vscode` line, Pass or Skip,
    never WARN/FAIL). The provider's maintenance window comes from
    the API. **FAIL**: any CUDA/tensor/copy/NCCL error, count mismatch, missing
    torch/nvidia-smi, driver below the floor, unreachable/timed out. **WARN**: download
    < 2 MB/s or unreachable, < 10 GB free, host load above max(32, host CPUs), a
    maintenance window. Driver floor: `MIN_DRIVER_VERSION` (`none` = off), else derived from
    `ALLOWED_CUDA_VERSIONS` (13.x → 580, 12.8 → 570, 12.4 → 550, …; the lowest listed
    version wins), else no driver check. Hetzner CPU VMs skip the GPU checks (decided by
    provider only: a RunPod/Vast pod the API reports with 0 GPUs still gets them, plus a
    `provider` warning). Output: a
    `NAME RESULT GPUS DRIVER CUDA NET NOTES` table, `-v` lists every check, and a `same
    host?` line when ≥2 failing pods share a machine (a bad host breaks every pod on it): its
    IP, or on Vast its `machine_id` — never a Vast IP, which several machines can share; `up
    --check` rejects a replacement on a failed machine by the same rule).
    `--json` prints per-pod `{id, name, provider, status, checks, facts}` (no IPs) — `[]`
    when no pod could be checked; summary/notes go to stderr. Exits
    non-zero if any pod FAILs; warnings don't. Targets scope the run (a named pod with no SSH
    endpoint is a FAIL; a typo is an error). Two pods sharing a name each get their own row
    (with a warning naming their ids). Each verdict (and `up --check`'s) is also kept in
    the **health cache** for `snapshot`: `$ARENA_STATE_DIR/<prefix>/health.json`, else
    `${XDG_STATE_HOME:-~/.local/state}/arena/<prefix>/health.json` — per
    `MACHINE_NAME_PREFIX`, so the sandbox and prod never share one; owner-only, written
    atomically under a lock, keyed by provider + pod id; records of pods their provider
    listed OK without are dropped. A corrupt file is one warning and gets replaced.
  - `snapshot [--json] [--public] [--out FILE]` — the fleet in one **read-only** picture
    (one list per provider + the details query, the local proxy file, the health cache; **no
    SSH, no checks**): `pods list`'s columns plus `PROXY` (`:9500`, `:9500 stale`, `?` =
    remote proxy) and `HEALTH` (`fail 2h GPU error`). `--json` = the internal snapshot (ids,
    endpoints, costs — never publish it). `--public` = the dashboard JSON, an **allowlist by
    construction**: only *prefixed* `MACHINE_NAME_LIST` pods (`@` staff boxes and off-list
    pods are left out) by short name, with GPU, `up|starting|down`, health + age + a reason
    from a fixed vocabulary (never check text), maintenance start/end (only a complete RFC
    3339 time, re-printed in UTC — anything else is dropped), `updated_at`, `complete`. No
    IPs, hosts, ports, ids, providers, costs or keys (pinned by a leak test). `--out` is
    atomic and the public file is 0644 whatever the umask; if no provider answers — or the
    one that failed leaves no machine to show over a page that listed some — it fails and
    leaves the old file (the page shows it going stale, never "No machines.").
  - `teardown --check [--json]` — the **end-of-program audit** (read-only; deletes nothing):
    a `✓ ✗ ? –` checklist of what is still billing or scheduled, each with the exact command
    that cleans it up. Every pod on every configured provider in **any** state (a stopped pod
    keeps its name and still bills its disk; only `TERMINATED` is left out) — fix `pods
    terminate --all` only when every listing answered and every pod is this cohort's
    (`{prefix}-…`), else one `pods terminate <id>` per cohort pod; staff boxes (`@` list
    entries) and other pods are labelled and left to you, never in a fix; RunPod **network volumes** (REST v2 `GET /v2/network-volumes`, else
    GraphQL `myself.networkVolumes`; GraphQL field names taken from existing clients, not
    live-verified) with size and an **estimated** ~$/month at $0.07/GB/month — fix a `curl -X
    DELETE …/v2/network-volumes/<id>` (irreversible); this cohort's **enabled OpenRouter
    keys** with usage — fix `keys revoke <names>` (by name: `--all` only reaches machines
    that still have a pod; staff boxes' keys are only named in a note); this user's crontab — arena's block (`cron remove`) and
    hand-added lines mentioning arena (`crontab -e`); pending **`at` jobs** whose command
    (from `at -c`, never its environment) mentions arena or a legacy fleet script
    (`destroy_pods`, …) — fix `atrm <ids>` (`at` not installed is said, not failed); and
    forwards left in the **local** proxy file — fix `proxy apply`. A source that couldn't be
    read (a provider's 429, a remote proxy, an unreadable `at` job, a proxy `server` block
    arena can't parse) is `?` — **never
    "empty"** — and, like anything remaining, makes the exit non-zero; a provider with no key
    is `–` (not checked). Exit 0 only when all clear. Only this user's crontab/`at` queue
    are read (not root's, not `/etc/cron.d`); shown commands have secret-looking values
    redacted (`NAME=…`, `--flag=…`/`--flag …` named key/token/secret/pass, `sk-`/`rpa_`/`hf_`
    words; tab-separated fields too). `--json` = the same checklist for scripts.
  - `pods pull [label]` — the **file** backup (complementing the git `backup`): rsyncs
    each pod's home into `<dir>/<label>/<pod>/`, reporting files/bytes moved per pod.
    **Keeps `.git`** (so the backup is a usable repo; `--no-git` to skip), size-caps with
    `--max-size`, excludes other dotfile dirs + `site-packages`. Knobs come from flags
    else config: `LOCAL_BACKUP_DIR` (`--dir`), `BACKUP_MAX_SIZE` (`--max-size`),
    `BACKUP_REMOTE_PATH` (`--remote-path`, default `~/`). Label defaults to the `wNdM`
    iteration. Confirms first; `--dry-run` prints the exact rsync commands. A repo that
    setup **linked onto the `/workspace` volume** is still backed up as its tree (rsync `-a`
    copies a link as a link): each pod is first asked (20s) where its repo really is, and a
    linked one is left out of the pull and pulled from its real path into the same place
    (`<dir>/<label>/<pod>/ARENA_materials/`), so the backup looks as it always did; a pod that
    doesn't answer gets its repo pulled through its configured path (`ARENA_materials/`, which
    follows a link). The same holds for a `--remote-path`/`BACKUP_REMOTE_PATH` that is the
    home (`~/`, `/root/`, `.`), holds the repo (`/root` → `<pod>/root/ARENA_materials/`) or
    names it without a trailing slash (`ARENA_materials`); one that can't be judged (`$VAR`,
    a glob, `..` — rsync escapes `$`, so `$HOME/` never meant the home) is pulled as given,
    with a warning for each pod whose repo is a link. The image's checkout that setup moved
    aside (`*.arena-aside-*`) isn't backed up — only an untouched image checkout is ever moved
    aside after a reset (see `pods setup`). `pods replace`/`migrate copy` carry the repo the same way (direct and via
    local staging), into the new pod's own volume copy when it has one.
  - `pods restore <pod> [--from <label>|big] [--path <subdir>] [--dir] [--timeout]
    [--overwrite-newer] [--with-git] [--dry-run]` — push a backup (pull's layout) back onto
    **one** pod over its direct endpoint: by default its newest `wNdM` snapshot (files under
    the size cap); `--from big` = the all-files tier, `--from <label>` any other (`pull
    --label`; a refusal lists every backup there is); `--path` restores just that part (e.g.
    `ARENA_materials/chapter1`). **Never deletes** (no `--delete`), and **never reverts newer
    work**: a file on the pod newer than the backup's copy stays (rsync `--update`; each is
    named afterwards; `--overwrite-newer` replaces them too). A file it does replace is kept on
    the pod — under `/workspace/.arena-restore/<UTC time>/` when the pod has its volume
    mounted (a restart can't wipe it there), else `~/.arena-restore/<UTC time>/`; pulls
    don't back that dot-dir up. rsync `--keep-dirlinks` writes **through** the repo's link
    onto the volume (a plain push would replace the link with a directory on the container
    disk). Never pushes `~/.ssh`, the shell rc files/histories (they hold the pod's own API
    keys), `~/.name` or `.claude*`; never chowns the home; never pushes `.git` from a snapshot
    (it can lack git packs over the size cap — refs to missing objects break the repo;
    `--with-git` to push it anyway, or `--from big`). After a restore that pushed the repo's
    `.git`, the repo is checked (`git fsck --connectivity-only`, 120s) and a broken one fails
    the command, saying where the replaced files are. Refuses before touching the pod when
    the backup doesn't exist or is empty, or `--path` isn't in it. Confirms first with
    source, destination (where the repo lands, from a read-only question to the pod) and
    size — and **warns when the snapshot was written after the pod's disk was last reset**
    (the container's creation time, from `/.dockerenv`) while an older snapshot predates it:
    the */15 pull of a wiped pod overwrites the snapshot's copies of the work with the
    image's files, so it names the older one (`--from …`). 2h budget (`--timeout`), and gives
    up after 300s without I/O — a stopped restore deletes nothing, re-run to finish.
  - `pods copy-keys` — distribute API keys into each pod's `~/.bashrc`/`~/.zshrc`
    (idempotent): per-host keys from `<keys-dir>/<provider>_api_keys.csv`
    (openai/anthropic/openrouter) **plus broadcast tokens** — a Hugging Face token
    (`--hf-token`/`HF_TOKEN`, sets `HF_TOKEN` + `HUGGING_FACE_HUB_TOKEN`) for **gated
    repos** (Llama 3, …) and a **Claude Code token** (`--cc-token`/`CLAUDE_CODE_OAUTH_TOKEN`).
    Targets / `--exclude` scope it to specific pods. Confirms first;
    `--dry-run` lists what would be set (values redacted). Both broadcast tokens are
    env-introducible (e.g. `CLAUDE_CODE_OAUTH_TOKEN=… arena pods copy-keys`). Each pod's
    write has a 60s budget, so a wedged pod reports `✗ <name>: … timed out` instead of
    hanging the command.
  - `pods copy <file> [dest]` — scp a local file to every pod (concurrent; `-t <targets>`/
    `--exclude` to scope). With no `dest` it **mirrors the path under the ARENA repo**
    (a local `…/ARENA_3.0/foo/bar.py` → `/root/ARENA_3.0/foo/bar.py`); otherwise `dest`
    is the remote path (trailing `/` = into that dir). Creates the remote parent dir and
    **verifies the file landed** (size check; flags a silent scp non-write or a
    misplacement when the dest is actually a directory) rather than trusting scp's exit
    code. Per pod it runs mkdir → scp → check and stops at the first failure; the scp's
    budget scales with what's sent — 10 min plus 1s per MB × pods (all copies share your
    uplink; `-r` counts the tree), or `--timeout <secs>` — and the mkdir/check get 20s
    each. Confirms first (the preview shows the budget); `--dry-run` previews.
  - `keys gen|list|rotate|revoke` — manage **OpenRouter** runtime keys via the
    provisioning API (needs `OPENROUTER_PROVISIONING_KEY`). `gen <targets|--all>` mints
    one key per machine (named `<prefix>-<machine>`) with a USD cap (`--limit`, default
    `OPENROUTER_KEY_LIMIT` or $5) and writes `keys/openrouter_api_keys.csv`; `--copy` also
    pushes them out via `copy-keys` (targets may be MACHINE_NAME_LIST names with no pod yet;
    `--all` = every current pod). `rotate <targets|--all>` deletes + re-mints (leak
    recovery), `revoke` deletes only, `list` shows names/limits/usage for **this
    iteration's** `<prefix>-*` keys (noting how many others were hidden; `--all` shows
    every key on the account, disabled ones included — the listing walks every page). Keys
    are found by name, so no local hash bookkeeping; a machine that left the list but still
    has a key (or a CSV row) can still be targeted by name. Dry-runs show each key name and
    whether it exists. `keys which` shows the local keys file. (`copy-keys`
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
    deploy key, write `~/.ssh/config` + `authorized_keys`, **put the repo on the persistent
    volume** when the pod has one, point the repo at GitHub,
    update submodules, write `~/.name`, and export any set **broadcast tokens** (Hugging
    Face for gated-repo access, Claude Code; via config or `--hf-token`/`--cc-token`) —
    else those steps are skipped and it says so. It also **auto-distributes per-host API
    keys** if any
    `keys/*_api_keys.csv` exist (reporting what it added, or that none are set up) — to
    exactly the pods that just provisioned successfully, never one that failed or timed
    out — so a `setup` (or `up`, per pod) makes pods fully ready. A pod that refuses our key
    (`Permission denied (publickey)`) on a provider with a key API (Vast) gets the cohort keys
    re-attached to it and is set up again (up to 3×, 20s apart); elsewhere the refusal is
    reported as before. Confirms first (`--dry-run` previews,
    token redacted). Uses `GIT_SSH_KEY_LOCAL/REMOTE`, `ARENA_REPO_OWNER/NAME`, `DEFAULT_BRANCH`.
    The repo update fetches **only the default branch, without tags** (a bare `git fetch`
    would pull every participant's autocommit branch); a tracked non-default branch pulls
    just its own upstream. Pods run in parallel and **every step has a time budget** —
    copies 60s, the image config step 300s, the hetzner bare-VM script 1800s; `--timeout
    <secs>` (or config `SETUP_TIMEOUT_SECS`, 1..86400; `up`/`replace`/`migrate copy` use the
    config value and reject a bad one — and `up` missing `ARENA_REPO_*` unless `--no-setup`,
    or with `--check` a bad `MIN_DRIVER_VERSION` — *before* creating anything) overrides the
    main-step budget. A wedged pod prints `✗ <name> (timed out at <step> after Ns)` and the
    others finish normally; a timed-out `ssh`/`scp` is stopped (SIGTERM, so scp also stops
    its ssh transport; SIGKILL 2s later). Connection refusals right after create are still
    retried for ~150s (sshd booting); an auth failure (`Permission denied (publickey)`)
    fails at once.
    **VS Code warm-up** (last step, on every provider, `up`/`restart`/`replace` included):
    pre-installs what a participant's first Remote-SSH connect would otherwise download on
    the pod — the latest stable VS Code server for the pod's CPU (x64/arm64, resolved from
    the update API on the pod; checksum-verified) in the layout Remote-SSH looks for
    (`~/.vscode-server/code-<commit>` + `cli/servers/Stable-<commit>/server`, plus the legacy
    `bin/<commit>`), the extensions (`VSCODE_EXTENSIONS`, default Python + Pylance +
    Jupyter; shared by every server version, so they help even when a client is another
    release), and the conda env as `python.defaultInterpreterPath` in the machine settings
    (added only if unset; other keys kept). Skips whatever is already there; its own 300s
    budget; **best-effort** — a failure or timeout prints a warning (`✓ name (warning:
    vscode warm-up: …)`) and the pod still counts as set up. `--no-vscode` skips it for a
    run, `VSCODE_PREINSTALL=0` everywhere. (`~/.vscode-server` is a dot-dir, so `pods pull`
    and replace's home copy already skip it.)
    **Repo on the volume** (image-based pods; its own best-effort step `repo onto volume`
    before the config step, 900s budget, never a failed setup; `REPO_ON_VOLUME=0` turns it
    off): when `/workspace` is a real mount (`mountpoint`, else `/proc/self/mountinfo`) the
    repo lives at `/workspace/<repo dir name>` and `BACKUP_REPO_PATH` becomes a symlink to it,
    so a restart or stop keeps the participants' work and every git operation (setup's
    fetch/reset, `backup`, `set-branch`) acts on the volume copy. A fresh volume: refused
    while the repo is in use (a process's working directory **or an open file** in it) or
    either disk lacks the room; the checkout is copied there (complete before it counts, the
    copy stopped at 360s), refused if anything in it changed meanwhile, then the original is
    moved aside to `<repo>.arena-aside-<UTC time>` on the container disk (a reset clears it;
    on an overlay root `mv` may have to copy it — one more checkout's time and space) and
    only then is the copy put in place and linked; any failure puts the checkout back. After
    a reset (the volume kept a copy): the **volume copy wins** — but only over the image's
    **untouched** checkout (`git status` clean and nothing in it changed since the container
    was created, `/.dockerenv`'s time): it's moved aside, never deleted, the volume copy never
    overwritten, and the link recreated (the restart flow's re-setup does this). If
    participants already worked in the checkout (a provider restart, a stop + start, before
    setup ran), **neither is touched** and the warning says to copy their changes into the
    volume copy, move the checkout away and re-run setup — moving it aside would hide that
    work where no pull looks. Idempotent, under a lock (a setup whose relocation timed out
    may still be copying on the pod: the next waits 120s, and the config step waits for it
    before the repo update). Without a volume nothing changes. Anything odd —
    `/workspace/<repo>` that isn't a git checkout, the path linking elsewhere, the copy
    failing — leaves everything as it is and shows as `✓ name (warning: repo onto volume:
    …)` (the TUI's setup line too); the restart gate then stays closed. A pod restarted by its
    provider (or a stopped pod started again) shows the image's checkout until `pods setup`
    re-links it. **Rolling this out to a live fleet:** the first setup copies each repo (and,
    on an overlay root, a second time to keep the original) — run it while participants are
    idle; a save that lands mid-move is caught and the move undone, but only between checks.
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
    — the provider list API omits this), GPU%/MEM/TEMP, **BRANCH** (autocommit
    branches shortened to their `w1d2` label; `main`/others shown as-is), and progress.
    Below ~105 columns (or in the detail view's split) TEMP, MEM, BRANCH and DISK drop out
    in that order, so NAME/STATUS/GPU/$/H/HEALTH/PROXY stay whole down to 73 columns.
  - **Fleet columns from the core snapshot** — every refresh is one
    `snapshot::build` (`arena snapshot`'s builder) over the per-provider listing, the
    local proxy file and the health cache, so these read exactly as the CLI prints them:
    `$/H` (provider currency, `-` unless billing), an **M** badge for a host maintenance
    window (detail pane: window + note), **HEALTH** = the last `pods test --deep` verdict
    + age (`pass 12m`, `fail 2h`; detail pane: worst issue + the failing check's text),
    **PROXY** = the stable port, green live / yellow stale (`:9501 stale` when wide), `-`
    no forward, `?` remote proxy. The **summary bar** is `pods list`'s footer (`$` and
    Hetzner `€` kept apart, unpriced pods counted) plus per day, **led** by a yellow
    `⚠ vast failed to list — its pods (and their cost) are missing` when a provider didn't
    answer (first, so a narrow terminal can't cut it off). API calls per refresh are
    unchanged (one list per provider); the details query (GPU/$/maintenance) runs every
    60 s or on `r` (≥10 s apart); one that fails part-way still shows what it filled (as
    `pods list` does) over the last good answer, with a footer warning.
  - **`d`** deep-checks the cursor pod (or the marked set) **in the background** — no
    modal; the row says `checking`, the footer reports the tally when done, and the
    verdicts go to the same health cache `arena snapshot` reads. **`/`** marks pods with
    the CLI's selector syntax (`alpha..delta`, ids, `-x`/`!name`, `--on`, `--gpus`); a
    typo marks nothing and leaves the marks as they were, saying why in the footer; a
    valid one replaces the marks. Marks are per `provider:id` (a Vast and a Hetzner id
    can collide). `all` is refused, and since `/first..last` marks a cohort in one line,
    terminate/restart on a marked set of more than 5 pods — or of every listed pod — ask
    for `ALL`, like the whole fleet (smaller sets: the pod count). A marked-set confirm
    names the pods (the first 10, then `… and N more`), with the token prompt always on
    screen.
  - Every pod SSH call (metrics, test/run/backup/set-branch/setup, deep check) goes
    through `Remote` with a budget (90 s / 30 min / 5 min / 2 min / setup's per-step /
    150 s), so a wedged pod ends an action with `timed out after …`; only `c` (your
    interactive shell) is a plain `ssh`.
  - **Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU
    breakdown, full branch/origin/health, util/temp **sparklines**). `Ctrl-C`/`q` quit.
  - **Act** on the selected pod with `a` (restart / stop / terminate / backup / setup /
    test / run / set-branch), on the **whole fleet** with `A` (backup / setup /
    test / run / set-branch; restart and terminate only on a **marked** set), or **add pods** with `n`. `run` and `set-branch` pop a
    text-input modal to type the command / branch; `test` is read-only. Every mutation
    goes through a confirmation modal — the *only* place the TUI mutates anything.
    Lifecycle actions (restart/stop/terminate) require **typing the pod's exact name**;
    restart is destructive like terminate (red, and its modal says it wipes the container
    disk on RunPod/Vast; Hetzner: "disk kept");
    whole-fleet mutations require typing **ALL** (a marked set: its count, or `ALL` as
    above); backup/setup/test show the precise
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

### Targeting & concurrency

Every fleet command picks pods with **one selector syntax** (`arena_core::selector`):

- **names** — bare `apple`, full `arena8-apple`, absolute `@james-gpu` / `james-gpu` — or
  a provider **id**; several at once (`apple bloom`, or `apple,bloom`);
- **ranges** `apple..mayor` — inclusive, in `MACHINE_NAME_LIST` order (the proxy-port
  order); both ends must be list names, and a reversed range is an error. Absolute `@`
  entries *inside* a range are left out (personal boxes, not the cohort) — name one, or
  make it an endpoint, to include it;
- **`all`** / `--all` — every pod;
- filters: `--exclude <targets>` (same syntax, ranges too; repeatable), `--gpus N` (exactly
  N GPUs by the provider's count — an unknown count never matches), `--on
  runpod|vast|hetzner` (not `--provider`, which picks where `create`/`up` make pods).

**Typos fail loudly**: a target *or* `--exclude` that matches no pod is an error naming it
and the closest pod names, and a narrowed selection (names/filters) that ends up empty is an
error saying how each step narrowed it — nothing is touched. Targets + `--all` together is
refused, and so is an empty argument (`""`, `" "`, `","` — e.g. an unset `$POD` in a
script), which never means "the whole fleet". Dry-runs and prompts list the resolved pods. `--include X` is still accepted as an
old spelling of a target (`stop --all --include X` keeps meaning "only X").

| Command | Targets | Nothing given | Execution |
| --- | --- | --- | --- |
| `run` | `-t <targets>` (the command is positional) | every pod | parallel |
| `test`, `test --deep` | positional | every pod | parallel |
| `setup`, `copy-keys`, `backup`, `init-branches` | positional | every pod | parallel |
| `cp`, `pull` | `-t <targets>` (file/label are positional) | every pod | parallel |
| `set-branch <branch>` | positional after the branch | **refused** (targets or `--all`) | parallel |
| `stop`, `reimage` | positional | **refused** (targets or `--all`) | serial (provider API) |
| `keys gen/rotate/revoke` | positional (pods *or* list names) | **refused** (`--all` = current pods) | serial |
| `terminate`, `restart`, `rename`, `replace`, `migrate` | **one** pod (same matcher; ambiguous name refused) | — (`terminate --all` for the fleet) | n/a |
| `create`, `up` | names to create + `-n`/`-a` | — | serial (capacity backoff) |

Notes / sharp edges to know:
- A pod you **named** that has no SSH endpoint is reported, and if none of the named pods is
  reachable the command fails (`test --deep` FAILs each one); pods swept in by `--all` or
  the default are skipped with a note.
- **Every pod-SSH call has a time budget**, so one wedged pod can't hang a fleet command:
  it reports `✗ <name>: timed out after Ns`, counts as a failure (non-zero exit) and the
  other pods carry on. Budgets: quick probes 20s (`list` GPU probe, `cp` mkdir/check,
  replace/migrate identity/marker checks), `test` 90s, `test --deep` 150s,
  `set-branch`/`init-branches` 2 min,
  `backup` git push 5 min, `cp` scp 10 min + 1s per MB × pods (or `--timeout`), `run`
  `--timeout` (default 30 min), the replace/migrate direct pod-to-pod copy 2 h (a copy that
  runs out stops rather than redo it via local staging — nothing swapped; re-running
  `migrate copy` continues it, and a stopped `replace` leaves `<name>-new` running: continue
  with `migrate copy <name>` + `migrate cutover <name>`, or terminate it);
  `setup`/`copy-keys` as described above, `restore` 2h (`--timeout`; rsync gives up after
  300s without I/O), the repo-location question before a volume pod's restart/stop/pull 20s.
  Other rsync transfers (`pull`, the replace/migrate via-local copy) have no budget yet.
- `copy-keys` warns by name about any reachable pod that matched no per-host key.

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

## Publishing the fleet page

`web/fleet.html` is a self-contained status page (no external scripts or fonts, phone- and
dark-mode-friendly) that reads `fleet.json` from its own directory every minute and shows a
sortable table — machine, GPU, up/starting/down, last health check + age + reason,
maintenance window — with "updated N min ago" and a banner once the data is over 10 minutes
old (the cron stopped) or a provider didn't answer. It only displays; it can't change anything.

```bash
mkdir -p /srv/arena-fleet && cp web/fleet.html /srv/arena-fleet/index.html
arena --config /path/to/config.env snapshot --public --out /srv/arena-fleet/fleet.json  # try it once
arena --config /path/to/config.env cron install --proxy --snapshot /srv/arena-fleet      # then every 2 min
```

Then serve `/srv/arena-fleet` with any static host (an existing web server's directory, a
`python3 -m http.server`, object storage synced from it …). `fleet.json` is public-safe by
construction, so no auth is needed; health shows `unknown` until a `pods test --deep` or
`up --check` has run under the same user (the cache lives in that user's state dir — set
`ARENA_STATE_DIR` for both if the cron runs as someone else).

## Testing

```bash
cargo test --release   # everything offline: pure planners, fake providers, FakeRemote, fixtures
```

`crates/cli/tests/live_smoke.rs` is the **opt-in live smoke test** (`#[ignore]`d; one ≤ $0.30/h
community pod for a few minutes — run it before each cohort). It drives the built `arena` binary:
`pods up <free name> --gpu A4000,3070 --gpus 1 --cloud community --max-price 0.30 --retry-mins 5
--check` → `pods test --deep --json` (parses, not FAIL) → `snapshot --public` (the machine is up
with its cached health; no IPv4 literal, SSH host, pod id or prefix) → `pods rename` to a second
free name and back → `pods terminate` → `teardown --check` until nothing of it is left.

```bash
cd /home/dev/sandbox/arena-infra-rs
ARENA_LIVE_SMOKE=1 ARENA_LIVE_CONFIG=/home/dev/sandbox/config.env \
  ARENA_LIVE_BIN=/home/dev/sandbox/bin/arena-dev \
  cargo test --release -p arena-cli --test live_smoke -- --ignored --nocapture
# also: RUNPOD_API=v2 (smoke the v2 backend) · ARENA_LIVE_GPU=3070 (the --gpu list)
```

`ARENA_LIVE_BIN` (optional, recommended in the sandbox) runs the commands through a wrapper
instead of the built binary — the sandbox's `bin/arena-dev` adds its own refusal of production
keys/prefix and passes `--config` itself, so it must point at the same config as
`ARENA_LIVE_CONFIG`. Build first (`cargo build --release`) so the wrapper runs this tree.

It refuses to start unless both variables are set; the config (symlinks resolved) is not
`/home/dev/prod-ro/config.env` or anything under `/home/dev/prod-ro` or `/root`; its
`MACHINE_NAME_PREFIX` starts with `devtest` (or is named in `ARENA_LIVE_PREFIX_ALLOW`,
comma-separated — never an `arenaN` prefix); a configured proxy is a local file outside `/etc`
(resolved as the binary will write it: relative to the config's directory, `..` and symlinks
followed); and every configured provider lists, holding only `{prefix}-…` pods (anything else =
the wrong account). The binary gets a cleared environment (only `PATH HOME USER LOGNAME LANG
LC_ALL TZ RUNPOD_API`, so an exported key or prefix can't override the checked config) plus
`SSH_PROXY_RELOAD_CMD=` (write-only proxy) and a fresh `ARENA_STATE_DIR`, and runs from the
config's directory like `bin/arena-dev`. Whatever fails, a guard first terminates the pod id
the run recorded (no listing needed), then each round terminates every pod holding the run's
two names (or that id) on the providers that answer, until a listing where *every* provider
answered shows none — so another provider's outage delays the all-clear, never a terminate;
after 8 rounds without one it prints the commands to finish by hand. Only Ctrl+C gets past
it, so after an interrupted run check `teardown --check`. A COMMUNITY pod that never gets a
public IP fails as `FAILED endpoint` (see `docs/TODO.md`); the guard still cleans it up.

## Layout

```
crates/
  core/   library: config, provider trait + impls, Remote, planners/judges (see ARCHITECTURE.md)
  cli/    `arena` binary (clap); tests/live_smoke.rs = the opt-in live smoke test
  tui/    `arena-tui` binary (ratatui)
web/
  fleet.html  the public fleet page (reads fleet.json next to it)
docs/     RunPod API notes, known issues
```

# arena-infra-rs

[![CI](https://github.com/nickypro/arena-infra-rs/actions/workflows/ci.yml/badge.svg?branch=dev%2Fphase0)](https://github.com/nickypro/arena-infra-rs/actions/workflows/ci.yml)

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
    RunPod retires REST v1 on **2026-11-15**; used only with an explicit `RUNPOD_API=v1`
    (which `config check` warns about, with the date and `remove RUNPOD_API=v1 (v2 is the
    default)`).
  - `provider::runpod_v2` — RunPod REST **v2** backend (`api.runpod.io/v2`), the **default**:
    `RUNPOD_API` unset or empty means v2 (`v1`/`v2` in any case select one; any other value
    is an error, which quotes the value only if it's short and version-like — never a
    pasted key; `config check` shows which is active, `(default)` when unset). Why the
    default: v1 retires on 2026-11-15, and v1 can only ask for hosts on CUDA ≤ 13.0 (below),
    which starved RunPod placement — "no capacity" all day once the 13.0 hosts were taken.
    Same provider name and commands. Differences: real lifecycle statuses
    (`PROVISIONING`/`STARTING`/`RUNNING`/`EXITED`/`ERROR`); the SSH endpoint comes from
    `ssh.direct` only; create always sends `cloud` (v2 defaults to SECURE) and merges the
    account's registered SSH keys into `PUBLIC_KEY` (v2 skips them when it's set);
    `replace` recovers GPU type + cloud tier. Rename is `PATCH /v2/pods/{id}` with only
    `{"name"}` (live-verified restart-free — PID 1, a `/root` marker and the endpoint
    survive); the GPU catalog, prices, stock and the `--gpu` check come from `GET
    /v2/catalog/gpus` (each tier fetched once per run). Error bodies: a `422` (e.g.
    `Unknown GPU type: …`, `Request validation failed.`) is a config error — never capacity,
    never retried, message kept; a `400 Pod is locked` (stop/restart/terminate on a
    `locked` pod) is its own `Locked` kind. **Locks** are `PATCH /v2/pods/{id}` with only
    `{"locked": bool}` (live-verified: the container is untouched); the pod's `locked`
    field (in every list/GET) says which pods are — `actions` still lists stop/restart/
    terminate while locked, so it's never read for that. **GraphQL dependency inventory** (GraphQL is
    reportedly retiring too, "early 2027", unverified) — on v2 it is left only for the
    maintenance window (`enrich`, best-effort: a failure is one warning line, GPU/$ still
    shown), the live account balance (v2 has billing *history* only — which `balance`'s
    estimates use once GraphQL is gone) and, as a fallback after
    REST v2, `teardown --check`'s volume listing; see the module doc of `runpod_v2.rs`.
    Contract tests replay scrubbed recorded v2 responses (`provider/fixtures/runpod_v2/`).
    **Host CUDA is a floor**: `MIN_CUDA_VERSION` (or `ALLOWED_CUDA_VERSIONS`, now read as a
    floor — its lowest version) goes out as v2's `gpu.minCudaVersion`, so any newer host
    qualifies (an exact `["13.0"]` had made every 13.1/13.2 host — all B300 MIG ones — "no
    capacity"). REST v1 has no floor and its schema stops at 13.0: it sends every listed
    version ≥ the floor (13.1+ hosts unreachable; a 13.1+ floor or `EXACT_CUDA_VERSIONS`
    entry is refused before the request: `remove RUNPOD_API=v1 (v2 is the default)`). `EXACT_CUDA_VERSIONS` is
    the opt-in exact set (`allowedCudaVersions`; never sent with a floor). A value that isn't
    a version (`13.x`, `newest`) fails closed everywhere — every create (RunPod and Vast) is
    refused and the deep check won't derive a driver floor from it — never read as "no floor".
    v1 creates also
    send `supportPublicIp: true` (some community hosts have no public IP, leaving only
    RunPod's SSH proxy). **v2 has no such field** (its OpenAPI `CreatePodRequest`, checked
    2026-10-11, offers no public-IP option, and its schema refuses unknown keys), so a v2
    COMMUNITY pod can land on a proxy-only host (live: two 4090s did): `up` calls it a host
    failure after the 5-minute grace — `FAILED endpoint`, or replaced under `--check`;
    SECURE pods have a public IP. Neither API reports a pod's CUDA constraint, so
    `replace`/`migrate copy` re-apply the configured one to the replacement (as `create` does).
  - `provider::vast` — Vast.ai REST backend against the same trait. Vast rents
    *offers* rather than named pods, so `create_pod` searches the marketplace
    (`PUT /search/asks/`, filters wrapped in `{"q": …}`) for the cheapest rentable offer
    matching the spec — exact GPU count, disk, CUDA floor (`MIN_CUDA_VERSION` / `ALLOWED_CUDA_VERSIONS`), open
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
    sizing/OS/location/SSH-keys from `HETZNER_*` config (`create`/`up --server-type` over
    `HETZNER_SERVER_TYPE`; `--gpu`/`--gpus` are refused there, `--server-type` everywhere else).
    VMs come up on a real public IP with SSH on `:22`, so they use the same proxy/`pods up`
    flow as GPU providers. A refused create is explained from the project (read-only
    `GET /ssh_keys` + `GET /server_types`): a missing SSH key names the project's keys and
    what to set or upload; a type the location doesn't sell lists the ones it does.
  - `provider::shadeform` — Shadeform (one REST API over ~18 GPU clouds), **opt-in overflow
    only** (`--provider shadeform`, never a default): a pod is a **bare GPU VM**, not a
    container from the cohort image, and it **bills until deleted — no stop/start, no
    volumes** (`VOLUME_GB>0` is refused). Placement is Vast-like: `create_pod` reads the catalog
    (`GET /instances/types?gpu_type=…&num_gpus=…&available=true&sort=price`) and creates on the
    cheapest **in-stock** `(cloud, region, type)` entry that passes every filter, re-checked
    client-side: the cloud in `SHADEFORM_CLOUDS` (default `massedcompute` — in practice the only
    cheap cloud with CUDA-13 images; `*` = any, with a `config check` warning) and the region in
    `SHADEFORM_REGIONS`, a VM (bare metal only with `SHADEFORM_ALLOW_BAREMETAL=1`), on-demand
    (spot never), an OS image at or above the CUDA floor (`MIN_CUDA_VERSION` /
    `ALLOWED_CUDA_VERSIONS`: the newest `ubuntuNN.NN_cudaX.Y_shade_os`), a worst-case boot
    ≤ `SHADEFORM_MAX_BOOT_SECS` (1200), and a price ≤ min(`--max-price`, `SHADEFORM_MAX_PRICE`
    — default $1.00/h, the hard ceiling). A capacity refusal tries the next entry (3 at most).
    GPU names map from RunPod's ids (`NVIDIA RTX A6000` → `A6000`; an unmapped one is refused
    before any request — Shadeform answers a typo with an empty catalog). The key goes in
    `X-API-KEY` only. Every create carries a fresh **nonce tag** (`arena`, `arena-<prefix>`,
    `arena-nonce-<uuid>`): an answer that might hide an instance (a 5xx, no id, a timeout or a
    dropped connection after sending) is settled by listing for that tag — adopted if found,
    else the run stops with "may have been created" (never retried, never the next option).
    After create the instance's price is checked again: above the cap or 10 % over the quote
    → deleted at once. Root SSH comes from a **bootstrap script** (`launch_configuration`,
    run by Shadeform when the VM turns active) that authorizes the cohort keys for root and
    the `shadeform` user and writes the nonce to `/var/lib/arena/shadeform-nonce`; the endpoint
    (`ip`, `ssh_port` or 22) is withheld until the VM has been active 60 s. The create uses the
    uploaded cohort key `SHADEFORM_SSH_KEY` (default: the prefix), never silently the account's
    Shadeform Managed Key (whose private half Shadeform holds; `default` opts into it). The
    listing shows **every** instance on the account; arena only terminates, renames or
    restarts one whose name has the prefix **and** that carries `arena-<prefix>` — anything
    else is refused with no request; a rename must keep the `<prefix>-` name (else arena
    couldn't touch it again). A new instance that goes to `error` before it has an endpoint,
    or isn't up within `SHADEFORM_READY_MINS` (15) or `--timeout`, whichever comes first, is
    deleted and the name placed on the next option (`up`, with or without `--check`). A price
    that can't be read after create (`info` retried with backoff for ~30 s, then the listing)
    gets the instance deleted and the next entry tried. Setup is the bare-VM script in its
    GPU mode (`ARENA_GPU=1`: the CUDA requirements kept, `uv --torch-backend auto`, and the
    env must see the GPU). The bootstrap leaves each cohort key as one plain line in root's
    `authorized_keys` (a cloud-init `command="echo 'Please login as …'"` line for it is
    dropped). `pods stop/start` skip it — said, the rest stopped/started, and `stop` exits
    non-zero while one still bills; a selection of nothing else is refused. Restart is gated
    as a wipe (unverified); no lock; the reaper doesn't act on it yet. Balance: the
    undocumented `GET /account` (`balance`, `hourly_burn` only — its personal fields are never
    kept); any failure there is "unknown". `api get shadeform sshkeys --raw` is refused however
    the path is spelled (`v1/`, case, percent-encoding), and Shadeform output never shows a
    private key or the account's personal fields, `--raw` included. `teardown --check` lists
    the account's Shadeform volumes too. Spec and recorded fixtures: `specs/shadeform.md`,
    `crates/core/src/provider/fixtures/shadeform/`.
  - `naming` — next-free machine-name allocation, mirroring the legacy logic.
  - `snapshot` — one read-only picture of the fleet (`FleetSnapshot`, built by one pure
    `build` that the CLI and the TUI share), the **health cache** of the last deep check per
    pod, and the allowlisted `PublicSnapshot` for the web page.
  - `teardown` — the pure judge behind `teardown --check`: listings, volumes, keys, cron/`at`
    and proxy inputs in, a `✓ ✗ ? –` checklist with fix commands out (unknown ≠ empty).
  - `balance` — prepaid account balances and the fallback chain: live (RunPod GraphQL
    `myself`, Vast `users/current`, Shadeform's undocumented `account`), else an estimate from
    the last reading minus max(billed,
    metered) spend, else `?`; burn, runway, levels, the `balance.json` state and every
    surface's rendering (Hetzner postpaid); `idle` — the read-only
    `pods idle` probe script, its reply parser and the verdict/report (unknown ≠ idle).
  - `reap` — the opt-in auto-reaper (`pods reap`), pure but for its state store: the strict
    `reap.json` types and settings (`REAP_*`, file-only), the read-only probe (`idle_probe.sh`,
    framed by a nonce, indexed and `cksum`-closed so a damaged reply is never "idle") and its
    parser, classification / idle streak / due rules, the decision table (spec rows G1–G7,
    R1–R40), the typed proofs and `ReapTicket` (the only way reap code reaches a terminate),
    the ENDS column, plan table, notice, cron line and JSONL log line.
  - `lock` — pod locks (RunPod v2, `Pod::locked`): the one refusal sentence (`devtest-x is
    locked — `arena pods unlock devtest-x` first`), the up-front refusal over a selection,
    the mapping of the API's `Locked` error to it, and `pods lock`/`unlock`'s plan.
  - `apiextra` — `--api-json` / `CREATE_EXTRA_JSON`: parse (one JSON object), deep-merge
    into a backend's create body, and refuse the fields each backend sets itself (its
    `MANAGED` table + "ports must keep `22/tcp`").
  - `apiget` — `arena api get`: the provider API bases, the path guard (relative only;
    absolute URLs, `//host`, `..`, `#` refused; the built URL re-checked against the base)
    and secret redaction (env blocks, key/token/password-named fields, `rpa_`/`sk-`/`hf_`
    tokens, SSH key material, PEM blocks; the API key itself never printed).
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
    runpod|vast|hetzner|shadeform`; Vast reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY`
    + `HETZNER_*`, Shadeform `SHADEFORM_API_KEY` + `SHADEFORM_*`). `restart`/`terminate` take **one** pod (name, bare name, `@name` or id —
    a name two pods share is refused: pass the id); `stop` takes the shared
    [target selection](#targeting--concurrency).
    `list` shows NAME PROVIDER ID STATUS (`run locked` for a locked pod) GPU (`count×type`;
    a MIG slice as the slice — `1×B300 MIG 34GB`, never the parent card nvidia-smi names,
    and `up`'s placement, READY and summary lines name it the same (the live catalog's name,
    else read off the id: `NVIDIA B300 SXM6 AC MIG 1g.34gb` → `B300 MIG 34GB`);
    RunPod's GPU is its display name on v1 and v2 alike, so `--json`'s `gpu_type` doesn't
    change with `RUNPOD_API`) $/H ENDPOINT (`no public endpoint (RunPod ssh proxy only)` for
    a pod up without a public IP) MAINT (the
    host's RunPod maintenance window, e.g. `maint 10-09 02:00→06:00 UTC`; the host's
    free-text note is flattened onto one line) and a footer
    `fleet: $X/h across N billing pod(s)` summing the **billing** pods (Hetzner's € shown
    separately, unpriced pods counted; `— PARTIAL: vast didn't answer …` when a provider
    failed to list, and no "(no pods)" claim then). "Billing" is one rule (`status::is_billing`):
    running or on its way up — RunPod v2 `PROVISIONING`/`STARTING`/`ERROR` too, Hetzner
    `initializing`, Vast `loading` — not `EXITED`/`STOPPED`/`TERMINATED`/`off`; a Hetzner
    server bills while it exists, powered off included. A non-billing pod's `$/H` shows `-`
    (`--json` keeps the raw `cost_per_hr`). Under the footer (table only, `(no pods)` too)
    one `balance: runpod $32.54 live ≈4.0d · vast ~$118.30 est. ≈49.2d` line (see `balance`;
    `⚠`/`‼` with the threshold or the consequence when low/critical, `?` with the `set`
    nudge when unknown; an opt-in account — Shadeform, never in the default provider chain —
    at $0 with nothing billing there reads plainly, `shadeform $0.00 live unfunded`: it's
    unfunded on purpose, and alarms like any other once a pod bills there), observed while the list runs — within 5 s, a live read only when
    the last one is 5 min old, silent whatever happens; `--json` reads no balance at all. The maintenance window comes from one extra read-only
    RunPod GraphQL query per `list` (on REST v2, the default, the REST list already has GPU and
    $; on `RUNPOD_API=v1` that query supplies them too) (best-effort: if it fails you get one warning line and
    the list still renders); the `nvidia-smi` probe over SSH (default for the table,
    `--probe`/`--no-probe`) overrides the GPU when a pod answers. `list --json` emits the
    pods incl. `gpu_count`/`cost_per_hr`/`maintenance`. **`restart` WIPES a RunPod pod's
    container disk** (live finding: on REST v2 — the default — the container is reset to its
    image — everything outside a `/workspace` volume, `~/.name`, setup's git remote and keys
    are gone, so only `VOLUME_GB` keeps work across a restart;
    a v1 restart (`RUNPOD_API=v1`) was seen keeping it, so on v1 it is gated and set up again the same way, but
    the output only ever says it *may* have reset; Vast's stop+start is treated the same,
    unverified; a Hetzner hard reset keeps the VM disk). The
    prompt says what's lost and what survives (the volume size comes from the pod's spec);
    unless the ARENA repo sits on a confirmed `/workspace` volume, the restart is **refused
    unless `--wipe-ok`** (even with `--yes`): a volume elsewhere doesn't save the
    participants' work. With a volume, `setup` puts the repo on it (see `pods setup`:
    `BACKUP_REPO_PATH`, default `/root/<ARENA_REPO_NAME>`, becomes a link to
    `/workspace/<repo>`), so the gate asks the pod (read-only, 20s) where its repo **really**
    is — `readlink -f` + is `/workspace` mounted — and lets a repo linked onto the mounted
    volume through without `--wipe-ok`; no answer → judged by the configured path, as before
    (refused). Pods without a volume are never asked. Afterwards it waits for the endpoint to settle, **re-runs `setup`** on the pod
    (`--no-setup` skips) and syncs the proxy (`--skip-proxy`) once the endpoint has settled
    **again** (a reset container's endpoint can flap after it first settles; a forward the sync
    still couldn't route is re-synced once, then named with `arena proxy apply`; a pod that
    never got an endpoint back is waited for once, not again by the sync), so it comes
    back usable; if that setup fails the error says to run `pods setup <name>`, not another
    restart. The re-run setup moves a repo on the default branch up to origin only as a
    fast-forward of a **clean** checkout: one with tracked changes or commits of its own (the
    participant's, on the volume) is left as it is, with a warning on the pod's line —
    `pods setup --force` is the only thing that resets it. A **stopped** pod is refused (`… is stopped (EXITED) — … arena pods start …`: a
    restart needs a running pod). `stop` gets the same gate (`--wipe-ok`): a stopped RunPod
    pod keeps no data. A Hetzner `stop` asks the VM to shut down (ACPI, so it flushes its
    writes) and waits up to 90 s for it to be off; only one that isn't is powered off, and
    the output says so (`⚠ … powered off — writes not yet on its disk may be lost`).
    **`pods start <targets>`** (targets or `--all`) is the way back from a
    stop (RunPod v1 `POST /pods/{id}/start`, v2 action `start`, Vast state `running`, Hetzner
    `poweron`): only stopped pods are started (others skipped with a note), each comes back as
    a fresh image, so it waits for its endpoint to settle, **re-runs setup** (`--no-setup`)
    and syncs the proxy (`--skip-proxy`); a Hetzner VM kept its disk (no setup). A start the
    provider refuses (no free GPU left on the host) is reported per pod, exit non-zero. The SSH
    commands (`run`, `test`, `setup`, `backup`, `pull`…) skip a stopped pod saying so — `skip X
    — stopped (EXITED); arena pods start X brings it back` — even when the listing still shows
    its last endpoint. `run` / `pull` / `backup` ask the provider **once more** (after 20 s)
    for a RUNNING pod listed without an endpoint (RunPod v1 once listed one that way for ~20 s,
    and a backup tick skipped it). **`reimage`** gets the same `--wipe-ok` gate as
    restart/stop (a reimage is a fresh container on every backend; the dry run says when it
    would be refused) and the same come-back: settled endpoint, **setup again** (`--no-setup`),
    proxy sync. `terminate
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
  - `pods lock [targets]` / `pods unlock [targets]` (targets or `--all`; RunPod on REST v2,
    the default) — a **locked** pod is refused stop, restart and terminate by RunPod
    itself, from **any** client (this tool, scripts, RunPod's web console; live-verified
    2026-10-08) — the protection for a running cohort. Setup, backups, `run` and SSH are
    unaffected. Already-locked pods are left alone; a pod on a backend that can't lock
    (`RUNPOD_API=v1`, Vast, Hetzner) is reported (`not supported on … — …`; on v1 `remove
    RUNPOD_API=v1 (v2 is the default) to lock RunPod pods`) and makes the exit
    non-zero. `pods up --lock` locks each pod as the pipeline's **last** step (after
    `--check`, whose replacement needs to terminate; a lock that fails is `FAILED lock`, the
    pod left running, unlocked); `--lock` is refused up front where pods can't be locked.
    While a pod is locked, `stop` / `restart` / `reimage` / `terminate` / `replace` /
    `migrate cutover` / `migrate finish` refuse it **up front** — the whole command, before
    anything is touched — with `devtest-x is locked — `arena pods unlock devtest-x` first`;
    the API's own `400 Pod is locked` (a pod locked since the listing) reads the same. Nothing
    unlocks on its own, except **`terminate --unlock`** (e.g. end of program: `pods terminate
    --all --unlock`), which says so in its confirm text and unlocks each locked pod right
    before its terminate (a failed unlock = not terminated; a pod locked since the listing is
    unlocked and retried once). With `--all` it lifts **only the cohort's** locks
    (`{prefix}-…`): a locked staff/personal box on the same account (`@` entry, or off the
    list) is **kept** — not unlocked, not terminated — and the exit fails; to remove one, name
    it alone (`pods terminate james-gpu --unlock`). `rename` is *not* refused: the
    spec only blocks stop/reset and a rename is restart-free (unverified on a locked pod —
    if RunPod refuses it, the same sentence says so). `teardown --check` marks locked pods
    and puts `--unlock` in their fix (on the `--all` line when any provider holds one).
  - `--api-json '<object>'` on `create`/`up` (plus config `CREATE_EXTRA_JSON`, single-quoted;
    the flag merges over it): extra fields **deep-merged** into the provider's create body
    for API options without a flag — e.g. RunPod v2 `{"dataCenterIds":["EU-RO-1"]}`,
    `{"globalNetworking":true}`, a network volume `{"mounts":{"network":[{"volumeId":"…",
    "path":"/data"}]}}` (with `--volume 0`: RunPod takes a network volume *or* the pod
    volume, so one next to `--volume`/`VOLUME_GB` > 0 is refused up front),
    `{"gpu":{"minRamPerGpu":32}}`; v1 `interruptible`; Vast `price`; Hetzner `labels`/`user_data`. Objects merge key by key (env adds variables), arrays and
    scalars replace. Fields arena sets itself are **refused before any API call** with what
    sets them instead (name/label, image, GPU id/count, CUDA, cloud tier, disk,
    `mounts.persistent`/volume, `env.PUBLIC_KEY`/`MACHINE_NAME`, the start command, `startSsh`,
    `locked` → `up --lock`, Vast `runtype`/`onstart`, Hetzner type/image/location/keys) — and a
    template (`templateId`, Vast `template_hash_id`), which would fill those in behind the
    tool's back, and v1 `computeType` — and so is a `ports` list without `22/tcp`; invalid JSON
    or a non-object too. `--dry-run` prints the merged body (env values and keys redacted).
    `config check` shows `CREATE_EXTRA_JSON`'s keys. The **flag** is `create`/`up` only; the
    **config key** is a create default like `GPU_TYPE`, so it also reaches the TUI's add-pod
    form and the replacement pod of `replace` / `migrate copy` (their plans list its keys; a
    refused one stops them before anything is created). It applies to whichever provider
    creates, so keep it to fields that provider takes (an unknown field is the API's error).
  - `create` also takes **explicit names** (`pods create apple bloom`) and an
    **`--image`** override, alongside `-n` (target number on `--provider`; pods on other
    providers don't count) / `-a` (add). Bare names get the configured prefix; names already
    present are skipped. Every name-picking listing — `-n`/`-a`, explicit names, placement's
    retry rounds, `rename`, the TUI's add-pod form — must come from **every** configured
    provider: if one fails to list, the command refuses (naming the provider; a rejected key
    is named too), since a name a pod there holds would read as free and be created twice
    (live: a bogus `VAST_API_KEY` had `create delta` plan a duplicate of Vast's
    `devtest-delta`). `pods list` still shows what answered, its footer marked `PARTIAL`.
    Between `--retry-mins` rounds, a backend that is only throttled or erroring (429/5xx,
    past the listing's own retries) or whose list stalled past its 60 s bound skips that
    round — nothing is created from the partial listing — and the next round lists again;
    a rejected key ends the run.
  - `create`/`up` take `--retry-mins <M>` (`--retry-secs`, default 60) to **keep
    topping up to the target** while capacity is short — one round per interval for up
    to M minutes, **Ctrl+C** stops early keeping what was made. A `no instances
    available` capacity error is recognized as such (it waits), not treated as fatal.
    A round starts only while it still fits in the window (no create after it closes).
    The confirm prompt lists the exact pod names about to be created, and a later round
    only retries those not made yet (after a whole-fleet listing: one that exists now is
    skipped, a met `-n` target stops it) — never a name picked afresh, which under `up`
    could be one whose pod `--check` is replacing right then.
  - **`--gpu` is checked** (`create`/`up`/`offers`/`replace`/`migrate copy`, RunPod only):
    each token — an exact id (any case), the catalog's display name as `arena gpus` prints it
    (`RTX PRO 6000`, `B300 MIG 34GB` — checked before the aliases, so `RTX PRO 6000` is the
    Server Edition, as listed), an alias (`3070`, `4080super`, `L4`, `2000ada`, `A4500`, …)
    or a unique catalog short name (`RTX 3070`, `H100 SXM`) — must be in RunPod's
    live GPU catalog (`GET /v2/catalog/gpus` on v2, the default; GraphQL on `RUNPOD_API=v1`), else the
    command stops before anything is created: ``--gpu: `3070x`
    isn't a RunPod GPU type — did you mean `NVIDIA GeForce RTX 3070` (RTX 3070)?``. If the
    catalog can't be fetched it warns and passes the flag through unchecked. On REST v1 an
    id outside v1's create enum (it lags the catalog — the PRO 6000 MIG slices) is refused
    too, saying v2's create takes it: `remove RUNPOD_API=v1 (v2 is the default)`. Vast/Hetzner:
    passed through as before.
  - **Multi-option placement** (`create`/`up`): `--gpu A4000,4000Ada,3090 --cloud
    community,secure --max-price 0.5 [--order cheapest|listed]`. The gpu × cloud options
    are priced (RunPod's live catalog — v2 `/catalog/gpus` per tier (the default), once
    per tier per run, GraphQL on `RUNPOD_API=v1` — falling back to preset estimates, shown `~`), capped
    per pod (price × `--gpus`; with a cap an option with no known price is dropped, not
    risked) and ordered: `cheapest`
    (default; a price tie goes to the better stock hint, then the listed order) or `listed`
    (GPU-major). Stock never filters — creating is the truth. Per name the options are tried
    **one create at a time** (never two creates for one name); a capacity error skips that
    option for the rest of the round; a v2 create `403` ("no access to the requested pool")
    skips it for the whole run (`[no access]`), and if *every* option is refused the run
    aborts as an access problem; an option whose request the API rejects as invalid (v1's
    `400 … schema requirements`, v2's `422 Unknown GPU type`) is skipped for the run too
    (`[rejected]`; the error keeps v1's `problems` list) and the run fails once every
    option is refused (rejected or denied, in any mix — at once, not after idling out the
    window); auth aborts, any other error stops as before.
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
    `--provider shadeform`: one catalog read prices every option at its cheapest in-stock
    entry (allowed clouds, CUDA floor, `SHADEFORM_MAX_PRICE`); an option with nothing in stock
    is noted and left unpriced.
  - `pods up -n N` — one-command spin-up: create, then **one independent pipeline per
    pod** (never a batch), each started **the moment its create returns** — while placement
    may still be retrying for other names under `--retry-mins` (placed pods used to sit idle,
    billing, until the window ended): wait for *its* SSH endpoint and sshd answering
    (`--timeout`, default 600s, per pod; a pod up for 5 minutes with only RunPod's SSH proxy
    — a host with no public IP — is a host failure: `FAILED endpoint`, or replaced under
    `--check`; so is one still up behind the proxy only when a shorter `--timeout` ends, and
    a listing that momentarily lacks the pod doesn't restart those 5 minutes; on Shadeform a
    pod listed `error`, or not up within `SHADEFORM_READY_MINS` or `--timeout` (whichever comes
    first), is a failed placement —
    deleted at once and the name tried on the next option, with or without `--check`,
    within `--check-attempts` placements)
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
    **`--check`** deep-checks each pod after setup (`pods test --deep`). A FAIL on the host
    (driver, CUDA, device count, a GPU's tensor op, peer copies, NCCL) is a bad machine: the
    pod is terminated, confirmed gone from its own provider's listing (never two
    pods per name — and no replacement if another pod has taken the name meanwhile), and the
    name recreated from the same `--gpu/--cloud/--max-price` options
    (or the one configured spec) — options that haven't failed first, waiting for capacity
    per `--retry-mins` or `--keep-trying` — and run through the pipeline again; a replacement
    that lands on a machine that already failed (any name's; by the provider's machine id —
    RunPod's `machineId` from REST v1 or GraphQL — before the IP, which several RunPod hosts
    share) is rejected unseen and
    terminated (even with no attempt left). A host FAIL on a machine the provider names
    (RunPod/Vast machine id — never an IP) is also **remembered for 72 h** in the health cache
    — but only for definite, machine-level evidence: a CUDA/driver error from torch while
    nvidia-smi lists the GPU, an nvidia-smi error or missing GPUs, a GPU count mismatch, a
    wrong result (NaN matmul, garbage peer copy). Never a timeout (python's 75 s, nvidia-smi's
    10 s, NCCL), python dying before it reported, a driver below the floor, or NCCL: those
    still replace the pod in that run, and are forgotten with it. Any pod that lands on a
    remembered machine later — another name's first pod, or any pod of a later `up` — is
    turned away the same way before setup (`landed on runpod machine …, which failed the deep
    check 3h ago (<UTC time>, <pod>: cuda: …) — rejecting it`; the summary says so too);
    without `--check` it is only a warning, nothing is terminated. Vast's creates skip such
    machines' offers altogether; RunPod's create can't be told, so a pod RunPod places there
    again is just rejected and retried (within `--check-attempts` — a second `--gpu`/`--cloud`
    option draws from another pool). A machine is banned **as a whole**: a multi-GPU or MIG
    host loses all its slots, not just the GPU that failed. It is forgotten after 72 h, at
    once when a later complete check on it passes (`up --check`, `pods test --deep`), or
    ignored for one run with **`--retry-bad-hosts`** (new failures are still remembered);
    `config check` lists the remembered machines. A cache that can't be read remembers
    nothing (one warning), never fails `up`. Up to `--check-attempts N` placements per name
    (default 2); the last pod that FAILed its check is left running for a look. Only a check
    whose script ran can condemn a host: one that couldn't run (SSH dropped, timed out) is
    rerun once SSH answers, and if it still can't run the name FAILs with the pod left
    running. A FAIL that is only **software/setup** — as the facts show it: no python, python
    reporting that `import torch` fails, a CPU-only torch (`+cpu`, no CUDA in it) on a GPU pod,
    the check script never starting although SSH worked (no zsh to run it in on a bare VM
    before setup — `--no-setup` on Hetzner — which says to run `arena pods setup`; no
    `base64`) — is never replaced — a new pod boots the same image and runs the same setup, so it
    would fail the same way: the name FAILs at once (`FAILED check — software/setup failure,
    not the host — …`), the pod is left running and its machine isn't counted as bad
    (`pods test --deep` says the same under its table). Python dying before it could report
    (killed by its 75 s timeout or the OOM killer, SIGILL on an old CPU, a segfault, output
    cut off) is a host failure, replaced as before. A WARN counts as ready (shown in
    HEALTH). **Ctrl+C** stops every pipeline where
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
    pods' forwards. `migrate cutover` still treats a sync that didn't land (or didn't route the
    name to the new pod) as a failure and auto-reverts — except on a **write-only** proxy
    (`SSH_PROXY_RELOAD_CMD=""`), where nothing can be verified through nginx: said up front,
    the forward written, no 45 s probe, no revert; `replace` only terminates the old pod once
    the sync routed the name to the new one (else it keeps it and says why), and **`migrate
    finish`** now syncs first and refuses to terminate `<name>-old` until `<name>`'s port
    routes to the live pod. Provider list calls are bounded (60s), so a stalled API reads as
    "failed to list" (forwards kept), not a hang.
  - `plan check | show` — a scheduled provisioning plan (`arena-plan.json`, see
    `arena-plan.example.json`): per-day target fleets with **GPU-first fallback chains**
    (e.g. `A4000` across community→secure→vast, then `3090`, then `A5000`) and a night
    **window** + caps. `check` validates + prints the detected local time/timezone and
    every day's resolved date; `show [--date]` previews the fallback order and
    fill-to-target vs the live fleet. Read-only today; the timed executor + `arm`/`disarm`
    (which only fires inside the night window, in system-local time) are the next step.
  - `config check` — validate that the keys the selected provider + proxy + backup
    need are present (never prints secret values; exits non-zero if a required key is
    missing or any key is set to a value it can't use — every such key is a ✗ row and is
    named in the error; ⚠ rows, e.g. a key file this user can't read or no
    `ARENA_START_DATE`, are advice). Copy `config.env.example` to get started.
  - `cron install|remove|show` — manage arena's lines in your crontab (other entries are
    never touched). Each line has an identity — **backup**, **proxy**, **snapshot** — and
    `install` adds/replaces **only the lines its flags name**, keeping the rest (a later
    `install --proxy` no longer drops an earlier `--pull`/`--start-date`/`--snapshot`):
    `--backup` = `pods backup` every 15 min (`--schedule`), git-only unless `--pull` (git +
    rsync file backup each tick), `--start-date` bakes `ARENA_START_DATE` in — those three
    (and `--no-pull`, `--no-start-date`) imply `--backup`, and a bare `cron install` means
    `--backup`. Rewriting the backup line **keeps the installed one's `--pull`, start date and
    schedule** unless a flag changes them (`--no-pull` / `--no-start-date` drop one; a note
    says what was kept), so `cron install --backup` just upgrades the line — a start date it
    can't read back (a hand edit's `cd /root && ARENA_START_DATE=…`) is the one thing dropped,
    with a note (`--start-date` sets it again); `--proxy` = a `*/5 … proxy
    apply --yes` line (log `proxy.log`; a `PATH` with `/usr/sbin` so cron finds
    nginx) that catches changes made outside the CLI (dashboard terminates, restarts that
    move an endpoint) — remove it before changing `MACHINE_NAME_PREFIX`/`_LIST`: a name that
    leaves the list loses its forward on the next tick; `--snapshot <DIR>` = a `*/2 …
    snapshot --public --out <DIR>/fleet.json` line (log `snapshot.log`) for the
    [fleet page](#publishing-the-fleet-page); `--archive` = the [backup archive](#backup-archive-arena-archive)'s
    two push lines (`archive sync --tier snap` at :07/:22/:37/:52, `--tier big` at :40, under
    `nice`, log `archive.log`, each tier locked in-process; refused without this
    user's archive state, an absolute `LOCAL_BACKUP_DIR` or a start date). Every line runs under `flock -n` (a tick that
    finds the previous one still running exits instead of stacking — the backup's lock is
    `backup.lock`, held by `flock -o` itself so no stray rsync can keep it). **Locks live in
    `~/.local/state/arena-cron/`** (`backup.lock`, `proxy.lock`, …), **and so do the logs by
    default** (`backup.log`, `proxy.log`, `snapshot.log`, `archive.log`, `reap.log`), so nothing
    new appears in your home. **A line that already logs somewhere else keeps doing so**: a
    re-install keeps the installed line's `>> PATH` (say `/root/logs/cron/arena-cron.log`, a
    folder logrotate rotates; `>>PATH`, with or without `2>&1`, too) — only arena's own old
    defaults move. A line whose log it can't read (`> PATH`, `2>>ERR`, a comment after it, a
    quoted path with a space) gets the default log, and a note names that line's redirection. Every line starts with
    `mkdir -p` of the cron folder and of a kept log's folder: cron's `sh` doesn't run a command
    whose `>>` (or lock) points into a missing directory, so a deleted folder would otherwise
    silently stop the backups. arena doesn't rotate its logs: add a logrotate rule for whichever
    folder your lines log to (e.g. `/root/.local/state/arena-cron/*.log { weekly rotate 4
    compress missingok notifempty copytruncate }` — `copytruncate`, since each line holds its
    log open with `>>`). Older versions logged and locked directly in the home
    (`~/arena-cron.log`, `~/arena-<line>-cron.log`, `~/.arena-<line>-cron.lock`; one build used
    `~/.local/state/arena/cron/`): re-running your `cron install …` replaces each old line in
    place and, once the crontab is written, moves exactly the old default files the replaced
    lines named into the folder (appended if the new log exists; regular files of yours only — a
    symlink, or a new log that isn't a regular file, is left alone) and deletes their old locks,
    saying so in one `· cron logs now in …` line. The lock moved too: a hand-written job or
    script that took `~/.arena-backup-cron.lock` to stay out of the cron's way no longer
    serializes with it (take `~/.local/state/arena-cron/backup.lock` instead), and a run the old
    line started may overlap the new line's first tick — re-install when no backup is running.
    `install` and `remove` refuse a `$HOME` that isn't this user's (unset, relative, owned by
    someone else — `sudo -E` — or holding whitespace, `%` or a newline): run them as the user
    whose crontab it is (`sudo -i`). The lines carry `$HOME`, the arena binary, the config path
    and the archive's state dir unquoted, so `install` refuses any of them holding a character
    other than letters, digits and `./_+@:,=-`, naming the first one (`/srv/o'neil` or a config
    in `course(2026)/` was a shell syntax error on every tick, so arena never ran; `&`, `;`, `$`
    or a backtick would run something else). Use a path without it.
    `remove --backup|--proxy|--snapshot` drops just those; plain `remove` drops the whole
    block. Both print the block's before → after diff (every removed copy of a duplicate
    shown) and confirm (`--yes` for scripts, `--dry-run` to preview); `show` labels each line
    with its identity. Lines written by older versions are recognized by the command they
    run; comments in the block (a paused `# …` line, a note) are never matched, replaced or
    removed by a targeted install/remove.
  - `archive up|status|sync|resize|access|ssh-config|ssh|forget|unforget|down` — the **backup archive**: an
    always-on Hetzner VM with two volumes (dated snapshots, all files) holding copy #2 of
    `LOCAL_BACKUP_DIR`, pushed by cron, readable by participants once `access on`; never part of
    the fleet. See [Backup archive](#backup-archive-arena-archive).
  - `pods backup [targets]` — the **full save**: git-push the ARENA tree **and** rsync the
    home to the local backups folder (`pull`). The git push is on **whatever branch the
    pod is on** (never switches/creates one, so bespoke branches are respected) and
    **skips `main`/`master`**; clean trees in step with origin's copy of the branch report
    `NO_CHANGES` (origin's — never the branch's own upstream: a branch made from `upstream/main`,
    tracking it, is pushed like any other). Selected pods, or — none
    named (no targets, `all`/`--all`) — **this cohort's**: `<prefix>-…` pods (one off
    `MACHINE_NAME_LIST` too: an extra pod made for someone, a parked `-old` twin), never an `@`
    staff entry. The rest (staff boxes, another prefix, a pod the provider named) get one
    `⊘ skipped N non-cohort pods (…): a, b — name one to include it, or pass --include-non-cohort`
    line, never a failure; named, they're backed up like any pod (list a staff box that wears the
    prefix as `@<prefix>-name` to leave it out). `--include-non-cohort` (on `backup`, `pull`,
    `init-branches`, `set-branch`) takes every pod instead. Cohort pods on `main`/`master` (after `set-branch main`,
    until `init-branches`) get one line after the run, not a SKIP each:
    ``⚠ 12 cohort pods on main: not backed up until `arena pods init-branches` `` (it names the
    pods when only some are). Not a failure — the exit stays 0: the reset window is routine
    every morning, and a daily non-zero exit would bury the ones that mean a push broke.
    `--no-pull` = git only; `--message` overrides the commit message. Confirms first
    (`--dry-run` previews both). Each pod's git push has a 5-min budget, its rsync the
    `pull` budgets below. To stage onto a dated autocommit branch, run `pods init-branches`
    first. Two things never go into the commits, because each once kept participants' work
    off GitHub: **files over 95 MB** (GitHub refuses the push, and every later one — GH001)
    are left out (never staged, so no blob is written), stay on disk and are reported per pod
    (`skipped N file(s) > 95 MB: … covered by the rsync big tier`); a branch whose unpushed
    commits already carry a file over GitHub's own 100 MiB limit fails with the exact fix
    (`arena pods run -t <pod> 'git -C <repo> reset -q <sha>'` — keeps every file on disk; the
    next backup re-commits the work without it), never the same opaque rejection every tick (a
    95–100 MB file committed by hand is one GitHub takes: it's pushed). And **nested repos are
    never committed as gitlinks** (a commit id without its files): the image's nested clones
    (no commits of their own) are **adopted** once per checkout — the course repo tracks their
    files where they are (listed in `.git/arena-nested`), so work inside them is committed like
    everything else, while each clone keeps its own `.git` untouched: git run inside an
    exercise still works on that clone alone (never on the whole course repo), and its stashes,
    branches and reflog stay in it, backed up by the rsync tiers like every `.git` (and carried
    by `pods replace`). Nothing on disk changes; to undo, `git rm -r -q --cached <dir>` and
    delete its line from `.git/arena-nested`. A nested repo with its own history (a
    participant's project), or one cloned after that, is left exactly as it is, kept out of
    the commit (a managed block in `.git/info/exclude`) and reported every run (the rsync tiers
    have it). A gitlink an earlier backup committed is replaced by the files. A file over the
    limit staged by hand (`git add`) is taken back out of the index the same way. The script
    runs under `sh` whatever the login shell; `--dry-run` prints each pod's line — `ssh …
    'sh -s <exact arguments>' < arena-backup.sh` — and, once, the here-doc that writes
    `arena-backup.sh`: paste that first and every line runs as the backup would (without it the
    shell refuses to start, never a silent no-op). **A pasted key never blocks the push** (live:
    an OpenRouter key in an exercise made GitHub refuse three pods' pushes for ~20 hours, GH013):
    each new or changed text file is scanned for high-confidence key formats
    (`arena_core::backup::SECRET_PATTERNS`: OpenRouter, Anthropic, OpenAI, Hugging Face, GitHub,
    AWS) and committed with each key replaced by `REDACTED-…` — in git only: the file on disk
    keeps the key (the code still runs) and shows as modified; reported per pod (`redacted N
    pasted key(s) in …`). **Unpushed commits holding a key** — an older backup's, or one the
    participant made by hand — are rewritten before the push, whatever GitHub would do: just
    those commits (none origin has — one only an `upstream` elsewhere has is scanned too: origin
    never had it), the key redacted — same messages, authors, dates — and pushed as a
    fast-forward, never forced; files on disk untouched. **The course's own commits are left as
    the course published them**: on a pod whose `upstream` fetches from and pushes to the course
    repo alone (`ARENA_UPSTREAM_REPO`, default `ARENA-education/ARENA_materials`; any spelling of
    its URLs — every `git remote get-url --all upstream` and `--push --all` one), what
    `refs/remotes/upstream/main` holds is never scanned or rewritten — it's public already, and a
    redacted copy of a course commit broke its ancestry, so every later `git pull upstream main`
    conflicted on the redacted lines (review ai-review claude #4); they're pushed with their commit
    ids unchanged. Only that one ref: any other `upstream/*` ref, and every ref of an `upstream`
    with a fetch or push URL elsewhere (a pushurl of the participant's fork, where a successful
    `git push upstream` updates the ref), is scanned like the rest (review ai-review codex #1,
    claude #5). The trust boundary: the exemption trusts whatever the pod's
    `refs/remotes/upstream/main` holds — the URLs are checked, not where the ref's commits came
    from, so a ref fetched from another URL before `upstream` was repointed at the course (or set
    by hand) stays trusted until the next fetch from the course replaces it. And
    `ARENA_UPSTREAM_REPO` should name a public repo: its commits go into the cohort repo unscanned
    (claude #6), so a staff key committed to a private course repo would be pushed with them. If
    GitHub's push protection refuses one, the pod's line says it's the course's own key-like
    string, public already — allow it in the cohort repo's push protection — not a participant's
    leak. A secret the patterns don't
    know, refused by GitHub, fails the pod with GitHub's name for it and its path:line (never the
    secret, never GitHub's allow-URL). **A pod whose push itself fails** (a refusal, a big file,
    the network) **is rsynced into `LOCAL_BACKUP_DIR` in the same run — even under `--no-pull`**
    (the cron's) — and its line says where its files went (or why that failed too); the run still
    fails. A pod that fails before its push (no repo, ssh refused, a timeout) keeps its plain
    failure line and isn't copied every 15 minutes. **A pod whose `origin` isn't the configured
    repo** (`ARENA_REPO_OWNER`/`ARENA_REPO_NAME`; any spelling of it passes: scp-style, `ssh://`,
    `https://` with or without credentials, `.git`, a trailing slash, any case) is refused before
    anything else, `main` included (live: a pod nobody set up kept the upstream course repo and
    was never backed up to git, silently): nothing committed or pushed, `✗ <pod>: origin is <url>,
    not the configured <owner>/<name> — not backed up to git; run \`arena pods setup <pod>\``
    (the URL never with its credentials), and the run fails. Its work reaches no git remote at
    all, so it is rsynced like a failed push, even under `--no-pull`. `set-branch` and
    `init-branches` don't switch it, and the reaper doesn't reap it until `pods setup`. No
    configured repo: no check. **A branch with an operation in progress** — a participant
    mid-way through a conflicted `git pull upstream main` (a merge), a cherry-pick or a revert,
    a `git am` (HEAD attached; a patch that didn't apply leaves no conflict at all), a rebase that
    kept HEAD attached, a multi-commit cherry-pick or revert paused between its commits (git's
    `sequencer/` alone, once one was resolved and committed by hand), or with the conflicts a `git
    stash pop` left — is left exactly as it is (review R3: the
    backup committed the conflict markers as a merge commit and pushed it, after which `git
    merge --abort` and VS Code's conflict view no longer worked): nothing staged, committed or
    pushed (`BUSY`, exit 6), `✗ <pod>: a merge is in progress on <branch> — not committed (a
    backup now would commit the merge's state, unresolved conflicts and all, and end the merge);
    resolve the conflicts and \`git add\` them, then \`git commit\` — or abort it (\`git merge
    --abort\`) on the pod`. Conflicts are named only while some are unresolved (git refuses the
    commit until they're added); with none left (all resolved and added, or a clean `git merge
    --no-commit` / `git revert -n`) it's `… commit the merge's state and end the merge); finish it
    (\`git commit\`) or abort it (\`git merge --abort\`)`. A cherry-pick, a revert, a `git am`
    (`BUSY <branch> am`) and a rebase each get their own `--continue`/`--abort`; a paused sequence
    (`sequence`) names both `git cherry-pick --continue` and `git revert --continue`, since only
    the right one works; a `git stash pop`'s conflicts `resolve them and \`git add\` them, or
    undo them (\`git reset --merge\`)`. The run fails. The check runs again right before the
    staging and right before the commit, so an operation the participant starts while the backup
    scans is caught too (what's left is the instant before git takes its index lock: git has no
    lock that spans commands).
    It is rsynced like a failed push, even under `--no-pull`, until it's ended — but not every
    tick: a read-only look at the pod first (under its lock) says whether anything changed, and
    it is copied only when no copy since the operation started is recorded — or the recorded copy
    is gone from this machine (its `.git` no longer there), or isn't under `LOCAL_BACKUP_DIR` as
    it is now —, when its work tree changed since the last recorded copy (every file git lists:
    mtime, size, ctime and path, so a `cp -p`/`rsync -a` replacement that keeps the old mtime and
    size counts too), or when that copy is an hour old; else its line says so (`✗ <pod>: not
    backed up to git (…) — copied at 14:02 UTC to <dir>/w2d1/<pod>, unchanged since`). So a
    course push that leaves many pods conflicted can't make one cron tick outlast the next (whose
    `flock -n` would skip the healthy pods' backups too). A copy counts only when it is whole:
    every rsync of both tiers, the snapshot's uncapped `.git` pass among them (a busy lock, no
    endpoint, a timeout, a failed tier or pass: no copy). The record is `busy-copies.json` in the
    fleet's state dir (written atomically, owner-only, off the async runtime under the state
    dir's lock, waited for 10 s at most; missing or corrupt = copy again). (The reaper doesn't reap such a pod
    meanwhile: its git step fails, retried every 6 h, and the run's table and its notification say
    `a merge is in progress on <branch> — not reaped until it's finished or aborted` with how to
    end it, while `reap log` and `reap status` keep their fixed words: `git_failed`,
    `backoff_after_failure`.) The backup after that commits as always. A rebase detaches HEAD, so
    it is skipped like `main` — and said (`UNCOMMITTED HEAD rebase`), so `set-branch --hard`
    copies it first.
    **Left alone, it is finished** — `BUSY_AUTO_RESOLVE_HOURS` whole hours (default 3; `0`/`off`:
    never), the human's rule (2026-10-19: "backup files + some automated heuristic … whatever
    changes are most recent … after like 3h of no activity"), applied per file and per hunk so
    that neither side's work is silently lost. It needs all of these older than that, on the
    pod's own clock: the operation's start (its `MERGE_HEAD`/`CHERRY_PICK_HEAD`; the index for a
    stash pop's conflicts), every file git lists (tracked, and untracked but not ignored; never
    `.git`), the index (something staged), any terminal login's typing (`who`'s ttys) and any
    pseudo-terminal's use (`/dev/pts/N`, read directly: the images' containers have no utmp, and
    VS Code's, Jupyter's and tmux's terminals are no login); and nothing at work in git — no
    `.git/index.lock`, no `git` running, no process holding `MERGE_MSG`, `COMMIT_EDITMSG`,
    `git-rebase-todo` or `SQUASH_MSG` (an editor a git command waits on). Unsaved buffers in an
    editor or a notebook can't be seen: only what reaches the disk counts — the copy and the
    folder below hold the state before. It also needs a whole copy recorded after the work
    tree's last change, `.git` with it (this run's or an earlier one's, still there — never when
    this run's copy failed); a repo linked onto the `/workspace` volume counts as held by a pull
    of the home, which copies it in the link's place. Then, on the pod, under its lock, each
    conflicted path, nothing changed yet:
    - a regular text file with no conflict marker left (at the path's `conflict-marker-size`),
      both sides there, is kept as the participant resolved it;
    - one whose markers are still there but which isn't what the merge left — git's conflicted
      output made again from its stages (`git merge-file` with the file's own labels and marker
      size, `--diff3`/`--zdiff3` for base sections; else the merge's own way, `checkout
      --conflict` into a scratch tree, labels aside; with no labels to read, a file changed more
      than 2 minutes after the operation started) — leaves the whole operation BUSY: `⊘ <pod>: not
      finished automatically — you started resolving x.py by hand — finish it (and git add it) or
      abort, on the pod`;
    - else it takes the side whose last change to that path is the more recent (`git log -1
      --format=%ct <side> -- <path>`, HEAD against `MERGE_HEAD`/`CHERRY_PICK_HEAD` or the stash
      entry; a tie keeps HEAD's; a path renamed on one side counts from that side's own commit):
      a text file per hunk (`git merge-file --ours|--theirs`: that side of each conflict, the
      clean changes of both sides kept — so a course fix elsewhere in the file is never recorded
      as merged and dropped), a binary file, a symlink or a modify/delete whole, a path that side
      deleted removed.
    A file over 95 MB anywhere in that, or conflict markers the participant already staged
    (`git add` of a file still in conflict), leaves it BUSY, said. The state before is kept first
    in `.git/arena-resolved/<UTC stamp>/` on the pod — the conflicted files as they were, git's
    own `MERGE_HEAD`/`MERGE_MSG`/`CHERRY_PICK_HEAD`/`ORIG_HEAD`, the old HEAD, the index with its
    conflicts, and a README that says how to see it or do it again — which no later copy
    overwrites (the day's snapshot is updated by every pull that day). Right before anything
    changes, the state is read again (the operation and its commit, HEAD, the conflicts, the
    index's mtime and size, every file, no index lock): any difference — the participant aborted
    the merge, started a rebase, staged a file — and nothing is touched. Each staged file goes
    through the backup's key redaction (the redacted blob is committed, the file on disk keeps
    its key). Right before the commit the operation, HEAD and the index lock are checked once
    more. A merge becomes a real merge commit, a cherry-pick is `--continue`d (one left empty is
    committed empty, as git advises), a stash pop's conflicts are added (its stash entry stays) —
    in the repo's own identity, never `-c user.*` — the message saying, per path, which side won
    and both sides' last change to it, UTC to the minute (`- README.md: the merged branch won
    (its side of each conflict; the other changes of both sides kept) — last changed: yours
    (HEAD) 2026-10-19 09:12Z, the merged branch (MERGE_HEAD) 2026-10-20 13:05Z`), then where the
    state before is and today's copy. The resolve looks again first and touches nothing if
    anything changed since the look its copy followed (`⊘ <pod>: not finished automatically —
    changed: files changed since the copy`). Then the pod is backed up again in the same run, as
    any — `✓ <pod> -> <branch> — finished a merge left for 3h+ (README.md: MERGE_HEAD's side of
    each conflict, HEAD 2026-10-19 09:12Z / MERGE_HEAD 2026-10-20 13:05Z; notes.py: as resolved by
    hand; before: <repo>/.git/arena-resolved/…/ on the pod; today's copy: <dir>/w2d1/<pod>)` — and
    it isn't a failure of the run (only the pods finished come off the run's failures; any other
    pod that failed stays one). Never a revert (`⊘ <pod>: not finished automatically — arena won't
    finish a revert …`: its other side is the commit it undoes, always older, so the newer side
    would undo nothing), a `git am`, a rebase or a sequence (what they'd go on to apply is the
    participant's), nor anything unusual — conflicts not from the newest stash entry, a submodule
    in conflict, a merge of several branches, no git identity in the repo: those stay BUSY. The
    TUI's backup and the reaper's git step copy nothing first, so they don't finish one; the next
    cron backup does, and the reaper's retry then finds the pod backed up. Nothing tells the
    participant but the commit and the folder (the reaper's `~/.arena/NOTICE` is its own).
  - `pods set-branch <branch> <targets|--all> [--hard] [--init-branches]` — switch pods'
    ARENA checkout to a branch. Gentle by default (fetch origin + checkout + fast-forward to
    origin's branch, by name — fails on a diverged/dirty tree rather than clobbering work, and all
    or nothing: when the checkout went through but the fast-forward didn't (the local branch has
    commits origin's doesn't — Live 4: a local `main` made from `upstream/main` — or origin has
    no such branch), the pod goes back to the branch it was on, and its line says why and
    `back on <branch>` (or, if going back fails too, which branch it is on now); never
    the local branch's own upstream, so a participant's `feat` tracking `upstream/feat` lands on
    origin's, and an unreachable `upstream` doesn't matter). **`--hard` is destructive**: force the branch to
    match `origin/<branch>`, discarding local commits/changes — and overwriting any untracked file
    at a path `origin/<branch>` tracks (other untracked files stay) —
    e.g. `set-branch main --all --hard` resets the fleet to `main`. Confirms first;
    `--dry-run` previews. Pods switch concurrently, 2-min budget each. A nested clone the
    backup adopted (see `pods backup`) is tracked only on the autocommit branches, so a
    switch to `main` would delete it from disk and break that exercise: it is put back from
    the previous HEAD, **staged** (so switching back goes through), and named per pod
    (`restored 2 nested repos that main doesn't track (Grokking, …): on disk and staged; the
    first backup on the next day branch commits them`). `--all` = this cohort's pods, as for
    `backup` (a staff box only when named). Each
    pod's current branch is **backed up first** (the `backup` commit + push, keys redacted; a
    no-op on `main` or a branch in sync), under the pod's backup lock, held until its switch
    ends (only with an absolute `LOCAL_BACKUP_DIR` that exists: with a relative one, such as the
    legacy `./backup`, no lock is taken, and the dry run and the run say so in one line). A pod
    whose push fails is rsynced into `LOCAL_BACKUP_DIR` first, then switched, its
    line saying so (its unpushed commits stay in that branch on the pod — or, after `--hard` onto
    that same branch, under the ref `refs/arena-discarded/<UTC time>-<branch>` the switch pins them
    with); a pod that can be **neither pushed nor copied** (the copy
    failed too, the backup failed before its push, or another backup held its lock for the whole
    2-min wait) is **not switched** (`✗ … not switched: <why>`), and the run fails. A pod with
    an operation in progress on a branch the backup commits (a merge, …: see `pods backup`) is
    copied first the same way, its line saying `⚠ NOT COMMITTED FIRST: a merge is in progress on
    <branch>…`; the gentle switch then refuses to end it (`arena set-branch: a merge is in
    progress on <branch> — not switched (a switch now would end the merge); <how to end it> on
    the pod, then switch again`, exit 6 — git's own checkout would drop a merge whose conflicts
    are staged; the TUI's set-branch says the same, naming `--hard` as the destructive way), and
    `--hard` ends it, discarding its uncommitted changes to tracked files, its line saying where
    the copy went (a `git am`, rebase or paused sequence too: git keeps their state across a
    checkout, so once switched `--hard` quits it — its state only — rather than let it stop the
    next init and every backup, or a later `git rebase --abort` take the pod back off `main`). On
    `main`/`master` (or a detached HEAD) no backup commits or pushes: the backup there only says
    what a reset could discard — the first of an operation in progress, changes to tracked files
    (compared through a copy of the index, so the participant's own is never written), commits no
    remote has (one only `upstream` has is the course's, not lost), untracked files git doesn't
    ignore — and **`--hard` copies such a pod
    first** too, `.git` and all (`⚠ NOT COMMITTED FIRST: main has uncommitted changes to tracked
    files, and no backup commits on main; …`, `⚠ NOT PUSHED FIRST: main has commits no remote has,
    and no backup pushes main; … that copy has its .git, those commits with it; …`), never
    resetting it if the copy fails. Any untracked file counts: `--hard` overwrites one wherever
    the branch it resets to tracks the same path (review pr4: a participant's `solutions.py` became
    the staff's), and which paths that branch tracks after the switch's own fetch the backup can't
    know. A nested repo doesn't (the image ships clones like that), nor does an ignored file. A pod
    with nothing of its own on main — main as on origin — isn't copied, and `pods backup` and the
    cron say nothing of any of it. A pod `--hard` switched after copying such work is switched, not
    a failure; one line after the run names those pods. **Before any `--hard` reset, what it
    discards is saved on the pod** — in `.git/arena-discarded/<UTC time>/`: `changes.patch` (HEAD →
    the files), and `staged.patch` (HEAD → the index) and `unstaged.patch` (the index → the files)
    apart — a version staged and then undone in the files is in the staged one only —, the files
    with unresolved conflicts, the operation's state (`MERGE_HEAD`,
    `MERGE_MSG`, `rebase-merge/`, …), the untracked or ignored files, links and directories the
    reset removes (a directory where the branch has a file or a link goes whole — ignored
    contents, a nested repo's `.git` and all — so it's copied whole, its links kept as links;
    a big one costs its size there), and a
    README on getting them back (`git apply --index …staged.patch && git apply …unstaged.patch` on
    the commit it was on, which it names; the ref `refs/arena-discarded/<UTC time>` keeps that
    commit from git's gc — `git update-ref -d` it, and remove the folder, once it's not wanted) —
    and the pod's line says `what it discarded was saved to .git/arena-discarded/<time>/ on the
    pod first`. **The commits the reset leaves on no branch are pinned too** (review ai-review
    codex #3: `checkout -f -B <branch> origin/<branch>` moves the local `<branch>`, whichever branch
    the pod was on — a pod on a clean, pushed autocommit branch lost an unpushed commit on its local
    `main` to git's gc once the reflog expired): the local `<branch>`'s tip, when it has commits
    origin's doesn't, as `refs/arena-discarded/<UTC time>-<branch>`, and the commit HEAD was on,
    when it has commits no remote has, as `refs/arena-discarded/<UTC time>` — whether or not
    anything else is saved (then the folder holds just the README, naming them and how to make a
    branch of each again); the pod's line names the refs (`its commits kept by …`), and the
    README's last line deletes them all. In `.git`, so the reset doesn't touch it and every later copy (snapshot, big,
    archive) carries it under a name no later save reuses: the copy into `LOCAL_BACKUP_DIR` is the
    day's label, which the next pull of that label overwrites from the reset pod (review ai-review
    claude #2). Nothing to discard, no folder; a save that fails leaves the pod as it was, not
    reset (a failed switch), and removes what it had copied. `--hard` with `--include-non-cohort` is refused unless the pods are
    named: it would reset staff boxes and other cohorts' pods too. A file the
    backup committed with a key redacted keeps the real key on disk: around the switch it's set
    aside in `.git/arena-unredacted/<time>-<pid>/` (so gentle doesn't refuse it and `--hard`
    doesn't drop the key; a later set-aside never overwrites an earlier one) and put back when
    the pod returns to that branch. A gentle switch that refuses a pod mid-merge does so before
    setting anything aside, and the putting back is the command's exit trap: a switch that fails,
    stops on an error, or gets a hangup, an interrupt or a TERM puts the keys back first. arena's
    own 2-minute timeout kills only its ssh client — a non-pty session's shell is usually never
    signalled, and ran on (review ai-review claude #13) — so the switch bounds itself on the pod:
    it runs under `timeout` for 100 s (TERM, the keys put back; KILL 10 s later if still running,
    which runs no trap: keys then stay set aside until a switch back to their branch), said on the
    pod's line if it ran out; a pod without coreutils' `timeout` runs it unbounded. Its script
    runs under `sh`, whatever the login shell (zsh on the pods).
    Landing pods on `main`/`master` ends with the reminder
    ``⚠ N cohort pods on main: not backed up until `arena pods init-branches` ``;
    `--init-branches` then runs `init-branches` on every pod that switched (the start date is
    checked first) — one copied first because its push failed included, whatever failed it (the
    network, push protection, a non-fast-forward, an operation in progress): one participant's
    failed push used to leave the whole fleet on main, where no backup commits (review pr4). The
    run still fails afterwards, naming the pods copied instead of pushed. A pod that didn't
    switch (no copy, its switch failed, the gentle switch refused it, its lock stayed busy, the
    wrong origin) is left out of the init — `init-branches on the N pod(s) that switched — not on
    …` — and fails the run too; with none switched, no init.
  - `pods init-branches` — create each pod's `autocommit-…-wNdM-…` branch and push it
    upstream **without committing** (legacy `init_branches`), so a new day's branch
    exists before `backup` runs. `--week`/`--day` override; `--dry-run` previews. 2-min
    budget per pod. A pod still on another branch has it backed up first, as `set-branch` does
    (and isn't switched if it can be neither pushed nor copied). A pod with an operation in
    progress (a merge, cherry-pick, revert, `git am`, rebase, paused sequence, conflicts) is
    refused, touching nothing (`arena init-branches: a merge is in progress on <branch> — not
    switched …`, exit 6): `git checkout -b` would drop the merge and carry its conflicts onto the
    new branch, with nothing left to abort — and carry a `git am`, rebase or sequence onto it
    half-done. Landing on a day branch that exists, it puts back the real keys a switch set aside
    on it; its script runs under `sh`, whatever the login shell (zsh on the pods, where a glob
    with no match is fatal: a set-aside run left without its `.branch` ended the whole command
    before the push), bounded on the pod like `set-branch`'s switch (100 s, then a KILL 10 s
    later). No targets =
    this cohort's pods, as for `backup`.
  - `pods run [-t <targets>] [--timeout <secs>] <cmd>` / `pods test [targets]` — run an
    arbitrary command on every (selected) pod (concurrent, confirms first naming the pods;
    each pod gets `--timeout`, default 1800s = 30 min — flags go *before* the command: a bare
    `-t`/`--exclude`/`--on`/`--gpus`/`--dry-run` after it is refused rather than run on every
    pod, so quote a command that takes one, `run 'tmux kill-session -t lab'`; on a timeout the local ssh is killed, and the remote
    command dies at its next write to the closed connection; a pod whose command fails
    shows what it printed on stdout too, then `exit N: <stderr>` — never `Some(N)`; on a pod
    with no zsh yet — a bare VM before setup — it says so and to run `arena pods setup`) / the
    read-only torch-version health check (90s per pod). Every dry run's `ssh`/`scp`/`rsync`
    line is shell-quoted, so it pastes as exactly what would run (globs like `'**/.git/**'`
    don't expand).
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
    `pods jobs --kill JOB` (confirms) sends SIGTERM to every process group of the job's session
    where it is running — the wrapper's, and any a command made of its own (`timeout …` does) —
    (→ `exit 143 (SIGTERM)`); no SIGKILL follow-up. Log text is stripped of
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
    the create's CUDA floor — `MIN_CUDA_VERSION`, else the lowest of `ALLOWED_CUDA_VERSIONS`
    / `EXACT_CUDA_VERSIONS` (13.x → 580, 12.8 → 570, 12.4 → 550, …; one that isn't a version
    is an error, as a malformed `MIN_DRIVER_VERSION` is) — else no driver check. Hetzner CPU VMs skip the GPU checks (decided by
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
    listed OK without are dropped. A corrupt file is one warning and gets replaced. It also
    holds `up --check`'s **bad machines** (`bad_hosts`: machine id → when, pod, failed checks),
    dropped 72 h after their failure or when a complete check on the machine passes, never by
    a listing; `pods test --deep` adds none (its pods are in use, and a participant's own job
    can fail a check), but its passes clear them.
  - `snapshot [--json] [--public] [--out FILE] [--no-probe]` — the fleet in one **read-only**
    picture (one list per provider + the details query, the local proxy file, the health
    cache, and a **reachability probe**: a TCP connect to the SSH endpoint of each billing
    machine on the prefixed `MACHINE_NAME_LIST` — never staff `@` boxes or off-list pods —
    all at once, ≤ 3 s, waiting only for sshd's `SSH-…` greeting — nothing sent, no login, no
    command; **no checks**): `pods list`'s columns plus `SSH` (`ok`/`down`/`-` = not
    probed), `PROXY` (`:9500`, `:9500 stale`, `?` = remote proxy) and `HEALTH` (`fail 2h
    GPU error`). Public `up` needs the probe's answer: a pod listed running whose SSH port
    is silent reads `starting` (the vocabulary stays `up|starting|down`, documented in
    `web/fleet.html`); `--no-probe` skips it (then running + endpoint = `up`). `--json` =
    the internal snapshot (ids, endpoints, costs — never publish it). `--public` = the
    dashboard JSON, an **allowlist by construction**: only *prefixed* `MACHINE_NAME_LIST`
    pods (`@` staff boxes and off-list pods are left out) by short name, with GPU,
    `up|starting|down`, health + age + a reason
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
    After the verdict come `info:` lines with the prepaid balances (and the spend RunPod
    reports that arena doesn't list: volumes, stopped disks, serverless) — informational,
    never part of the verdict or the exit; `--json` carries them as a top-level `balance`.
  - `balance [--json] [--check] [--no-live] [-v]` — money left on each prepaid account, how
    long it lasts and **where the number comes from** (read-only). A balance is **live** —
    **RunPod** GraphQL `myself { clientBalance currentSpendPerHr spendLimit underBalance }`
    (a field RunPod drops costs that field: the query is retried with `clientBalance` alone),
    **Vast** `GET /api/v0/users/current/` → `credit` (that body carries the email and API
    key: only `credit` is read, and it is never quoted), **Shadeform** `GET /account` (undocumented:
    `balance` and `hourly_burn` only — the body's email, phone, tax id and address are dropped at
    parse and never quoted; at $0 Shadeform's behaviour is undocumented: keep Auto Top Up on) —
    when read now or under 5 min ago;
    else **est.**: the last live reading (or `balance set`) minus max(billed, metered)
    spend since — *billed* from RunPod's REST v2 hourly billing history (every resource on
    the account, but it lags: live, a bucket appears about an hour after its usage, filled
    in ~5-min steps — the hour in progress isn't there, nor is an empty one — so for the
    first hour metering alone carries the estimate), *metered* from arena's own burn samples (each step charged at
    its higher end), taken per UTC hour as the larger of the two — shown `~$X est. (live
    n/a 2d, billed)`, for up to 14 days; else `?` with `arena balance set <acct> <USD>`.
    Estimates err low (they warn early, never late) and never rise between readings.
    **Pods nobody priced** (RunPod v1's REST list has no $/h; `arena balance` and teardown
    don't fetch details; after GraphQL nothing does) are counted at the last price seen for
    that pod (remembered in the state from any surface that saw one — an enriched `pods
    list`, the TUI, a v2 listing, the result of any create — `create`, `up` and its
    replacements, `replace`, `migrate copy`, the TUI's add-pod form — only from pods that bill by the
    hour, and kept while the pod keeps being listed, 15 days after that), else at a stand-in
    that errs high: a known GPU at the most it is known to cost (RunPod's recorded catalog,
    the presets, the dearest live catalog/offer price seen in `offers`/`create`/`up`), an
    unknown GPU type at the ceiling `BALANCE_UNPRICED_PER_GPU` (default $8, above the dearest
    recorded catalog price) per GPU — shown `~$X/h` with a note. **Disks**, from the
    listing's sizes: a running RunPod pod's container and volume disk at RunPod's
    $0.10/GB-month on top of its listed $/h (live: a listed $0.13/h 3070 with a 60 GB disk
    drained $0.138/h); a stopped RunPod pod's volume disk alone at $0.20/GB-month (its
    container disk isn't billed — live, a stopped pod with no volume cost $0.00/h); a stopped
    Vast instance's disk at a $0.50/GB-month ceiling (a running one's price includes it) — so
    after a nightly `pods stop --all` a healthy account reads ok. A running pod with no GPU
    information, a stopped pod whose (volume) disk size isn't listed, or a burn not known at
    all make the account `unknown` ("burn unknown: …", never "no burn"). A listing that fails
    (a Vast 429 is retried once first, when it asks for ≤ 2 s) is stood in for by the burn
    observed under 30 min ago, marked stale (`-v` says so) — older, a $0 is unknown and
    anything more a floor. RunPod's own rate, while it is read, replaces the stand-ins; it
    lags a create, stop or terminate by minutes, so "RunPod reports $X/h more than arena
    lists" needs two readings at least 5 min apart.
    **Known undercounts:** a pod created and destroyed between two observations while
    billing is unavailable; and spend arena doesn't list that falls inside billing's lag.
    Vast's negative `balance`, when reported, is shown as "Vast reports $X owed" (never
    subtracted: what it means is for the live console comparison to settle). **Hetzner** is postpaid: €/h, no runway, never a
    warning; volumes aren't counted. Burn = the higher of the provider's own rate and this
    fleet's billing pods there (a provider that failed to list is an unknown burn — never
    $0/h); runway = balance ÷ burn. Levels: **critical** under 24 h, at ≤ $0, or on RunPod's
    `underBalance` (at $0 RunPod *terminates* pods without a network volume); **low** under
    `BALANCE_WARN_HOURS` (default 72, `0` = runway levels off; malformed → 72 and a
    warning here — `config check` fails on it). Lists the fleet (no details) while reading every account (≤ 10 s), records the
    reading in `balance.json` (0600, next to the health cache, per prefix, scoped by a
    32-bit key fingerprint), then runs the **fallback check**: the estimate from a reading
    30 min–~1 day old against the live truth, metered from the listing's burns alone (what
    the fallback will have once RunPod's rate is gone), with this run's billing and with
    metering alone — either reading high is a ⚠ (`fallback check ✓ over 26h, checked 3m ago
    (the estimate read $0.40 low)`), so a broken fallback shows up while GraphQL still works.
    If the state file can't be saved (a probe writes and fsyncs a state-sized file first, so a
    full disk or a spent quota counts), inline surfaces read no live balance at all — they'd
    read it on every look — and say nothing; `arena balance` reads live and says so. `--no-live` shows what
    the fallback says (no live call, nothing written); `-v` adds the burn's parts, spend
    limit, anchors, billing buckets, last failures and the state file. Exit 0; with
    `--check` 2 if any account is critical, else 3 if any is unknown while burning, else 1
    if any is low. A failing source is one note here and a ⚠ row in `config check` —
    nowhere else does it print or change an exit.
  - `balance set <runpod|vast|shadeform> <USD>` — record the balance read off the console when every
    API is dead (a manual anchor; a local write, scoped to the configured key; the next live
    reading replaces it). `create`/`up` print the target account's balance next to the plan
    — the runway after the new pods (`n ×` the dearest option; with no `--gpu` list or
    `--max-price`, the spec's GPU and tier priced live alongside, within the same 5 s — else
    the live price `offers`/placement saw for that GPU and tier under a day ago, `~` with its
    age — else a stand-in), and RunPod's refusals ahead
    of time: under an hour's credit for them, over the spend limit, `underBalance` — never
    blocking.
  - `pods idle [targets] [--hours N] [--json]` — which pods look abandoned: a **read-only
    report that never acts**. One bounded probe per billing pod (60 s; POSIX `sh`, writes
    nothing): established inbound connections from `/proc/net/tcp{,6}` — SSH sessions on
    sshd's port (VS Code Remote-SSH counts; the probe's own, identified by `$SSH_CONNECTION`,
    is left out) and non-loopback ones to any other listening port (Jupyter through the
    provider's proxy) —, GPU util (busiest of 3 `nvidia-smi` samples) + memory, the newest
    file mtime under the home and `BACKUP_REPO_PATH` (`find` ≤ 20 s, skipping `.git`, caches,
    editor servers, package dirs) and PID 1's age, all on the pod's own clock. A table, then
    the **candidates** — idle by every measure for `--hours` (default 6: no connection, GPU
    ≤ 5 %, no file change, up that long), this cohort's machines only (a staff `@` box or
    another prefix is named, never given a command) — with what they cost and the exact
    commands to run, **printed, not run**: `arena pods backup <name>` then `arena pods
    terminate <name>` (the id when a name is shared). Anything unreadable (no reply, a scan
    that timed out, `[N/A]` GPU, our own connection not found, more than 2000 sockets so not
    every connection was read) makes a pod `?`, never a candidate. Not seen: sessions through RunPod's `ssh.runpod.io` proxy (not via sshd).
  - `pods reap run|set|clear|pin|unpin|status|log|hold|release` — the **opt-in auto-reaper**:
    terminate a pod past an end time the operator set (`--expire-in 8h`, `--expire-at …`), or
    idle for a window (`--idle-after 8h`), **after a verified backup and only when every check
    is certain** — anything it can't confirm (an unreachable pod, a probe it can't read, a
    failed listing, an unverified clock, a lock, a pin, `~/.arena/keep` on the pod, anything
    the reap's backup wouldn't copy — any file in a `TEMP_FOLDER_FOR_SWEEPS/`, a link from the
    home to somewhere outside it, data written since the pod was created on `/workspace`,
    another writable mount or the usual places outside the home (`/data /scratch /srv /mnt
    /media /opt /var/tmp /tmp /home/<other>`; the system, installed Python packages, caches and
    empty files — nothing to lose in those: uv's 0-byte `/tmp/uv-setuptools-*.lock` once kept a
    fresh VM from ever reading as idle — passed over) —, a home that's a link to another filesystem, a process list, crontab or
    timer it can't read) means "do nothing and log it". **Since when** is the pod's creation as
    its provider lists it — not the last boot: a Hetzner VM's disk outlives reboots (on RunPod,
    where a restart starts the container afresh, the later of creation and container start). Off by default in layers: a pod needs a policy
    (`reap set <names> …`, or `pods up/create --expire-in/--expire-at/--idle-after`, recorded
    the moment each create returns); `reap run` alone only probes (read-only) and prints the
    plan (`WOULD TERMINATE`); `reap run --apply` acts only with `REAP_ENABLED=1` **in the config
    file** (the environment can't set any `REAP_*` key), no hold, an NTP-synced clock and, with
    no terminal, `--yes` — at most `REAP_MAX_PER_RUN` (default 1) per run and
    `REAP_MAX_PER_DAY` (10) per 24 h. Acting on a pod: the deciding look taken again if over
    60 s old, a durable write-ahead, the git push, the tier roots' markers and free space
    checked again, both rsync tiers under the pod's backup lock (every pod-side step behind
    its identity check) — the big tier as the reap's own copy, which keeps the home's
    dot-directories and all of `~/.cache` but its well-known download caches; the participant's
    notice says exactly what it leaves out: "not kept: only what can be downloaded or installed
    again — Python environments and installed packages (venvs, site-packages, ~/.local/lib, conda
    envs, node_modules), download and build caches (pip, uv, npm, the Hugging Face and torch hubs,
    compiled kernels, Python bytecode, matplotlib's), the VS Code server's binaries and extensions,
    and arena's moved-aside copy of the image's repo — and every .ssh folder; the rest of ~/.cache
    is kept" (`pull::REAP_NOT_KEPT` lists the paths: Hugging Face `datasets/`, W&B and any
    cache nobody listed are kept; conda's `envs/`/`pkgs/` only under a conda root in the home) — a
    dry-run verify (0 files pending), then the verified dirs **frozen**: hard-linked (same
    filesystem, no space) into `<root>/reaped/<name>/<UTC stamp>-<provider>-<id>-attempt/snapshot`
    and `<big root>/reaped/…/big`, counted equal to what was verified, and recorded on the
    attempt's write-ahead — renamed `…-reaped/` only once the terminate succeeded. An attempt that
    never sent its terminate (a re-listing, a pin, `REAP_ENABLED=0`, a crash before it) keeps the
    `-attempt` name; so does its copy when the pod is later found gone (by hand, with its host:
    "gone, not by the reaper (or before its terminate)"). Only a write-ahead marked "terminate
    sent" — saved right before the call — lets a run that finds the pod gone rename the copy it
    recorded (a crash between the terminate and its record); a half-made copy of a run killed
    mid-freeze is never recorded, so never renamed. A rehearsal's is `-rehearsal`; copies frozen
    before these names (no suffix) are "older (state unknown)". `reap status` counts each kind
    under each root's `reaped/` and their size. A later same-named pod's pulls
    never touch that copy, and nothing in arena writes or deletes under `reaped/` (a freeze that
    fails removes the leaves it made itself, and backs off 6 h like a failed backup).
    Then a post-backup quiet check (same id and container; nothing changed since the deciding
    look's scan began; no process started since boot that isn't the container's own) — recorded
    as a look of its own and the next look's baseline, so the backup's own CPU and traffic never
    read as use afterwards (a rehearsal or a stopped attempt leaves the streak alone), while what
    it sees counts like any look's —, a
    fresh re-listing (same id/name, RUNNING, lock reported off, no `-new`/`-old` twin, no
    second pod of that name — and every provider listed: one that failed could hide a twin),
    a state re-read (no hold, no new pin, still due) and
    `REAP_ENABLED` re-read from the config file — then one terminate, routed by provider name.
    **CPU** counts as use above 0.25 cores over the probe's own 10 s, or 0.10 cores since the
    previous look — averaged over at least 3 minutes, so a look taken soon after another (a manual
    `reap run` right after the cron's) never reads the reaper's own ~5 s of CPU as use, while a
    job's 20 s of a full core between two such looks still does.
    **Processes**: a process counts as use — a dormant `nohup python train.py`, a kernel, an
    editor, `mosh-server`, a `screen` — unless it's one of the image's own services (a command
    line seen in the first two minutes of the FIRST boot the reaper looked at, kept per pod: a
    participant's `@reboot` job after a later reboot counts), a plain interactive shell, tmux's
    server (its panes are judged: a non-shell foreground counts), sshd's or cron's own, or the
    login's `systemd --user`. So does any open terminal (a process on a `/dev/pts` outside the
    probe's own session, a tmux pane's own shell aside: an ssh login, RunPod's web or proxy
    shell, a VS Code or Jupyter terminal) and scheduled work no process shows yet (a crontab
    line, an `at` job, a systemd timer someone added). To stop a run
    mid-way: `arena pods reap hold` (or `REAP_ENABLED=0` in the file) — checked again right
    before the terminate. `pods replace`/`migrate copy`/`cutover` refuse a pod being reaped —
    and, where the reaper is configured (`REAP_ENABLED=1` in the file or its cron line), when
    they can't read its state (root's state dir from an operator's shell), naming the path. Expiry never takes a pod in use or unreachable: it's
    deferred (`OVERDUE (in use)` / `(unreachable)` in `pods list`'s ENDS column) and notified at
    most every 6 h (`REAP_NOTIFY_CMD`, one line on stdin). A terminate error other than
    "locked" sets an automatic hold. Needs an explicit absolute `ARENA_STATE_DIR` **in the
    config file** (state: `reap.json`, log: `reap-log.jsonl`) — a shell exporting a different
    one is refused (it would put a `hold` where the cron never looks); every state change
    prints the file it wrote — RunPod on REST v2, the default (pods on `RUNPOD_API=v1` and Vast
    are never reaped; since v2 became the default — 70bce78, 2026-10-19 — a config with no
    `RUNPOD_API` line, which ran v1, has its RunPod pods reapable without anyone choosing v2, so
    `reap status` and every plan say `RunPod: REST v2 (…) — RunPod pods with a policy can be
    reaped: terminated, container disk and all, after their backup` whenever one has a policy,
    and `config check`'s RUNPOD_API row says it too; nothing acts by itself: `--apply` still needs
    `REAP_ENABLED=1` in the file), absolute `LOCAL_BACKUP_DIR`, a `.arena-backup-tier` marker on each backup root
    (`snapshot` / `big`; the big tier's root is `LOCAL_BACKUP_BIG_DIR`, default
    `<LOCAL_BACKUP_DIR>/big`), `ARENA_START_DATE` and no `BACKUP_REMOTE_PATH`; `config check`
    lists what's missing once the reaper is on or its cron line is installed (off: one `·
    reaper off (opt-in)` row, a ✗ only for a `REAP_*` value set but invalid). Every `pods reap
    … --json` prints exactly one JSON document on stdout — `{"error": …}` on a failure,
    `{"skipped": …}` when another run holds the lock — and everything else on stderr. Rollout: `cron install --reap observe` first (logs `would_reap`; the line runs at :11/:27/:41/:57 — minutes no other arena line uses — its output in `~/.local/state/arena-cron/reap.log` unless the line logs elsewhere already), then
    `--reap apply` (refused unless REAP_ENABLED=1). `reap set` also writes `~/.arena/NOTICE` and
    an interactive-only login banner on the pod (also written after `up`'s setup for a pod
    created with a policy, after `pods setup`, and after a restart's re-provision — the 1-hour
    `wall` before a reap reaches nobody in a RunPod container, which has no utmp); `pods
    rename` keeps the policy's name; a `replace`/`migrate` doesn't carry it (said).
    **When the reaper never acts** (all fail safe: the pod stays, `reap run` says why, an
    expired one is `OVERDUE` and notified every 6 h): a live Jupyter kernel (its loopback
    connections are sockets in use), any process started after the container's first two
    minutes that isn't one of its own services restarted (a VS Code server left running, a
    `screen`, `mosh-server`, a forgotten `python`), an open terminal, a crontab line or timer,
    data on `/workspace` (where setup puts the repo when the pod has a volume) or another
    writable mount, any file in a `TEMP_FOLDER_FOR_SWEEPS/`, data written outside the home since
    the pod was created (or a link out of the home), a pod whose provider gives no creation
    time, a MIG GPU slice (`[N/A]` utilisation reads as unknown), RunPod
    on `RUNPOD_API=v1` and Vast (no lock state — `up --expire-*` refuses them,
    ENDS shows `not reapable (…)`), a pod renamed outside arena or sharing its name, a home
    that's a link to another filesystem, `~/.arena/keep`. A tmux session whose panes run only
    a shell doesn't count (its utmp entries aren't logins); one running anything else does.
    A `reap.json` that can't be read stops every run (`reap hold`/`release` still work — the
    hold has its own `hold.json`); the error gives the recovery: move it aside
    (`mv reap.json reap.json.bad-<UTC time>`) and the reaper starts with no policies.
  - `pods pull [targets] [--label <label>]` — the **file** backup (complementing the git
    `backup`): rsyncs each pod's home into `<dir>/<label>/<pod>/`, reporting per pod what was
    sent and what the snapshot holds (`12 files, 3.4M sent of 6754, 1.8G` — sizes parsed in
    every format rsync prints). No targets = this cohort's pods, as for `backup` (a staff box
    only when named). Targets are positional like every command's (the old `-t`
    still works, and so does `pods pull w1d3` — read as the label, with a note — when `w1d3`
    names no pod). **Keeps `.git`, complete** (`--no-git` to skip): the size cap
    (`--max-size`) applies to the files, then a second pass copies every kept `.git` uncapped
    — a repo's big pack used to be dropped, leaving refs to missing objects — and the empty
    dirs inside a `.git` (`refs/heads/` once refs are packed) are kept, so each snapshot (and
    `big/`) is a repo git reads. Excludes other dotfile dirs + `site-packages`. **Hard-linked
    snapshots**: each dated snapshot is still a complete tree (restore/pull layout unchanged),
    but files unchanged since this pod's previous snapshot, or identical to the week's **fleet
    baseline** (`pods baseline`), are hard links (`rsync --link-dest`, absolute paths, same
    filesystem — all under `<dir>`), not copies: a day's snapshot costs only what changed, a
    new or replaced pod's first one only what isn't the image's. A linked file is one inode in
    every snapshot sharing it, so a pull into a label that already has files (the */15 cron
    pulls into the day's label all day) never changes an existing file's mode or owner in
    place (`--no-perms --no-owner --no-group`): a `chmod` on the pod used to rewrite the mode in
    every earlier snapshot (and, through the baseline, other pods'); now a mode-only change
    made after the day's first pull shows from the next label on, and earlier snapshots stay
    exactly as they were (a changed file is always a new file). The run ends with each
    snapshot's real cost (`1.8G in 6754 files, 12.0M of it new on disk`); `du -sh <dir>/*` in
    one invocation counts a hard-linked file once. Knobs come from flags
    else config: `LOCAL_BACKUP_DIR` (`--dir`), `BACKUP_MAX_SIZE` (`--max-size`),
    `BACKUP_REMOTE_PATH` (`--remote-path`, default `~/`). Label defaults to the `wNdM`
    iteration (`big`/`baseline`/`reaped` are refused). Confirms first; `--dry-run` prints the
    exact rsync commands, shell-quoted. **Per-pod backup lock** (in an existing absolute
    `LOCAL_BACKUP_DIR`: `.arena-locks/<name>.lock`, the folder 0700 — a missing backup dir is
    never created for it, and the run goes as before without locks): each pod's lock is taken
    once per name (two listed pods sharing a name share it) and released the moment that pod's
    own rsyncs end, so one slow pod never holds the others; `pods backup` holds it for that
    pod's git push, then for its rsyncs. A pod whose lock another backup (the cron, the
    reaper, the TUI's backup) holds for 120 s is skipped, named, and fails the run. **Bounded**: rsync
    `--timeout=300` (gives up after 5 min without any I/O) and a wall-clock budget per pod,
    `BACKUP_TIMEOUT_SECS` (default 7200 = 2 h, max a day; settable from the environment) —
    a pod that runs out is stopped (SIGTERM, so rsync takes its ssh and receiver down too;
    SIGKILL 2 s later) before the failure is reported — so nothing is left running even when
    it was the last job and arena exits right after — and named in the failure (`✗
    arena9-bloom [big]: timed out after 7200s`); the other pods finish, and the next run
    continues incrementally. **A directory where the pod now has a symlink** (or a file) —
    rsync's `could not make way for new symlink: ARENA_3.0`, exit 23, every run (live: 9 pods'
    `[big]`) — is moved aside into a holding directory beside it, its name kept:
    `.arena-olddir-<YYYYMMDD UTC>/ARENA_3.0` (`-2`, `-3`… if that's taken), at any depth and in
    any tier; nothing is deleted. Keeping the name keeps every name-based filter matching inside
    it (the archive's secret excludes: `hf/token`, `.git/config`, …), and the archive never takes
    a moved-aside directory at all. Only rsync's exit 23 is acted on; a pod can print the line
    itself (its ssh session's output reaches rsync's stderr), but that only ever moves one of its
    own backed-up directories aside, never deletes. Then that rsync runs once
    more within the same budget, and the pod's line says what moved. The path comes from rsync's
    stderr, so only a plain path inside the destination is acted on: never `..` or an absolute
    path, never through a symlink, and only a real directory is moved. (`--force` would have
    deleted the backed-up directory.) The reaper's own copy does the same. A repo that setup
    **linked onto the `/workspace` volume** is still backed up as its tree (rsync `-a`
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
  - **`pods replace` / `pods migrate`** — what the copy onto `<name>-new` does (live campaign
    1, findings #5–#7, #13, #16, #17, #19, #20, #26–#30):
    - It's a **replica**: a file deleted on the original is deleted on `-new` too, but the
      transfer destroys nothing on `-new` — whatever it overwrites or deletes there is
      **moved** into `~/arena-sync-kept/<UTC stamp>/` (a *visible* folder: `pods pull` backs it
      up and a later replace/migrate of that pod carries it; a mirror never deletes it; the
      repo's files land beside the repo's real directory first, on its volume, and are then
      brought into that folder under `ARENA_materials/`). Before each sync `-new` lists every
      file that **changed on it** since its last sync (`~/.arena-last-sync`) by **ctime** —
      created, edited, extracted, copied or renamed there; a user tool can't set a ctime, while
      `tar`/`unzip`/`cp -p`/`mv`/`wget` all leave old *mtimes*. After a **verified** sync it
      drops only the moved files on neither side of that line (an earlier sync's copies the
      original has since deleted or changed); everything else is **kept and listed** — the
      count and size are of the listed work; files from inside a `.git` (its index, rewritten
      whenever git runs there) are kept too but counted apart (`plus N of git's own file(s)`). A sync
      that didn't complete drops nothing. The original is only ever read; backups never mirror.
    - **A mirror only from an unchanged original**: onto a `-new` that was synced before, the
      original must still be what that sync copied — its container not created since (a
      reset: RunPod's own, a stop, a `pods start`) and its record of that sync
      (`~/.arena-sync/sent-to`, a dot-dir that's never backed up or restored, so it goes with
      the home on a reset) still there. Otherwise `migrate copy` / the cutover's final sync is
      **refused before anything moves** (mirroring a reset pod's bare image would delete the
      participant's files from `-new`), pointing at `cutover --no-final-sync`.
    - **Every step on a pod runs on that pod**: on RunPod each one — the source's prepare and
      the direct copy run on it; `-new`'s prepare, the transfer's remote rsync
      (`--rsync-path`) and the tidy-up — first checks `RUNPOD_POD_ID` in the same SSH session
      and does nothing on any other pod (a reassigned endpoint). A stopped `-new` is refused
      like a stopped original; so is a stopped original by `replace` / `migrate copy` *while
      planning*, before anything is created and billed. When the original's repo is missing
      or a dangling link, the via-local copy leaves `-new`'s repo alone, as the direct one does.
    - It carries the home incl. `.git` (every one: a nested clone's too, its stashes and
      branches; and each exactly as on the original — empty dirs included, so a repo whose
      refs are all packed stays a repo, and `-new`'s own git state the original lacks is moved
      aside like any file), `~/.config`, `~/.jupyter`, `~/.ipython`, `~/.local`
      (bin, share) — and says what it skips (caches and other dot-dirs — `.cache/` and
      `__pycache__/` inside `~/.config` too —, venvs, site-packages and `~/.local/lib`, HF
      caches, uv's Pythons, the `claude`/`codex` setup installs in `~/.local/bin` (never
      deleted on `-new` either) and Claude Code's state under `~/.local`, shell rc files +
      history, zsh's completion cache (`.zcompdump*`, never deleted on `-new` either),
      `.claude*`, `.ssh`, `~/.name`). The plans no longer promise a "copied-size sanity" check: what's verified is
      the delivery marker read back from the right pod, then an SSH + GPU health check.
    - The **direct** pod-to-pod copy is skipped when both pods share a public IP (a pod can't
      reach its neighbour through it) and gives up connecting after 15 s; when it fails the
      fallback names its real reason. The **via-local** fallback first sizes the staging (an
      rsync dry run) and refuses when the local disk would keep less than a tenth of itself
      (≥ 5 GB) free; the stage (`$TMPDIR/arena-replace-<id>`, mode 0700) is **removed** once
      the copy landed, and kept — with its path, size and the `rm -rf` — only when it failed
      (a re-run continues from it); `migrate finish` / a finished `replace` remove a leftover.
    - The source's **GPU type** comes from its own listing when its spec doesn't say (RunPod
      v1), before config's `GPU_TYPE` (the plan says where it came from). `--gpu`/`--cloud`
      take **lists** and go through the same placement as `up` (`--max-price`, `--order`,
      `--retry-mins`/`--retry-secs`): one create at a time, the first option with capacity —
      with `up`'s rules (the config's CUDA floor, v1's creatable-GPU check, an option the API
      rejects skipped). A new pod that stays up behind RunPod's SSH proxy only (no public IP)
      is given up on after `up`'s 5-minute grace, named as such and left in place, rather
      than after the whole 15-minute wait; so is a pod `start`/`restart`/`reimage` brings back
      that way.
    - After the swap the promoted pod gets `~/.name` in setup's format and its **per-host API
      keys under the canonical name** (setup ran while it was `-new`, which no key row
      matches); the parked one gets `<name>-old` in its `~/.name`; the `MACHINE_NAME` env note
      is printed. A new host with a **maintenance window** is warned about before the swap,
      and `replace` then keeps `-old`.
    - `migrate cutover --no-final-sync` cuts over to the copy already on `-new` when the
      original is stopped/unreachable (a stopped source is refused by the final sync *saying
      so*, with that flag); the plan says when `-new` was last synced. `migrate revert` says
      that the new pod's work since the cutover stays on `-new` (back it up with `pods pull
      <name>-new`); a revert never writes `~/.name` through a stopped pod's stale endpoint.
      `migrate finish` keeps a **stopped** `-old` — nothing was synced from it since `-new`'s
      last sync, and its volume may hold what changed since — saying so (volume, last sync)
      unless `--discard-stopped`. `migrate copy` of a **locked** original is allowed (it only
      reads it) with a note; cutover / replace / finish refuse a locked pod with the unlock
      command — nothing here ever unlocks.
  - `pods baseline <pod> [--week N] [--dir] [--replace] [--dry-run]` — capture the week's
    **fleet baseline** into `<dir>/baseline/w<N>/fleet/`: the image's own files, which every
    pod's snapshots hard-link to (see `pods pull`). Run it once a week (or after an image
    change) on a pod `up` just made — its container must say when it was created and be under
    a day old, else it's refused (so is a stopped pod: its listed endpoint is stale). It rsyncs the pod's home with the snapshot tier's filters
    (linked to last week's baseline), then **keeps only the files older than the container**:
    a hard link is made on equal size + mtime + permissions, contents are never compared, so a
    file written at setup or later (`~/.name`, the API keys in `.bashrc`, a script's output on
    several pods in one second) could otherwise stand in for another pod's different file.
    Nor any file **copied onto the pod since** with an older mtime kept — a `pods replace`/
    `migrate` copy of another pod's home (`rsync -a`), a `pods restore`, an archive setup
    unpacked: the pod lists them first (`find` for a ctime after the container's creation with
    an mtime before it — an image file's ctime is its layer's unpacking), and they're left out
    too, so even a replaced pod gives a clean baseline; a listing that fails or is cut short
    refuses the capture. A repo on the `/workspace` volume is left out (a volume can hold work
    older than the container). Captured into a partial dir and moved into place only when complete; an
    existing baseline needs `--replace`, which moves it aside (`fleet.replaced-<time>/`) —
    nothing is deleted. Chosen over an automatic "first snapshot of the week": a pod in use
    can hold work with pre-container mtimes, so which pod is pristine is the operator's call.
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
    by default (a snapshot pulled before `.git` got its own uncapped pass can lack git packs
    over the size cap — refs to missing objects break the repo; `--with-git` to push it, or
    `--from big`). After a restore that pushed the repo's
    `.git`, the repo is checked (`git fsck --connectivity-only`, 120s) and a broken one fails
    the command, saying where the replaced files are. Refuses before touching the pod when
    the pod is stopped (its listed endpoint is stale — `pods start` it first; the dry run too),
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
  - `api get <runpod|runpod-v1|runpod-v2|vast|hetzner|shadeform> <path> [--raw]` — **read-only**
    passthrough for what no command covers (`pods/<id>`, `catalog/datacenters`,
    `network-volumes`, `billing/pods`; Vast `users/current/`; Hetzner `locations`; Shadeform
    `instances/types`, its key in `X-API-KEY`; `sshkeys` there refuses `--raw`): one GET
    (there is no other verb) with the configured key, printed as pretty JSON. The path is
    relative to the provider's API base — absolute URLs, `//host`, `..` and `#` are refused
    before the key is read, redirects aren't followed — so the key only goes to the provider.
    A leading copy of the base's own path is dropped (`/v2/pods` on v2 is `pods`, not
    `…/v2/v2/pods`); another version (`v1/…` against v2) is refused, naming `runpod-v1`.
    Secrets are redacted (env values, key/token/password fields, `rpa_`/`sk-`/`hf_` tokens,
    SSH key material) unless `--raw`; the API key itself is never printed, `--raw` included
    (scrubbed from the body as received, before an error message clips it).
  - `gpus` — list the GPU types for `--gpu`: RunPod's **full live catalog** (via
    `GET /v2/catalog/gpus` for both tiers, the default — STOCK shown as
    `community/secure`, counted over hosts at or above the CUDA floor, and the best signal the
    answer carries (a data center's level, a CUDA version tagged available) — no GraphQL; via
    GraphQL on `RUNPOD_API=v1`) when on RunPod with a key, else the local presets. Shows VRAM, **live**
    community/secure $/hr (a tier that doesn't offer the card shows `-`: v1's GraphQL quotes
    phantom prices for those, dropped by its `secureCloud`/`communityCloud` flags) and
    RunPod's stock hint (`~` marks a preset estimate). On
    v1, if RunPod rejects the priced query (HTTP 200 with errors, or a non-auth 4xx/5xx), the plain catalog
    query is tried before falling back to presets. `--json` emits rows
    `{id, display_name, memory_gb, community_price, secure_price, stock_status, creatable,
    source, price_source}` (+ `secure_stock_status` on v2; incl. catalog entries v1's create
    API rejects, `creatable: false`; on v2 every row is creatable).
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
    `origin` is pointed at the cohort repo (where backups push); the public course repo
    (`ARENA_UPSTREAM_REPO`, default `ARENA-education/ARENA_materials`, `none` to skip) is added
    as the checkout's `upstream` remote over https when it has none — so a participant can
    `git pull upstream main` the course's own fixes. Setup never fetches it, and an `upstream`
    already there (a participant's own) is left alone; `checkout.defaultRemote=origin` makes a
    plain `git checkout <branch>` both repos have take origin's. arena's own fetches name origin
    (init-branches, set-branch), so an unreachable upstream never stops them.
    The repo update fetches **only the default branch, without tags** (a bare `git fetch`
    would pull every participant's autocommit branch); a tracked non-default branch pulls
    just its own upstream; the default branch is fast-forwarded only when the checkout has
    no tracked change and no commit of its own — otherwise it's left as it is with a warning
    (`--force` checks out the default branch and resets it). Pods run in parallel and **every step has a time budget** —
    copies 60s, the image config step 300s, the hetzner bare-VM script 1800s; `--timeout
    <secs>` (or config `SETUP_TIMEOUT_SECS`, 1..86400; `up`/`replace`/`migrate copy` use the
    config value and reject a bad one — and `up` missing `ARENA_REPO_*` unless `--no-setup`,
    or with `--check` a bad `MIN_DRIVER_VERSION` — *before* creating anything) overrides the
    main-step budget. A wedged pod prints `✗ <name> (timed out at <step> after Ns)` and the
    others finish normally; a timed-out `ssh`/`scp` is stopped (SIGTERM, so scp also stops
    its ssh transport; SIGKILL 2s later). Connection refusals right after create are still
    retried for ~150s (sshd booting); an auth failure (`Permission denied (publickey)`)
    fails at once.
    **Hetzner (bare VM):** the embedded `hetzner_setup.sh` builds the ARENA env with uv on
    **Python 3.13** (the repo's `requires-python` lower bound when newer; `ARENA_PYTHON`
    overrides) with CPU substitutions by pattern, not by exact upstream line (`jax[cuda…]` →
    `jax[cpu]`, bitsandbytes and `nvidia-*cuNN` wheels dropped, CUDA indexes and `+cuNNN`
    pins dropped, `uv pip install --torch-backend cpu`). It **fails loudly**: any failed
    package/venv/install step, or a venv whose python can't `import torch, transformer_lens`
    afterwards, exits non-zero with the reason as its last line (`FAILED: …`) — a VM is never
    reported set up without a working env. Re-runs skip the install only when that import
    gate already passes. Its installers' scratch (uv's above all) goes to a temp dir of its own,
    removed when it ends, failure or not: uv's 0-byte `uv-setuptools-*.lock` files, left in
    `/tmp`, kept a fresh VM from ever reading as idle to the auto-reaper.
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
    window (detail pane: window + note) — else **L** for a locked pod (detail pane: `status:
    … · locked`; restart/stop/terminate on it are refused with the unlock hint, and the
    dashboard never unlocks), **HEALTH** = the last `pods test --deep` verdict
    + age (`pass 12m`, `fail 2h`; detail pane: worst issue + the failing check's text),
    **PROXY** = the stable port, green live / yellow stale (`:9501 stale` when wide), `-`
    no forward, `?` remote proxy. The **summary bar** is `pods list`'s footer (`$` and
    Hetzner `€` kept apart, unpriced pods counted) plus per day, **led** by a yellow
    `⚠ vast failed to list — its pods (and their cost) are missing` when a provider didn't
    answer (first, so a narrow terminal can't cut it off), and ending with the account
    prepaid balances (`arena balance`'s views, observed in the details round — at most one
    live read per account per 5 min — and recorded in the CLI's state file; a low account
    moves to the front in yellow, a critical one in red, with its amount: `‼ runpod $1.20
    ≈1.9d`; an unfunded opt-in account with nothing billing there stays at the end,
    `shadeform $0.00 unfunded`). API calls per refresh are
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
    terminate/restart/stop on a marked set of more than 5 pods — or of every listed pod — ask
    for `ALL`, like the whole fleet (smaller sets: the pod count). A marked-set confirm
    names the pods once (the first 10, then `… and N more`), with the token prompt always on
    screen. `Esc` on the list clears the marks (it never quits).
  - Every pod SSH call (metrics, test/run/backup/set-branch/setup, deep check) goes
    through `Remote` with a budget (90 s / 30 min / 5 min / 2 min / setup's per-step /
    150 s), so a wedged pod ends an action with `timed out after …`; only `c` (your
    interactive shell) is a plain `ssh`.
  - **Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU
    breakdown, full branch/origin/health, util/temp **sparklines**). `Ctrl-C`/`q` quit;
    `Esc` only steps back (detail → list; on the list it clears the marks).
  - **Act** on the selected pod with `a` (restart / stop / terminate / backup / setup /
    test / run / set-branch), on the **whole fleet** with `A` (backup / setup /
    test / run / set-branch; restart, stop and terminate only on a **marked** set — a stop
    there skips, and names, pods not running and pods whose provider has no stop
    (Shadeform), and refuses a set of only those, as `pods stop` does), or **add pods** with
    `n` (it lists the fleet in the background first). `run` and `set-branch` pop a
    text-input modal to type the command / branch; `run` shows each pod's whole output (its
    last 40 lines, stderr too when it fails); `test` is read-only. Every mutation
    goes through a confirmation modal — the *only* place the TUI mutates anything.
    Lifecycle actions (restart/stop/terminate) require **typing the pod's exact name**;
    restart is destructive like terminate (red, and its modal says it wipes the container
    disk on RunPod/Vast; Hetzner: "disk kept");
    whole-fleet mutations require typing **ALL** (a marked set: its count, or `ALL` as
    above); every confirm says what will happen in plain words (`alpha: commit + push the
    current branch`), never the generated `ssh …` line — `v` shows that for
    backup/setup/test — and backup/setup/test take a single `y`. The dashboard's reads stay
    reads.
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
  nothing mutates non-interactively by accident — and exit with status **2** (a declined
  prompt at a terminal is `aborted.`, status 0). (`cron install`/`remove` confirm too, and
  bake `--yes` into the lines they install.)
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

# interactive dashboard (or `arena tui`, which inherits --provider/--config); arena-tui itself
# takes no arguments (any is refused) — its config is ARENA_CONFIG, default the prod path
ARENA_CONFIG=./config.env cargo run -p arena-tui

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
| `setup`, `copy-keys` | positional | every pod | parallel |
| `backup`, `init-branches` | positional | this cohort's pods (`all`/`--all` too; a staff box only when named) | parallel |
| `cp` | `-t <targets>` (file/dest are positional) | every pod | parallel |
| `pull` | positional (the label is `--label`; the old `-t`, and a lone `wNdM` that names no pod, still work) | this cohort's pods, as `backup` | parallel |
| `set-branch <branch>` | positional after the branch | **refused** (targets or `--all` = this cohort's pods) | parallel |
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
  `setup`/`copy-keys` as described above, the repo-location question before a volume pod's
  restart/stop/pull 20s. rsync transfers, which drive their own ssh outside this seam, are
  bounded the same way: `--timeout=300` (I/O silence) everywhere, `BACKUP_TIMEOUT_SECS` per
  pod (default 2 h) for `pull` and `backup`'s file step, `restore` 2h (`--timeout`), 2 h per
  leg for the replace/migrate via-local copy (pull, then push — a leg's rsyncs, the home and
  a linked repo, share it).
- `copy-keys` warns by name about any reachable pod that matched no per-host key.

### Overriding config without editing it

Any key in `config.env` can be overridden by an **environment variable of the same
name** (env wins; it can't introduce brand-new keys — except a short allowlist such as
`ARENA_START_DATE`, `SSH_PROXY_RELOAD_CMD` and `RUNPOD_API`, e.g. `RUNPOD_API=v1 arena pods
list` to use the retiring RunPod v1 backend for one command, until 2026-11-15; v2 is the default). This is how you point at an SSH
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
`up` means the machine's SSH port answered when the snapshot was taken (every 2 min), so a
machine that's listed running but not accepting logins shows `starting`, not `up`.

```bash
mkdir -p /srv/arena-fleet && cp web/fleet.html /srv/arena-fleet/index.html
arena --config /path/to/config.env snapshot --public --out /srv/arena-fleet/fleet.json  # try it once
arena --config /path/to/config.env cron install --snapshot /srv/arena-fleet  # then every 2 min (other arena lines kept)
```

Then serve `/srv/arena-fleet` with any static host (an existing web server's directory, a
`python3 -m http.server`, object storage synced from it …). `fleet.json` is public-safe by
construction, so no auth is needed; health shows `unknown` until a `pods test --deep` or
`up --check` has run under the same user (the cache lives in that user's state dir — set
`ARENA_STATE_DIR` for both if the cron runs as someone else).

## Backup archive (`arena archive`)

Copy #2 of the cohort's backups, on one small always-on Hetzner VM per cohort, so they outlive
the pods (a wipe, a replace, the end of the program) and participants can get their own files
back without the operator. Two volumes, because they fail differently: `snap` holds the dated
small-file snapshots (`snap/<wNdM>/<pod>/`, hard-linked across days exactly like the control
box's `LOCAL_BACKUP_DIR`), `big` the all-files mirror (`big/<pod>/`). A checkpoint dump that
fills `big` can never stop the history being written; each is sized, resized, watched and
deleted on its own.

- **The control box pushes** (`archive sync`, its own cron lines) with a key the archive forces
  into `rrsync -munge -wo -no-del -no-lock`: it can write, never read, delete, get a shell or
  forward, and every symlink a participant planted is stored defanged, never followed (one whose
  target is too long to store that way — over 4080 bytes — is left out by an exact rule in a
  private `--exclude-from` file, never argv, with a count in the run's output; a pod with more than
  1000 of them is left out of the push, named). The archive holds no outbound credential and can never reach a pod, a provider or the
  control box. Every push is additive and never touches a file already there (`--delete`,
  `--inplace`, `--append`, `--partial-dir`, `-a`, `-p`, `-L`… are never sent — test-enforced; a
  push is `-rlt -J --chmod=u=rwX,go=rX`): a changed file is written anew, so a past day's
  hard-linked file — or another pod's copy of a shared one — never changes, not even its mode, and
  no per-directory holding area exists for a planted link to redirect. **A file is linked only
  where it was compared by content**: rsync tries a `--link-dest` *before* the file already there,
  by size and mtime only — a same-size, same-mtime file of another day once replaced a label's own
  correct copy (live W7). So a pod's first push of a label adds only what is missing, checksummed
  against its earlier labels and the baseline (`--checksum --ignore-existing`); later pushes while
  the day runs compare with the label's own copy (no link source); and a **check** — `--checksum`
  with the link sources but without `-t` (so no mtime is ever set on a shared file), then a quick
  pass for the mtimes — re-links a label pushed before its predecessor and, once its day is over,
  compares every pod's copy with the control box's before the label seals; `sync --label` is
  always a check. A pod's copy counts as final only when every file of it was compared in a pass
  that completed: a check — or a first push into a tree known to be absent (the snap volume was
  created empty by `archive up`, and every push's start is written and fsynced before rsync runs,
  so a push killed half-way is followed by a check, never by another first push). A sync state of
  unknown origin (a lost `sync-snap.json`) proves no absence: its first pushes wait for a check.
  A pod's first push of today's label waits while its pull runs (its backup lock held) rather than
  seeding a half-written tree; a past label's tree is final and never waits, nor do later pushes.
  A backup run holds each pod's lock from its start until that pod's rsync ends, so a pod can
  wait run after run: after 4 runs (an hour at the cron's pace) `sync` and `status` ("waiting for
  a pull") name it. The content read costs one pass of each pod's label at its first push and one
  at its check.
- **Only this cohort's pods leave the control box**: the `MACHINE_NAME_LIST` names (and their
  `-old`/`-new` twins), never `@` staff boxes or anything else an unscoped pull put there
  (`sync`/`status` list those as "kept on the control box only", apart from pods named like the
  cohort's but missing from the list). A pod that leaves the list keeps its history archived
  (`sync`/`status` name it: "off the list, still archived"); one re-listed as an `@` staff box,
  or **forgotten** — `arena archive forget <pod>` (consent withdrawn; also its `-old`/`-new`
  twins unless those are list entries of their own; refused unless run against the state dir
  that holds `archive.json`, the one the cron reads; it shows what it would record and asks —
  `-y` or `--confirm <prefix>-<pod>` without a terminal) — is never pushed again, until
  `arena archive unforget <pod>` lifts it. Its archived copies
  stay — readable by every participant while access is on, as `forget` and `status` say — until
  `--delete-archived`, which lists every directory with its size, needs the name typed
  (`--confirm <prefix>-<pod>`) and only then takes the sync locks for the delete itself — and
  takes the pod out of the sync state, so none of its labels counts as pushed or final. After an
  `unforget` every dated label the control box still holds of it is pushed and checked again, and
  its big tree copied again; a custom label only by `archive sync --label L` (it says so); what the
  control box no longer holds — labels, or the big tree (`big/`) — is lost for good, and `unforget`
  and `status` ("lost") say so.
  **Secret files never do**: shell rc files (the API keys `copy-keys` writes), histories,
  `.env*`, `.netrc`, `.gitconfig`, `.git-credentials`, `.pgpass`, `.s3cfg`, `.ssh/`, `.config/`,
  `.cache/`, `.claude*`, cloud and Kaggle credentials, Hugging Face token files, `id_*`/`*.pem`/
  `*.key` keys and every repo's git configs (`.git/config`, `config.worktree`, submodules', a
  bare or mirror clone's `<name>.git/config`; git works without them). Nor does a directory a
  pull moved aside (`.arena-olddir-<day>/`) or the operator did (`<name>.olddir-<day>/`): the
  archive already holds its contents under the old name — `ARCHIVE_EXTRA_EXCLUDES` only adds to the list.
- **Participants read** over sftp as one chrooted, read-only user (`backups`): read-only three
  times over (`internal-sftp -R`, read-only bind mounts, ownership). Laptops use the cohort key,
  pods a per-cohort read key `pods copy-keys` installs. **Access is OFF after `archive up`** until
  you run `archive access on`, which says plainly that everyone holding the cohort key can then
  read every pod's backups (they are root on every pod already); `access off` is instant.
- **The fleet never sees it**: every resource carries the label `arena-archive=<prefix>`, the
  Hetzner fleet listing drops labelled servers (so no `pods`/`proxy`/`snapshot`/TUI command can
  touch it), and delete protection backs that up. Use its own Hetzner project
  (`ARCHIVE_HETZNER_API_KEY`); without one it falls back to `HETZNER_API_KEY` and says so.
- **Nothing is ever deleted automatically.** `ARCHIVE_KEEP_UNTIL` only drives messages, the
  README on the archive and `teardown --check` (`◷ kept` until then). Deleting data is
  `archive down --delete-volumes --confirm <prefix>-archive`, typed — `-y` is never enough.

```bash
arena archive up --dry-run          # the plan, live prices, the local sizes it will hold
arena archive up                    # keys (state dir), SSH-key object, server, volumes, pin, setup
arena cron install --archive        # snap at :07/:22/:37/:52, big at :40 (absolute LOCAL_BACKUP_DIR, a start date)
arena pods copy-keys --all          # the pods' `archive-<prefix>` block, pinned key and read key (no key files needed)
arena archive access on             # when you're ready (prints the privacy statement)
arena ssh-config                    # participants' config now ends with the archive block
arena archive status                # API + probe + pushes + retention + cost; non-zero on FAIL/UNKNOWN
```

`archive sync [--tier snap|big|both] [--label L]… [--dir D] [--dry-run]` pushes the snapshot
labels that aren't final yet — the newest first, then up to three older ones per run, those
tried least recently (round-robin: a label that fails every run never takes every slot) — pod by pod, as the pulls do: each pod's tree goes to
`snap/<wNdM>/<pod>/` hard-linked to that pod's earlier labels on the archive (complete or cut
short — only a file identical in content and mtime is linked) and to the week's fleet baseline, pushed once to
`snap/baseline/` (so the image's files are stored once for every pod, as on the control box), in
two passes like the pull's (the files under `BACKUP_MAX_SIZE`, then every `.git` uncapped — a
pack over the cap must not leave a broken repo); then `big/`, pod by pod (least recently pushed
first), so one pod's failure is that pod's alone. A label is final once a push that
*started* 2 h after its UTC day ended went through for every pod, linked to everything it should
have been, and compared by content with the control box's once its day was over — so a failed or
out-of-order push never leaves a full, un-linked copy for good: the next push checks and re-links it. One stuck pod never holds the rest up: a pod whose push of a past label
fails of its own kind three runs in a row is **sealed partial** for that label — kept as it is on
the archive, later labels link to it and seal — and `status` names each one (`archive sync --label
L` retries it); a fleet baseline that fails three runs in a row likewise stops holding labels
open. A failure only counts against a pod when another pod's push got through in the same run:
the archive's own failures — full, read-only or failing volume (EROFS/EIO), unmounted,
unreachable, keys refused, a stalled connection — stop the run, charge nobody and make `status`
FAIL, and so does a run in which every push failed. A pod cut off by the label's budget before it
had its fair share (budget ÷ pods), or still sending files when cut off, is never charged: it goes
first next run. It needs no Hetzner token (it reads `archive.json`), takes a lock per
tier (a running tier is skipped, exit 0; a dry run takes none), keeps per-tier state tied to the
volume it describes (a new volume gets everything again), stops a run on a failure that would
repeat (no space, unmounted, host key…), says on the first push how big the seed is against the
volume and the budget, and a failure in one tier never stops the other (`✗ big: NoSpace — arena
archive resize --big-gb 300`). The pulls write their rsync temp files to each tier's own
`.rsync-tmp/` (`<dir>/<label>/`, `<dir>/big/`), beside the pod trees and on their filesystem, so a
push never ships a half-written file. The cron lines carry the start date and `ARENA_STATE_DIR`
they were installed with, and a new `cron install --start-date` rewrites them too.

`archive resize --snap-gb|--big-gb N` grows a volume online (never shrinks; the volume's current
size grows only the filesystem, finishing a resize whose `resize2fs` failed); `archive ssh [-- CMD]`
is a root shell on the pinned key; `archive ssh-config [--pod | --known-hosts]` prints the blocks
(or the laptops' known-hosts file); `archive down` deletes the server and keeps the volumes
(protected, priced as everywhere: live `/pricing`, else the list prices) for a later `up`, which
re-attaches them without formatting. `archive up` also powers on a server it finds off (a shutdown
inside it): nothing else can, the fleet commands never see it. Every surface — `up`, `down`,
`status`, `teardown --check`, `balance` — prices the archive by the same rule, the server's IPv4
included.

**A rebuilt archive has a new host key**, and Hetzner may give the new server the same IPv4. `up`
says so when the pin changes: pods get the new pin from `arena pods copy-keys --all` (their sftp
refuses the archive until then); laptops keep the archive's key apart — the block's `HostKeyAlias
archive-<prefix>` and its own `~/.ssh/arena_archive_known_hosts`, so neither a new IP nor the old
key under the same IP in `~/.ssh/known_hosts` matters. Hand out the new `arena ssh-config --out
DIR/config` (it writes `DIR/arena_archive_known_hosts` beside it, keyed by the alias), or have
participants run `ssh-keygen -R archive-<prefix> -f ~/.ssh/arena_archive_known_hosts` and compare
the new fingerprint; a laptop on a block from before this one pinned it in `~/.ssh/known_hosts`
(`ssh-keygen -R <ip>` there). Participants:

```bash
sftp archive-arena9                    # ls snap ; cd snap/w2d5/arena9-apple ; get -r … ; reget for big files
scp -r archive-arena9:snap/w2d5/arena9-apple/ARENA_materials/chapter1 ./restore   # OpenSSH ≥ 9 (else scp -s)
```

Symbolic links are stored defanged on the archive (`ls -l` shows `-> /rsyncd-munged/<target>`):
they don't resolve there, so a participant's download skips or fails on them — copy the files,
then re-create a needed link with `ln -s <target> <name>`. (The archive's own README says so.)
The pods' `archive-<prefix>` block lives in its own managed block of `~/.ssh/config`; `copy-keys`
run by a user who can't read the archive state leaves it as it is and says so.

State: `$ARENA_STATE_DIR/<prefix>/archive/` (0700) — the admin, push and read keys, the pinned
`known_hosts`, `archive.json`, the per-tier sync state and locks, `forgotten.json`. Run `archive up` as the user
whose crontab pushes (prod: root, with `ARENA_STATE_DIR` in config), and back the directory up:
losing it stops pushes and admin access, never participants' reads.

**End of the program:**
1. A final `arena pods backup`, then `arena archive sync --tier both`.
2. `arena archive status`: every label final, no FAIL.
3. Announce the laptop block (`arena archive ssh-config`) and the keep-until date.
4. `arena cron remove --archive --backup`.
5. Tear the pods down (`arena teardown --check` lists what's left).
6. Set `ARCHIVE_KEEP_UNTIL` (`teardown --check` then shows the archive as `◷ kept`).
7. After that date: `arena archive access off` → `arena archive down` →
   `arena archive down --delete-volumes --confirm <prefix>-archive` → `teardown --check` ALL CLEAR.

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
# also: RUNPOD_API=v1 (smoke the retiring v1 backend; unset = the config's line, else v2) · ARENA_LIVE_GPU=3070 (the --gpu list)
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

## AI review before merging

`scripts/ai-review` gets a second-opinion code review of a change from **codex** (OpenAI's CLI)
and/or **Claude Code** (`claude -p`), both by default, run in parallel:

```bash
git fetch origin
scripts/ai-review                        # origin/master..HEAD, codex + claude
scripts/ai-review 7fface8..04968c3 --codex --focus "the new cron tidy-up"
scripts/ai-review --pr 4                 # gh pr diff/view (GH_REPO=owner/name picks the repo)
scripts/ai-review --dry-run              # only write prompt.txt: nothing is sent
```

- **What it sends, and where.** One prompt, built from git objects only (never the working tree,
  never a file outside the repository): fixed review instructions with this repo's production
  risks (backup.sh under root's cron on every pod, the cron lines, set-branch/init-branches, the
  reaper, the archive, participants' files and the control box's home), `git log --oneline`, the
  diff (from the range's merge base) and the full text of every changed `*.sh`. With `--both`
  that goes to **OpenAI** (codex) **and Anthropic** (claude); `--codex`/`--claude` sends it to one.
  Over `--max-bytes` (600 KB) it keeps the stat and the riskiest files and names the rest as left
  out. A changed path that looks like a secret (`config.env`, `*.key`, `id_rsa*`, …) is refused.
  **Key-like strings are masked** before anything is written or sent (review ai-review claude
  #14: only path names were screened): the backup's own `SECRET_PATTERNS`, read from
  `crates/core/src/backup.rs` at HEAD (git objects, as the rest), run over the whole prompt as
  the backup runs them — each key becomes its `REDACTED-…` placeholder — and it says `masked N
  key-like strings`; `--dry-run`'s `prompt.txt` is the masked one. The repo's own test fixtures
  with fake keys are masked too. Patterns it can't read, or a key-like string left after the
  masking, refuse the run (exit 2, nothing sent).
- **No tools.** Everything the reviewer needs is in the prompt, so it runs with none, in an empty
  temporary directory: claude with `--tools ''`, `--safe-mode` (no hooks, MCP servers or
  CLAUDE.md) and no saved session; codex `exec` read-only and ephemeral, with its shell, code
  mode, apps, plugins, browser and web search turned off. (Here codex's sandbox can't run a
  command anyway: bubblewrap fails.)
- **Output** goes to `${XDG_STATE_HOME:-~/.local/state}/arena-review/<UTC time>-<range|prN>/`
  (`--out DIR` for another parent): `prompt.txt`, `codex.md`, `claude.md`, and each reviewer's
  stderr in `codex.log`/`claude.log`. It ends by printing those paths and each review's last line,
  `Blocks merging: yes|no (…)`. Exit 0 when every reviewer that ran answered, whatever it found;
  1 when one failed or timed out (`--timeout`, 3600 s each) or none could run (a missing `codex`
  or `claude` is skipped with a message); 2 for a usage error, an empty diff or key patterns it
  can't read (refused).
- **Cost.** It runs on the operator's own codex and Claude Code logins, so it spends their
  subscriptions' usage, not the project's API keys: a 16-commit, ~420 KB prompt is roughly 125k
  input tokens per reviewer, and took codex 2½ minutes and claude 5. `--effort` (default `high`,
  for both) sets how hard they think: codex's own default here is `none`, a skim that answers in
  25 s. `--model-codex`/`--model-claude` pick the models.
- It's a second opinion, not a gate: read the findings, fix what's real, note what isn't.

## Release process

Work on `dev/phase0` → `cargo test --release` + the live smoke in the sandbox → PR to master,
**running `scripts/ai-review` (or `--pr N`) before merging it** → `cargo install` to prod only
between cohort sessions, keeping the previous binary as `arena.prev` for an instant rollback.

## Layout

```
crates/
  core/   library: config, provider trait + impls, Remote, planners/judges (see ARCHITECTURE.md)
  cli/    `arena` binary (clap); tests/live_smoke.rs = the opt-in live smoke test
  tui/    `arena-tui` binary (ratatui)
scripts/
  ai-review   a second-opinion review by codex/claude (see "AI review before merging")
web/
  fleet.html  the public fleet page (reads fleet.json next to it)
docs/     RunPod API notes, known issues
```

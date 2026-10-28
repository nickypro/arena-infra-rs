# arena — command reference

Everything `arena` and its dashboard do, command by command, with every flag, config key and
caveat. The [README](../README.md) is the short tour; how the participants' work is kept —
backups, branches, pulls, the archive and the reaper — has its own page,
[BACKUPS.md](BACKUPS.md). Each command's `--help` has the short version.

## Contents

- [Overview](#overview)
- [Safety model (this is developed against live production)](#safety-model-this-is-developed-against-live-production)
- [Running arena](#running-arena)
  - [From a checkout](#from-a-checkout)
  - [`update`: installing a newer release](#update-installing-a-newer-release)
  - [Command shorthands](#command-shorthands)
  - [Targeting & concurrency](#targeting--concurrency)
  - [Overriding config without editing it](#overriding-config-without-editing-it)
- [Providers](#providers)
  - [RunPod (REST v2, the default)](#runpod-rest-v2-the-default)
  - [RunPod REST v1 (retiring)](#runpod-rest-v1-retiring)
  - [Vast.ai](#vastai)
  - [Hetzner Cloud](#hetzner-cloud)
  - [Shadeform](#shadeform)
- [Library modules (`arena-core`)](#library-modules-arena-core)
- [Pods: create, list and the lifecycle](#pods-create-list-and-the-lifecycle)
  - [`pods list | create | stop | restart | terminate`](#pods-list--create--stop--restart--terminate)
  - [`pods rename`](#pods-rename)
  - [`pods lock` / `pods unlock`](#pods-lock--pods-unlock)
  - [Creating pods: names, retries, GPUs and placement](#creating-pods-names-retries-gpus-and-placement)
  - [`offers`](#offers)
  - [`pods up`](#pods-up)
- [Proxy and SSH config](#proxy-and-ssh-config)
  - [How the proxy plans forwards](#how-the-proxy-plans-forwards)
  - [`proxy plan` / `proxy apply`](#proxy-plan--proxy-apply)
  - [Auto proxy sync](#auto-proxy-sync)
  - [`ssh-config`](#ssh-config)
  - [Publishing the fleet page](#publishing-the-fleet-page)
- [Setting pods up](#setting-pods-up)
  - [`pods setup`](#pods-setup)
  - [`pods copy-keys`](#pods-copy-keys)
  - [`pods cp` (`pods copy`)](#pods-cp-pods-copy)
  - [`keys` (OpenRouter)](#keys-openrouter)
- [Running commands and checking health](#running-commands-and-checking-health)
  - [`pods run` / `pods test`](#pods-run--pods-test)
  - [Background jobs: `run --background`, `jobs`, `logs`](#background-jobs-run---background-jobs-logs)
  - [`pods test --deep`](#pods-test---deep)
  - [`pods ask`](#pods-ask)
- [Fleet status and money](#fleet-status-and-money)
  - [`snapshot`](#snapshot)
  - [`teardown --check`](#teardown---check)
  - [`balance`](#balance)
  - [`balance set`](#balance-set)
- [Config, plans and the provider APIs](#config-plans-and-the-provider-apis)
  - [`config check | set | which`](#config-check--set--which)
  - [`plan check | show`](#plan-check--show)
  - [`gpus`](#gpus)
  - [`api get`](#api-get)
- [Backups, branches and the archive](#backups-branches-and-the-archive)
- [The dashboard (`arena-tui`)](#the-dashboard-arena-tui)
  - [Columns](#columns)
  - [Fleet columns from the core snapshot](#fleet-columns-from-the-core-snapshot)
  - [Deep checks and marked sets](#deep-checks-and-marked-sets)
  - [Asking an AI about pods](#asking-an-ai-about-pods)
  - [SSH time budgets](#ssh-time-budgets)
  - [Navigating](#navigating)
  - [Acting on pods](#acting-on-pods)
  - [Adding pods](#adding-pods)
- [Testing](#testing)
  - [The live smoke test](#the-live-smoke-test)
- [AI review before merging](#ai-review-before-merging)

## Overview

All four target verticals are implemented (multi-provider spin-up, commit/backup,
GPU/progress dashboard, proxy/port-forwarding), behind a CLI and an interactive TUI:

- the library, `arena-core` ([Library modules](#library-modules-arena-core)) and its
  [providers](#providers);
- the `arena` CLI ([Pods: create, list and the lifecycle](#pods-create-list-and-the-lifecycle)
  onwards; backups in [BACKUPS.md](BACKUPS.md));
- the `arena-tui` dashboard ([The dashboard](#the-dashboard-arena-tui)).

## Safety model (this is developed against live production)

- Runs as the unprivileged `dev` user, which **cannot read `/root`**. It only sees
  a read-only copy of config at `/home/dev/prod-ro/config.env`.
- **Read-only by default.** `pods list`, `offers` and the TUI only ever read from the providers: REST GETs plus
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

## Running arena

### From a checkout

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

### `update`: installing a newer release

Every green master commit is published as a GitHub release (`.github/workflows/release.yml`):
`arena` and `arena-tui` for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, built
on Ubuntu 22.04 (glibc 2.35 or newer), with `CHANGELOG.md` and `SHA256SUMS`. The tag is the
version, `v2.0.YYMMDD.HHMM`, from the commit's time in UTC. `arena --version` prints it with the
commit: `arena 2.0.261027.1500 (5274b7a1c2)`.

```bash
arena update --check     # newer release? what changed? (exit 10 = yes, 0 = up to date, 1 = couldn't tell)
arena update --dry-run   # also download and check it (provenance included), install nothing
arena update             # … then ask, and swap both binaries in (--yes to skip the question)
sudo arena update        # where root runs arena from someone else's ~/.cargo/bin (prod)
```

- **What changed:** the release's `CHANGELOG.md` entries that this build's own copy (compiled
  in) lacks, with their **Action needed** and **Config** lines first.
- **Checks before anything changes:** both binaries must match `SHA256SUMS`, must be proven
  built by this repository's release workflow (below), and the new `arena --version` must run
  here and name the release. A forged, wrong-platform or cut-short download never replaces what
  cron runs, and a downloaded binary is only run once it is proven.
- **Build provenance:** `release.yml` attests each binary (Sigstore: a statement naming the
  file and its SHA-256, signed with a certificate GitHub Actions' OIDC token got for the run,
  logged in Rekor). `update` reads each binary's attestation from GitHub's API (anonymous) and
  verifies it here, natively, with no `gh` and no login: the signature; the certificate's chain
  to Sigstore's public-good root (compiled in) and its certificate-transparency timestamp; the
  Rekor entry (inclusion proof, checkpoint, signed timestamp); SLSA provenance naming that file
  with that digest; and who signed: `.github/workflows/release.yml` on `refs/heads/master` of
  `nickypro/arena-infra-rs` (GitHub's id 1368824951) on a GitHub-hosted runner. It says, per
  binary, which run built it. `SHA256SUMS` alone comes from the same release, so it can't prove
  that. No attestation, or one that doesn't verify: nothing is installed, and it says why.
- **`--skip-attestation`:** installs without the provenance check (`SHA256SUMS` is still
  checked), with a loud warning. It's for one case: a genuine release this build can't verify,
  because Sigstore changed its keys or GitHub its attestation format after this build was made.
  Check the release by hand first (`gh attestation verify arena-<target> --repo
  nickypro/arena-infra-rs`, gh 2.49 or newer, on any machine), then update once with it; the new
  build knows the new root. `--check` never reads attestations.
- **The swap:** where this `arena` really is (symlinks resolved), the current binaries become
  `arena.prev` / `arena-tui.prev` (an older `.prev` is replaced). Each new one takes the old
  one's mode and owner, and lands by rename, so the path always holds a whole binary.
  Roll back with `mv -f arena.prev arena && mv -f arena-tui.prev arena-tui`.
- **Afterwards:** it lists the `arena-tui` dashboards still running the replaced build, to
  restart. Cron picks the new `arena` up on its next run.
- It needs no config and no GitHub login (the API allows 60 unauthenticated requests an hour;
  an update makes 3, with the attestations).

### Command shorthands

Subcommands accept any **unambiguous prefix** (Cisco-style), at every level:

```bash
arena po li         # == arena pods list
arena tui           # launch the dashboard
arena co c          # == arena config check
arena po ba          # == arena pods backup (prompts to confirm)
```

An ambiguous prefix errors and lists the candidates — e.g. `arena p` is rejected
because it matches both `pods` and `proxy` (use `po`/`pr`); likewise `c` →
`config`/`cron` (use `co`/`cr`), and `arena po l` → `list`/`lock`/`logs`.

`arena --help` and `arena pods --help` list their commands in groups — `pods`: Look, Make,
Change, Move, Save, Stop; the top level: Fleet, Money, Access, Automation, Setup. A test keeps
every visible command in exactly one group. Hidden: `gpus`, the old name for `offers --all`.
clap's own `help` command isn't listed (`-h`/`--help` are); `arena help pods` still works.

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
`ARENA_START_DATE`, `SSH_PROXY_RELOAD_CMD`, `RUNPOD_API`, `DISCARD_MAX_GB` and the `ASK_<AI>_MODEL`/`_EFFORT` keys, e.g. `RUNPOD_API=v1 arena pods
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

## Providers

### RunPod (REST v2, the default)

`provider::runpod_v2` — RunPod REST **v2** backend (`api.runpod.io/v2`), the **default**:
`RUNPOD_API` unset or empty means v2 (`v1`/`v2` in any case select one; any other value is an error,
which quotes the value only if it's short and version-like — never a pasted key; `config check`
shows which is active, `(default)` when unset).

Why the default: v1 retires on 2026-11-15, and v1 can only ask for hosts on CUDA ≤ 13.0 (below),
which starved RunPod placement — "no capacity" all day once the 13.0 hosts were taken.

Same provider name and commands. Differences: real lifecycle statuses
(`PROVISIONING`/`STARTING`/`RUNNING`/`EXITED`/`ERROR`); the SSH endpoint comes from `ssh.direct`
(else the TCP 22 runtime mapping, never RunPod's SSH gateway). The list has left both out since
2026-10-26, so running pods are filled from `GET /v2/pods/{id}`, and the endpoint is remembered
while the pod keeps its `startedAt`: a dashboard refresh costs no extra request. It's rechecked
every 30 s for the first 3 minutes after a start (live: a restart's detail kept the old port for
~30 s), then every 10 min, at most 30 rechecks a minute per process; arena's own
stop/start/restart forget it at once. A failed lookup leaves just that pod without an endpoint.
Create always sends `cloud` (v2 defaults to SECURE) and merges the account's registered SSH
keys into `PUBLIC_KEY` (v2 skips them when it's set); `replace` recovers GPU type + cloud tier.

Rename is `PATCH /v2/pods/{id}` with only `{"name"}` (live-verified restart-free — PID 1, a `/root`
marker and the endpoint survive); the GPU catalog, prices, stock and the `--gpu` check come from
`GET /v2/catalog/gpus` (each tier fetched once per run).

Error bodies: a `422` (e.g. `Unknown GPU type: …`, `Request validation failed.`) is a config error —
never capacity, never retried, message kept; a `400 Pod is locked` (stop/restart/terminate on a
`locked` pod) is its own `Locked` kind.

**Locks** are `PATCH /v2/pods/{id}` with only `{"locked": bool}` (live-verified: the container is
untouched); the pod's `locked` field (in every list/GET) says which pods are — `actions` still lists
stop/restart/ terminate while locked, so it's never read for that.

**GraphQL dependency inventory** (GraphQL is reportedly retiring too, "early 2027", unverified) — on
v2 it is left only for the maintenance window (`enrich`, best-effort: a failure is one warning line,
GPU/$ still shown), the live account balance (v2 has billing *history* only — which `balance`'s
estimates use once GraphQL is gone) and, as a fallback after REST v2, `teardown --check`'s volume
listing; see the module doc of `runpod_v2.rs`.

Contract tests replay scrubbed recorded v2 responses (`provider/fixtures/runpod_v2/`).

**Host CUDA is a floor**: `MIN_CUDA_VERSION` (or `ALLOWED_CUDA_VERSIONS`, now read as a floor — its
lowest version) goes out as v2's `gpu.minCudaVersion`, so any newer host qualifies (an exact
`["13.0"]` had made every 13.1/13.2 host — all B300 MIG ones — "no capacity").

REST v1 has no floor and its schema stops at 13.0: it sends every listed version ≥ the floor (13.1+
hosts unreachable; a 13.1+ floor or `EXACT_CUDA_VERSIONS` entry is refused before the request:
`remove RUNPOD_API=v1 (v2 is the default)`).

`EXACT_CUDA_VERSIONS` is the opt-in exact set (`allowedCudaVersions`; never sent with a floor).

A value that isn't a version (`13.x`, `newest`) fails closed everywhere — every create (RunPod and
Vast) is refused and the deep check won't derive a driver floor from it — never read as "no floor".

v1 creates also send `supportPublicIp: true` (some community hosts have no public IP, leaving only
RunPod's SSH proxy). **v2 has no such field** (its OpenAPI `CreatePodRequest`, checked 2026-10-11,
offers no public-IP option, and its schema refuses unknown keys), so a v2 COMMUNITY pod can land on
a proxy-only host (live: two 4090s did): `up` calls it a host failure after the 5-minute grace —
`FAILED endpoint`, or replaced under `--check`; SECURE pods have a public IP.

Neither API reports a pod's CUDA constraint, so `replace`/`migrate copy` re-apply the configured one
to the replacement (as `create` does).

### RunPod REST v1 (retiring)

`provider::runpod` — RunPod REST **v1** backend: list / create / stop / terminate. RunPod retires
REST v1 on **2026-11-15**; used only with an explicit `RUNPOD_API=v1` (which `config check` warns
about, with the date and `remove RUNPOD_API=v1 (v2 is the default)`).

### Vast.ai

`provider::vast` — Vast.ai REST backend against the same trait. Vast rents *offers* rather than
named pods, so `create_pod` searches the marketplace (`PUT /search/asks/`, filters wrapped in `{"q":
…}`) for the cheapest rentable offer matching the spec — exact GPU count, disk, CUDA floor
(`MIN_CUDA_VERSION` / `ALLOWED_CUDA_VERSIONS`), open ports, **verified hosts only** (the vast CLI's
base filters; `VAST_ALLOW_UNVERIFIED=1` admits unverified/deverified hosts — the verified market for
a card can be empty), and `--max-price` (checked again at create: offers move) — and rents it with
runtype `ssh_direc ssh_proxy` (what the vast CLI sends for `--ssh --direct`), carrying the machine
name as the instance `label`. An offer's price is what it bills with *our* disk: the larger of
`dph_total` and `dph_base` + `storage_cost` for `DISK_GB`. GPU types map to Vast's spaced display
names (`NVIDIA GeForce RTX 3090` → `RTX 3090`, `RTX 4000 Ada Generation` → `RTX 4000Ada`; `RTX_3090`
finds nothing).

Vast's SSH launcher replaces the image's entrypoint; the cohort keys are attached to **each
instance** (`POST /instances/{id}/ssh/`, idempotent) and written by its `onstart` (which also turns
off Vast's auto-tmux) — never registered on the Vast account.

Pods are listed with the **direct** SSH endpoint (host IP + mapped 22), falling back to Vast's SSH
proxy only for a running instance without one, and carry Vast's `machine_id`.

A taken offer (`no_such_ask`, 410, "not your own") rents nothing, so the next fitting offer is tried
(3 at most), then placement moves on; any other refusal stops, and so does a rent whose outcome is
unclear (any 5xx, no instance id, the connection lost after sending) — rather than risk a second
instance. The key attach after a rent is bounded (60s).

### Hetzner Cloud

`provider::hetzner` — Hetzner Cloud backend for **CPU-only** VMs. Not a GPU container host, so it
ignores the GPU-centric `PodSpec` fields and takes its sizing/OS/location/SSH-keys from `HETZNER_*`
config (`create`/`up --server-type` over `HETZNER_SERVER_TYPE`; `--gpu`/`--gpus` are refused there,
`--server-type` everywhere else).

**Location:** `HETZNER_LOCATION` unset (or `any`) means any location. A create reads the
server types (`GET /server_types`) and tries every location that sells the type now, nearest
first. When a location has no room (`resource_unavailable` / "error during placement"), it tries
the next one with the same name.
- Nearest means the quickest TCP connect from the machine running arena to the location's
  speed-test host (`nbg1-speed.hetzner.com:443`): timed once per process, about a second at
  most. On prod that machine is the proxy box, which carries every student's SSH. From nbg1:
  nbg1 0.4 ms, fsn1 3, hel1 24, ash 95, sin 159, hil 167.
- Equally near locations go cheapest first, then those not marked low on stock, then Hetzner's
  recommended ones. A location that can't be timed goes after the timed ones, by price.
- No room anywhere is a capacity error that names each location tried, so `--keep-trying` waits.
- Any other refusal (token, quota, throttle, SSH key) stops at once.
- One location (`nbg1`) pins creates there, with no lookup. A list (`nbg1,fsn1`) is tried in its
  order, and `nbg1,any` tries nbg1, then the rest.
- An `--api-json` `datacenter` is one create where it says.

VMs come up on a real public IP with SSH on `:22`, so they use the same proxy/`pods up` flow as GPU
providers. A refused create is explained from the project (read-only `GET /ssh_keys` + `GET
/server_types`): a missing SSH key names the project's keys and what to set or upload; a type the
location doesn't sell lists the ones it does.

### Shadeform

`provider::shadeform` — Shadeform (one REST API over ~18 GPU clouds), **opt-in overflow only**
(`--provider shadeform`, never a default): a pod is a **bare GPU VM**, not a container from the
cohort image, and it **bills until deleted — no stop/start, no volumes** (`VOLUME_GB>0` is refused).

Placement is Vast-like: `create_pod` reads the catalog (`GET
/instances/types?gpu_type=…&num_gpus=…&available=true&sort=price`) and creates on the cheapest
**in-stock** `(cloud, region, type)` entry that passes every filter, re-checked client-side: the
cloud in `SHADEFORM_CLOUDS` (default `massedcompute` — in practice the only cheap cloud with CUDA-13
images; `*` = any, with a `config check` warning) and the region in `SHADEFORM_REGIONS`, a VM (bare
metal only with `SHADEFORM_ALLOW_BAREMETAL=1`), on-demand (spot never), an OS image at or above the
CUDA floor (`MIN_CUDA_VERSION` / `ALLOWED_CUDA_VERSIONS`: the newest
`ubuntuNN.NN_cudaX.Y_shade_os`), a worst-case boot ≤ `SHADEFORM_MAX_BOOT_SECS` (1200), and a price ≤
min(`--max-price`, `SHADEFORM_MAX_PRICE` — default $1.00/h, the hard ceiling). A capacity refusal
tries the next entry (3 at most).

GPU names map from RunPod's ids (`NVIDIA RTX A6000` → `A6000`; an unmapped one is refused before any
request — Shadeform answers a typo with an empty catalog).

The key goes in `X-API-KEY` only. Every create carries a fresh **nonce tag** (`arena`,
`arena-<prefix>`, `arena-nonce-<uuid>`): an answer that might hide an instance (a 5xx, no id, a
timeout or a dropped connection after sending) is settled by listing for that tag — adopted if
found, else the run stops with "may have been created" (never retried, never the next option).

After create the instance's price is checked again: above the cap or 10 % over the quote → deleted
at once.

Root SSH comes from a **bootstrap script** (`launch_configuration`, run by Shadeform when the VM
turns active) that authorizes the cohort keys for root and the `shadeform` user and writes the nonce
to `/var/lib/arena/shadeform-nonce`; the endpoint (`ip`, `ssh_port` or 22) is withheld until the VM
has been active 60 s. The create uses the uploaded cohort key `SHADEFORM_SSH_KEY` (default: the
prefix), never silently the account's Shadeform Managed Key (whose private half Shadeform holds;
`default` opts into it).

The listing shows **every** instance on the account; arena only terminates, renames or restarts one
whose name has the prefix **and** that carries `arena-<prefix>` — anything else is refused with no
request; a rename must keep the `<prefix>-` name (else arena couldn't touch it again).

A new instance that goes to `error` before it has an endpoint, or isn't up within
`SHADEFORM_READY_MINS` (15) or `--timeout`, whichever comes first, is deleted and the name placed on
the next option (`up`, with or without `--check`). A price that can't be read after create (`info`
retried with backoff for ~30 s, then the listing) gets the instance deleted and the next entry
tried.

Setup is the bare-VM script in its GPU mode (`ARENA_GPU=1`: the CUDA requirements kept, `uv
--torch-backend auto`, and the env must see the GPU). The bootstrap leaves each cohort key as one
plain line in root's `authorized_keys` (a cloud-init `command="echo 'Please login as …'"` line for
it is dropped).

`pods stop/start` skip it — said, the rest stopped/started, and `stop` exits non-zero while one
still bills; a selection of nothing else is refused. Restart is gated as a wipe (unverified); no
lock; the reaper doesn't act on it yet.

Balance: the undocumented `GET /account` (`balance`, `hourly_burn` only — its personal fields are
never kept); any failure there is "unknown".

`api get shadeform sshkeys --raw` is refused however the path is spelled (`v1/`, case,
percent-encoding), and Shadeform output never shows a private key or the account's personal fields,
`--raw` included. `teardown --check` lists the account's Shadeform volumes too.

Spec and recorded fixtures: `specs/shadeform.md`, `crates/core/src/provider/fixtures/shadeform/`.

## Library modules (`arena-core`)

`arena-core` — library

- `config` — tolerant parser for the existing `config.env` (incl. the `MACHINE_NAME_LIST` bash
  array), so this tooling reads the *same* config as the legacy bash/python scripts.
- `provider::Provider` — the single trait every backend implements.

- `naming` — next-free machine-name allocation, mirroring the legacy logic.
- `snapshot` — one read-only picture of the fleet (`FleetSnapshot`, built by one pure `build` that
  the CLI and the TUI share), the **health cache** of the last deep check per pod, and the
  allowlisted `PublicSnapshot` for the web page.
- `teardown` — the pure judge behind `teardown --check`: listings, volumes, keys, cron/`at` and
  proxy inputs in, a `✓ ✗ ? –` checklist with fix commands out (unknown ≠ empty).
- `balance` — prepaid account balances and the fallback chain: live (RunPod GraphQL `myself`, Vast
  `users/current`, Shadeform's undocumented `account`), else an estimate from the last reading minus
  max(billed, metered) spend, else `?`; burn, runway, levels, the `balance.json` state and every
  surface's rendering (Hetzner postpaid); `idle` — the read-only `pods idle` probe script, its reply
  parser and the verdict/report (unknown ≠ idle).
- `ask` — `pods ask` and the TUI's `?`: a read-only, masked, size-capped state bundle per pod (one
  SSH call reusing the metrics probe and the jobs reads, plus `probe.sh`), the one prompt for all of
  them, and claude/codex run with no tools (see [`pods ask`](#pods-ask)).
- `reap` — the opt-in auto-reaper (`pods reap`), pure but for its state store: the strict
  `reap.json` types and settings (`REAP_*`, file-only), the read-only probe (`idle_probe.sh`, framed
  by a nonce, indexed and `cksum`-closed so a damaged reply is never "idle") and its parser,
  classification / idle streak / due rules, the decision table (spec rows G1–G7, R1–R40), the typed
  proofs and `ReapTicket` (the only way reap code reaches a terminate), the ENDS column, plan table,
  notice, cron line and JSONL log line.
- `lock` — pod locks (RunPod v2, `Pod::locked`): the one refusal sentence (`devtest-x is locked —
  `arena pods unlock devtest-x` first`), the up-front refusal over a selection, the mapping of the
  API's `Locked` error to it, and `pods lock`/`unlock`'s plan.
- `apiextra` — `--api-json` / `CREATE_EXTRA_JSON`: parse (one JSON object), deep-merge into a
  backend's create body, and refuse the fields each backend sets itself (its `MANAGED` table +
  "ports must keep `22/tcp`").
- `apiget` — `arena api get`: the provider API bases, the path guard (relative only; absolute URLs,
  `//host`, `..`, `#` refused; the built URL re-checked against the base) and secret redaction (env
  blocks, key/token/password-named fields, `rpa_`/`sk-`/`hf_` tokens, SSH key material, PEM blocks;
  the API key itself never printed).

Shared library pieces: `ssh` (non-interactive, fail-fast SSH command build + run),
`remote` (the `Remote` trait every pod-SSH path goes through: `SshRemote` for real, a
scripted `FakeRemote` in tests), `metrics` (nvidia-smi parsing + per-pod aggregation), and
`provider::build` (the one factory that constructs a backend by name — used by both the
CLI and TUI).

All four target verticals are now in place: multi-provider spin-up (RunPod/Vast/
Hetzner), commit/backup, the GPU/progress dashboard, and proxy/port-forwarding.

## Pods: create, list and the lifecycle

### `pods list | create | stop | restart | terminate`

`pods list | create | stop | restart | terminate` (`--provider runpod|vast|hetzner|shadeform`; Vast
reads `VAST_API_KEY`, Hetzner reads `HETZNER_API_KEY` + `HETZNER_*`, Shadeform `SHADEFORM_API_KEY` +
`SHADEFORM_*`). `restart`/`terminate` take **one** pod (name, bare name, `@name` or id — a name two
pods share is refused: pass the id); `stop` takes the shared [target
selection](#targeting--concurrency).

#### `pods list`

`list` shows NAME PROVIDER ID STATUS (`run locked` for a locked pod) GPU (`count×type`; a MIG slice
as the slice — `1×B300 MIG 34GB`, never the parent card nvidia-smi names, and `up`'s placement,
READY and summary lines name it the same (the live catalog's name, else read off the id: `NVIDIA
B300 SXM6 AC MIG 1g.34gb` → `B300 MIG 34GB`); RunPod's GPU is its display name on v1 and v2 alike,
so `--json`'s `gpu_type` doesn't change with `RUNPOD_API`) $/H ENDPOINT (`no public endpoint (RunPod
ssh proxy only)` for a pod up without a public IP) MAINT (the host's RunPod maintenance window, e.g.
`maint 10-09 02:00→06:00 UTC`; the host's free-text note is flattened onto one line) and a footer
`fleet: $X/h across N billing pod(s)` summing the **billing** pods (Hetzner's € shown separately,
unpriced pods counted; `— PARTIAL: vast didn't answer …` when a provider failed to list, and no "(no
pods)" claim then). "Billing" is one rule (`status::is_billing`): running or on its way up — RunPod
v2 `PROVISIONING`/`STARTING`/`ERROR` too, Hetzner `initializing`, Vast `loading` — not
`EXITED`/`STOPPED`/`TERMINATED`/`off`; a Hetzner server bills while it exists, powered off included.
A non-billing pod's `$/H` shows `-` (`--json` keeps the raw `cost_per_hr`).

Under the footer (table only, `(no pods)` too) one `balance: runpod $32.54 live ≈4.0d · vast
~$118.30 est. ≈49.2d` line (see `balance`; `⚠`/`‼` with the threshold or the consequence when
low/critical, `?` with the `set` nudge when unknown; an opt-in account — Shadeform, never in the
default provider chain — at $0 with nothing billing there reads plainly, `shadeform $0.00 live
unfunded`: it's unfunded on purpose, and alarms like any other once a pod bills there), observed
while the list runs — within 5 s, a live read only when the last one is 5 min old, silent whatever
happens; `--json` reads no balance at all.

The maintenance window comes from one extra read-only RunPod GraphQL query per `list` (on REST v2,
the default, the REST list already has GPU and $; on `RUNPOD_API=v1` that query supplies them too)
(best-effort: if it fails you get one warning line and the list still renders); the `nvidia-smi`
probe over SSH (default for the table, `--probe`/`--no-probe`) overrides the GPU when a pod answers.

`list --json` emits the pods incl. `gpu_count`/`cost_per_hr`/`maintenance`.

#### `pods restart` and `pods stop`: the wipe gate

**`restart` WIPES a RunPod pod's container disk** (live finding: on REST v2 — the default — the
container is reset to its image — everything outside a `/workspace` volume, `~/.name`, setup's git
remote and keys are gone, so only `VOLUME_GB` keeps work across a restart; a v1 restart
(`RUNPOD_API=v1`) was seen keeping it, so on v1 it is gated and set up again the same way, but the
output only ever says it *may* have reset; Vast's stop+start is treated the same, unverified; a
Hetzner hard reset keeps the VM disk).

The prompt says what's lost and what survives (the volume size comes from the pod's spec); unless
the ARENA repo sits on a confirmed `/workspace` volume, the restart is **refused unless
`--wipe-ok`** (even with `--yes`): a volume elsewhere doesn't save the participants' work. With a
volume, `setup` puts the repo on it (see `pods setup`: `BACKUP_REPO_PATH`, default
`/root/<ARENA_REPO_NAME>`, becomes a link to `/workspace/<repo>`), so the gate asks the pod
(read-only, 20s) where its repo **really** is — `readlink -f` + is `/workspace` mounted — and lets a
repo linked onto the mounted volume through without `--wipe-ok`; no answer → judged by the
configured path, as before (refused). Pods without a volume are never asked.

Afterwards it waits for the endpoint to settle, **re-runs `setup`** on the pod (`--no-setup` skips)
and syncs the proxy (`--skip-proxy`) once the endpoint has settled **again** (a reset container's
endpoint can flap after it first settles; a forward the sync still couldn't route is re-synced once,
then named with `arena proxy apply`; a pod that never got an endpoint back is waited for once, not
again by the sync), so it comes back usable; if that setup fails the error says to run `pods setup <name>`,
not another restart.

The re-run setup moves a repo on the default branch up to origin only as a fast-forward of a
**clean** checkout: one with tracked changes or commits of its own (the participant's, on the
volume) is left as it is, with a warning on the pod's line — `pods setup --force` is the only thing
that resets it.

A **stopped** pod is refused (`… is stopped (EXITED) — … arena pods start …`: a restart needs a
running pod). `stop` gets the same gate (`--wipe-ok`): a stopped RunPod pod keeps no data.

A Hetzner `stop` asks the VM to shut down (ACPI, so it flushes its writes) and waits up to 90 s for
it to be off; only one that isn't is powered off, and the output says so (`⚠ … powered off — writes
not yet on its disk may be lost`).

#### `pods start`

**`pods start <targets>`** (targets or `--all`) is the way back from a stop (RunPod v1 `POST
/pods/{id}/start`, v2 action `start`, Vast state `running`, Hetzner `poweron`): only stopped pods
are started (others skipped with a note), each comes back as a fresh image, so it waits for its
endpoint to settle, **re-runs setup** (`--no-setup`) and syncs the proxy (`--skip-proxy`); a Hetzner
VM kept its disk (no setup). A start the provider refuses (no free GPU left on the host) is reported
per pod, exit non-zero.

The SSH commands (`run`, `test`, `setup`, `backup`, `pull`…) skip a stopped pod saying so — `skip X
— stopped (EXITED); arena pods start X brings it back` — even when the listing still shows its last
endpoint. `run` / `pull` / `backup` ask the provider **once more** (after 20 s) for a RUNNING pod
listed without an endpoint (RunPod v1 once listed one that way for ~20 s, and a backup tick skipped
it).

#### `pods reimage`

**`reimage`** gets the same `--wipe-ok` gate as restart/stop (a reimage is a fresh container on
every backend; the dry run says when it would be refused) and the same come-back: settled endpoint,
**setup again** (`--no-setup`), proxy sync.

#### `pods terminate`

`terminate --all` tears down the **whole fleet** (confirms first; `--dry-run` lists every pod
without touching them) — for end-of-program teardown (`terminate <name> --all` is refused).

`terminate … --revoke-key` also deletes each terminated machine's OpenRouter key(s) (found by the
name `keys gen` gave it, over the full paginated key listing; needs `OPENROUTER_PROVISIONING_KEY`)
and drops its row from `keys/openrouter_api_keys.csv` — only for pods that actually terminated, and
never for a name another pod still holds (a double create's twin removed by id keeps the survivor's
key); a failed revoke keeps the row, is named, and makes the exit non-zero without stopping any
terminate.

#### Stopping many pods

`stop apple..mayor` / `stop --all --exclude bloom` stops many at once (needs targets or `--all`):
every selected pod in a billing state (running, starting/provisioning/…, or ERROR) is stopped,
others are skipped with a note. (The legacy stop→wait→delete `kill` is gone: `terminate` deletes
directly.)

### `pods rename`

`rename <old> <new>` / `rename --from-prefix <p>` renames the pod (metadata only, no restart) and
then brings along what's keyed by the name: rewrites `~/.name` over SSH (setup's exact `export
MACHINE_NAME='<short>'` line; an unreachable pod is reported with a `pods setup <name>` hint, never
fatal), moves the machine's row in the keys CSV (the per-cohort file the symlink points at — or,
after a prefix change, *into* the current prefix's file, repointing the symlink and carrying the
other live pods' rows, so the next `keys gen` doesn't strand them) and renames its key on OpenRouter
in place (`PATCH /keys/{hash}` `name`; skipped quietly with no CSV / no provisioning key; a new name
that already has its own row or key is left alone and warned about), then syncs the proxy.

The pod's `MACHINE_NAME` env var keeps the old name (env only changes with a reimage) — the output
says so. `--dry-run` lists every step.

### `pods lock` / `pods unlock`

`pods lock [targets]` / `pods unlock [targets]` (targets or `--all`; RunPod on REST v2, the default)
— a **locked** pod is refused stop, restart and terminate by RunPod itself, from **any** client
(this tool, scripts, RunPod's web console; live-verified 2026-10-08) — the protection for a running
cohort. Setup, backups, `run` and SSH are unaffected. Already-locked pods are left alone; a pod on a
backend that can't lock (`RUNPOD_API=v1`, Vast, Hetzner) is reported (`not supported on … — …`; on
v1 `remove RUNPOD_API=v1 (v2 is the default) to lock RunPod pods`) and makes the exit non-zero.

`pods up --lock` locks each pod as the pipeline's **last** step (after `--check`, whose replacement
needs to terminate; a lock that fails is `FAILED lock`, the pod left running, unlocked); `--lock` is
refused up front where pods can't be locked.

While a pod is locked, `stop` / `restart` / `reimage` / `terminate` / `replace` / `migrate cutover`
/ `migrate finish` refuse it **up front** — the whole command, before anything is touched — with
`devtest-x is locked — `arena pods unlock devtest-x` first`; the API's own `400 Pod is locked` (a
pod locked since the listing) reads the same.

Nothing unlocks on its own, except **`terminate --unlock`** (e.g. end of program: `pods terminate
--all --unlock`), which says so in its confirm text and unlocks each locked pod right before its
terminate (a failed unlock = not terminated; a pod locked since the listing is unlocked and retried
once). With `--all` it lifts **only the cohort's** locks (`{prefix}-…`): a locked staff/personal box
on the same account (`@` entry, or off the list) is **kept** — not unlocked, not terminated — and
the exit fails; to remove one, name it alone (`pods terminate james-gpu --unlock`).

`rename` is *not* refused: the spec only blocks stop/reset and a rename is restart-free (unverified
on a locked pod — if RunPod refuses it, the same sentence says so).

`teardown --check` marks locked pods and puts `--unlock` in their fix (on the `--all` line when any
provider holds one).

### Creating pods: names, retries, GPUs and placement

#### Explicit names, `--image`, `-n` / `-a`

`create` also takes **explicit names** (`pods create apple bloom`) and an **`--image`** override,
alongside `-n` (target number on `--provider`; pods on other providers don't count) / `-a` (add).
Bare names get the configured prefix; names already present are skipped.

Every name-picking listing — `-n`/`-a`, explicit names, placement's retry rounds, `rename`, the
TUI's add-pod form — must come from **every** configured provider: if one fails to list, the command
refuses (naming the provider; a rejected key is named too), since a name a pod there holds would
read as free and be created twice (live: a bogus `VAST_API_KEY` had `create delta` plan a duplicate
of Vast's `devtest-delta`). `pods list` still shows what answered, its footer marked `PARTIAL`.

Between `--retry-mins` rounds, a backend that is only throttled or erroring (429/5xx, past the
listing's own retries) or whose list stalled past its 60 s bound skips that round — nothing is
created from the partial listing — and the next round lists again; a rejected key ends the run.

#### `--retry-mins`: keep topping up

`create`/`up` take `--retry-mins <M>` (`--retry-secs`, default 60) to **keep topping up to the
target** while capacity is short — one round per interval for up to M minutes, **Ctrl+C** stops
early keeping what was made. A `no instances available` capacity error is recognized as such (it
waits), not treated as fatal.

A round starts only while it still fits in the window (no create after it closes).

The confirm prompt lists the exact pod names about to be created, and a later round only retries
those not made yet (after a whole-fleet listing: one that exists now is skipped, a met `-n` target
stops it) — never a name picked afresh, which under `up` could be one whose pod `--check` is
replacing right then.

#### `--gpu` is checked

**`--gpu` is checked** (`create`/`up`/`offers`/`replace`/`migrate copy`, RunPod only): each token —
an exact id (any case), the catalog's display name as `arena offers --all` prints it (`RTX PRO 6000`, `B300
MIG 34GB` — checked before the aliases, so `RTX PRO 6000` is the Server Edition, as listed), an
alias (`3070`, `4080super`, `L4`, `2000ada`, `A4500`, …) or a unique catalog short name (`RTX 3070`,
`H100 SXM`) — must be in RunPod's live GPU catalog (`GET /v2/catalog/gpus` on v2, the default;
GraphQL on `RUNPOD_API=v1`), else the command stops before anything is created: ``--gpu: `3070x`
isn't a RunPod GPU type — did you mean `NVIDIA GeForce RTX 3070` (RTX 3070)?``.

If the catalog can't be fetched it warns and passes the flag through unchecked.

On REST v1 an id outside v1's create enum (it lags the catalog — the PRO 6000 MIG slices) is refused
too, saying v2's create takes it: `remove RUNPOD_API=v1 (v2 is the default)`. Vast/Hetzner: passed
through as before.

#### Multi-option placement

**Multi-option placement** (`create`/`up`): `--gpu A4000,4000Ada,3090 --cloud community,secure
--max-price 0.5 [--order cheapest|listed]`. The gpu × cloud options are priced (RunPod's live
catalog — v2 `/catalog/gpus` per tier (the default), once per tier per run, GraphQL on
`RUNPOD_API=v1` — falling back to preset estimates, shown `~`), capped per pod (price × `--gpus`;
with a cap an option with no known price is dropped, not risked) and ordered: `cheapest` (default; a
price tie goes to the better stock hint, then the listed order) or `listed` (GPU-major).

Stock never filters — creating is the truth.

Per name the options are tried **one create at a time** (never two creates for one name); a capacity
error skips that option for the rest of the round; a v2 create `403` ("no access to the requested
pool") skips it for the whole run (`[no access]`), and if *every* option is refused the run aborts
as an access problem; an option whose request the API rejects as invalid (v1's `400 … schema
requirements`, v2's `422 Unknown GPU type`) is skipped for the run too (`[rejected]`; the error
keeps v1's `problems` list) and the run fails once every option is refused (rejected or denied, in
any mix — at once, not after idling out the window); auth aborts, any other error stops as before.

`--retry-mins` re-runs rounds (blocks cleared, fleet re-listed first so a name that appeared
meanwhile — or a met `-n` target — isn't created again) only while the next round would still start
inside the window; Ctrl+C between rounds keeps what was made.

Ends with a per-name table: `created on 1×RTX 3090 COMMUNITY ($0.22/h) (after 1×RTX A4000 COMMUNITY:
capacity)` or `not placed (tried: …)`.

Cloud tiers exist only on RunPod (Vast/Hetzner collapse them, and Hetzner the GPU list too, with a
note).

Vast options are priced **live**: one marketplace search per GPU (needs `VAST_API_KEY`), the
cheapest fitting offer's price for the disk asked for — a snapshot, since each create searches again
(never above `--max-price`); a GPU with no offer right now is unpriced, with a note. Hetzner's
option is unpriced.

`--keep-trying` stays single-option (use `--retry-mins`). One `--gpu`, one `--cloud` and no
`--max-price` is exactly the old single-spec path; a list that collapses to one option (`--gpu
A4000,a4000`, `--cloud community,`) takes it too, creating the parsed option (never the raw list
text).

`--dry-run` prints the option table and the names it would attempt; the option order is fixed once
confirmed.

#### `--api-json` and `CREATE_EXTRA_JSON`

`--api-json '<object>'` on `create`/`up` (plus config `CREATE_EXTRA_JSON`, single-quoted; the flag
merges over it): extra fields **deep-merged** into the provider's create body for API options
without a flag — e.g. RunPod v2 `{"dataCenterIds":["EU-RO-1"]}`, `{"globalNetworking":true}`, a
network volume `{"mounts":{"network":[{"volumeId":"…", "path":"/data"}]}}` (with `--volume 0`:
RunPod takes a network volume *or* the pod volume, so one next to `--volume`/`VOLUME_GB` > 0 is
refused up front), `{"gpu":{"minRamPerGpu":32}}`; v1 `interruptible`; Vast `price`; Hetzner
`labels`/`user_data`.

Objects merge key by key (env adds variables), arrays and scalars replace.

Fields arena sets itself are **refused before any API call** with what sets them instead
(name/label, image, GPU id/count, CUDA, cloud tier, disk, `mounts.persistent`/volume,
`env.PUBLIC_KEY`/`MACHINE_NAME`, the start command, `startSsh`, `locked` → `up --lock`, Vast
`runtype`/`onstart`, Hetzner type/image/location/keys) — and a template (`templateId`, Vast
`template_hash_id`), which would fill those in behind the tool's back, and v1 `computeType` — and so
is a `ports` list without `22/tcp`; invalid JSON or a non-object too.

`--dry-run` prints the merged body (env values and keys redacted). `config check` shows
`CREATE_EXTRA_JSON`'s keys.

The **flag** is `create`/`up` only; the **config key** is a create default like `GPU_TYPE`, so it
also reaches the TUI's add-pod form and the replacement pod of `replace` / `migrate copy` (their
plans list its keys; a refused one stops them before anything is created). It applies to whichever
provider creates, so keep it to fields that provider takes (an unknown field is the API's error).

#### Provider errors, retries and backoff

Batch create (`create`/`up`) uses **typed provider errors** (`ProviderErrorKind`): on **capacity**
exhaustion it stops gracefully and keeps the pods it got (e.g. "created 6 of 10") rather than
erroring — `--keep-trying` instead waits and retries; on **auth** failure it aborts immediately.
Already-created pods are never rolled back.

**Transient** failures (429 / 5xx / connect-timeout) are retried automatically with exponential
backoff (`retry` module) around create and list calls — so a throttle or blip doesn't fail the
command.

Every backend reads a response's **status before its body** (`http` module), so a bad key's `401`
with an HTML or empty body is `Auth` (`… HTTP 401 Unauthorized: …`), never "error decoding response
body".

### `offers`

`offers [--gpu …] [--cloud …] [--max-price …] [--gpus N] [--server-type T] [--order …] [--all]
[--json]` — read-only: what `create`/`up` would try, on every provider at once. One table over the
providers with a key in the config, in the fleet's order (RunPod, Vast, Hetzner): PROVIDER, `#` (the
order `create`/`up` would try that provider's options in), OPTION, CLOUD, SIZE (VRAM per GPU — the
live catalog's, else the preset's — or vCPU/RAM for a Hetzner VM), PER POD (`$`/h; `~` = preset
estimate; Hetzner's in `€`, as it bills, never converted), SOURCE (`live`, `estimate`, or `list` for
Hetzner's list price, which placement doesn't use), STOCK; then what
the cap dropped and why, each provider's notes and the providers left out. With no key at all,
RunPod alone (priced from the presets), as before.

Each provider's rows are made by the functions `create`/`up` run — the `--gpu` check, the request
from the flags and the placement planner — so the order and prices are theirs, and a change to
placement shows here unchanged. `create`/`up` use **one** provider per run (`--provider`, default
`runpod`): `#` orders the options within a provider, never across.

- `--provider X` on the command line: that provider alone (its default, `runpod`, doesn't count —
  clap's value source tells them apart). **Shadeform** shows only this way: it's opt-in overflow,
  never in `up`'s default (with its key set, a `skipped:` line says so). One catalog read prices every option at its cheapest in-stock entry
  (allowed clouds, CUDA floor, `SHADEFORM_MAX_PRICE`); an option with nothing in stock is noted and
  left unpriced.
- **Filters** apply per provider exactly as `create` applies them. A flag that doesn't apply to a
  provider (`--gpu`/`--gpus` on Hetzner, `--server-type` elsewhere) leaves it out with a `skipped:`
  line — or, with that provider named, fails as `create` does. A provider whose planning fails (a
  `--gpu` typo, a server type Hetzner doesn't have or sells nowhere `HETZNER_LOCATION` allows) is
  listed as `failed:` while the others still show, and the command exits non-zero; named, it just
  fails. A read that doesn't answer (a catalog, a Vast search) is a note, never a failure, as in
  `create`'s own plan. Flags that apply to no provider in scope (`--server-type` with no Hetzner
  key) are an error: nothing to show. `--cloud` is RunPod's (it collapses
  elsewhere, with a note when several were asked for).
- **Hetzner's row** is the VM `up --provider hetzner` would create: `--server-type` over
  `HETZNER_SERVER_TYPE` (default `cx23`), its vCPUs and RAM, and its gross €/h where the create's
  location walk starts (one `GET /server_types`, plus the same nearest-first timing a create does;
  the walk is in a note). Placement itself quotes no price for it, so `--max-price` drops it ("no
  known price"), exactly as `create --provider hetzner --max-price …` would. Without
  `HETZNER_API_KEY`, or if the read fails, the row keeps the type's name, size and price unknown;
  a type the catalog says create would refuse is a failure, not a row.
- `--json`: `{"providers": [...], "skipped": [{provider, why}], "failed": [{provider, why}]}`. Each provider is its option plan as
  `create` sees it — `provider, gpu_count, order, max_price, options, dropped, notes`, the fields of
  the old single-provider `offers --json` — with `vram_gb` added to each option, and on Hetzner a
  `server_type` object (`name, cores, cpu_type, architecture, memory_gb, disk_gb, eur_per_hour,
  locations`). **Changed:** `offers --json` used to print one provider's plan as the top-level
  object; it's now under `providers[]` (`jq '.providers[] | select(.provider == "runpod")'` for the
  old shape).

**`--all`**: every type each provider sells instead of what `create` would try (it doesn't take
the option flags). RunPod: its GPU catalog, exactly as [`gpus`](#gpus) printed it. Hetzner: every
server type on sale somewhere now — vCPUs, RAM, disk, €/h in its cheapest location, where it's sold
— with `HETZNER_SERVER_TYPE`'s starred (or a line saying it's on sale nowhere now). Vast and Shadeform are marketplaces with no fixed list: a
note says to price GPUs with `offers --provider vast --gpu …`. A list that can't be read is a note;
RunPod's config refusing it (a bad `RUNPOD_API`) fails the command after the rest is shown.
`--all --json`: `{"runpod": [rows as gpus --json], "hetzner": [server types], "notes": [...]}`.

### `pods up`

`pods up -n N` — one-command spin-up: create, then **one independent pipeline per pod** (never a
batch), each started **the moment its create returns** — while placement may still be retrying for
other names under `--retry-mins` (placed pods used to sit idle, billing, until the window ended):
wait for *its* SSH endpoint and sshd answering (`--timeout`, default 600s, per pod; a pod up for 5
minutes with only RunPod's SSH proxy — a host with no public IP — is a host failure: `FAILED
endpoint`, or replaced under `--check`; so is one still up behind the proxy only when a shorter
`--timeout` ends, and a listing that momentarily lacks the pod doesn't restart those 5 minutes; on
Shadeform a pod listed `error`, or not up within `SHADEFORM_READY_MINS` or `--timeout` (whichever
comes first), is a failed placement — deleted at once and the name tried on the next option, with or
without `--check`, within `--check-attempts` placements) → sync the proxy (one writer at a time) →
provision it (`--no-setup` skips) → [`--check`: deep check] → copy its API keys (when
`keys/*_api_keys.csv` exist) → `[name] READY after 4m10s — …` or `[name] FAILED <stage>: …`, printed
the moment that pod is done — a slow pod never holds up another.

One fleet listing per `--interval` serves every pod's endpoint wait.

Ends with the usual one-line proxy sync and a `NAME GPU $/H PROXY PORT HEALTH STATUS READY AFTER`
table (READY AFTER = first create → confirmed ready, replacements included: start participants'
clocks at READY), plus a line per name that failed, warned or was replaced — a requested name that
got no pod is a `FAILED create` row; exits non-zero unless every requested name is READY (and when
nothing was created at all).

A pod that fails is left running — one is only ever terminated by `--check`.

Without a proxy layout, a READY pod's `~/.ssh/config` fleet map is rewritten at the end if pods came
up after it.

#### `pods up --check`

**`--check`** deep-checks each pod after setup (`pods test --deep`). A FAIL on the host (driver,
CUDA, device count, a GPU's tensor op, peer copies, NCCL) is a bad machine: the pod is terminated,
confirmed gone from its own provider's listing (never two pods per name — and no replacement if
another pod has taken the name meanwhile), and the name recreated from the same
`--gpu/--cloud/--max-price` options (or the one configured spec) — options that haven't failed
first, waiting for capacity per `--retry-mins` or `--keep-trying` — and run through the pipeline
again; a replacement that lands on a machine that already failed (any name's; by the provider's
machine id — RunPod's `machineId` from REST v1 or GraphQL — before the IP, which several RunPod
hosts share) is rejected unseen and terminated (even with no attempt left).

#### Bad machines are remembered for 72 h

A host FAIL on a machine the provider names (RunPod/Vast machine id — never an IP) is also
**remembered for 72 h** in the health cache — but only for definite, machine-level evidence: a
CUDA/driver error from torch while nvidia-smi lists the GPU, an nvidia-smi error or missing GPUs, a
GPU count mismatch between torch and nvidia-smi, a wrong result (NaN matmul, garbage peer copy).
Never a timeout (python's 75 s, nvidia-smi's 10 s, NCCL), python dying before it reported, a driver
below the floor, NCCL, or fewer GPUs than the provider's count where nvidia-smi agrees with torch or
can't count (one wrong provider field would ban machines fleet-wide): those still replace the pod in
that run, and are forgotten with it.

Any pod that lands on a remembered machine later — another name's first pod, or any pod of a later
`up` — is turned away the same way before setup (`landed on runpod machine …, which failed the deep
check 3h ago (<UTC time>, <pod>: cuda: …) — rejecting it`; the summary says so too); without
`--check` it is only a warning, nothing is terminated. Vast's creates skip such machines' offers
altogether; RunPod's create can't be told, so a pod RunPod places there again is just rejected and
retried (within `--check-attempts` — a second `--gpu`/`--cloud` option draws from another pool).

A machine is banned **as a whole**: a multi-GPU or MIG host loses all its slots, not just the GPU
that failed. It is forgotten after 72 h, at once when a later complete check on it passes (`up
--check`, `pods test --deep`), or ignored for one run with **`--retry-bad-hosts`** (new failures are
still remembered); `config check` lists the remembered machines. A cache that can't be read
remembers nothing (one warning), never fails `up`.

#### Attempts, and failures that aren't the host

Up to `--check-attempts N` placements per name (default 2); the last pod that FAILed its check is
left running for a look.

Only a check whose script ran can condemn a host: one that couldn't run (SSH dropped, timed out) is
rerun once SSH answers, and if it still can't run the name FAILs with the pod left running.

A FAIL that is only **software/setup** — as the facts show it: no python, python reporting that
`import torch` fails, a CPU-only torch (`+cpu`, no CUDA in it) on a GPU pod, the check script never
starting although SSH worked (no zsh to run it in on a bare VM before setup — `--no-setup` on
Hetzner — which says to run `arena pods setup`; no `base64`) — is never replaced — a new pod boots
the same image and runs the same setup, so it would fail the same way: the name FAILs at once
(`FAILED check — software/setup failure, not the host — …`), the pod is left running and its machine
isn't counted as bad (`pods test --deep` says the same under its table).

Python dying before it could report (killed by its 75 s timeout or the OOM killer, SIGILL on an old
CPU, a segfault, output cut off) is a host failure, replaced as before.

A WARN counts as ready (shown in HEALTH).

**Ctrl+C** stops every pipeline where it is, starts nothing new, terminates nothing, and reports —
also when pressed during the create's retry wait (the pipelines then start stopped).

So `pods up -n 28 --gpu A40 --cloud SECURE --disk 200 --retry-mins 60 --check` is a full
start-of-iteration spin-up. Confirms first (`--dry-run` previews the pipeline); `--no-wait` skips
everything after the create (not with `--check`).

## Proxy and SSH config

![Diagram comparing a manual ssh config with the proxy's stable ports](https://raw.githubusercontent.com/nickypro/arena-infra/refs/heads/main/management/arena-infra-diagram.png)

### How the proxy plans forwards

`proxy` — port-forwarding planner. Pods are reached over SSH (VS Code Remote-SSH), and the provider
reassigns a pod's SSH endpoint on restart, so the proxy host gives each machine a *stable* public
port (`cute.sus.cat:7000`, …) that nginx's `stream` module forwards straight to the pod's current
SSH endpoint — pure nginx, no tunnel process.

The public port is anchored to the machine's index in `MACHINE_NAME_LIST`, so tearing down one pod
never renumbers the others and a returning machine reclaims its port. Pure/no-I/O — it plans, you
apply.

The plan is a **sticky merge**: the previous forwards are parsed back from the config it rendered
last time (each block carries a `# arena-forward name=… port=… target=… provider=… pod_id=…` line;
the older `# <name>` format is read too) and merged with a **per-provider** listing
(`Provider::list_by_provider`).

A forward is only removed once its pod is confirmed gone — absent from a provider whose listing
*succeeded*; a pod listed without an endpoint, or one whose provider failed to list (e.g. a Vast
429), keeps its forward, and if no provider answers nothing is written. Provider list responses with
an unexpected shape are errors, never "zero pods".

### `proxy plan` / `proxy apply`

`proxy plan` — read-only; shows the merge against the current config (`+` added, `~` changed —
including a kept entry whose port moved with the list —, `-` removed, `=` kept-stale, plus a `+N
added, ~N changed, …` summary) and prints the nginx `stream` config (`--out` saves it locally; never
connects to the proxy).

Without the current config (remote proxy, unreadable file) it's labelled a **listing-only preview**
and `--out` is refused — deployed by hand it would drop every kept forward.

`proxy apply` prints the same changes, confirms, then **deploys** the config (locally, or over SSH
when `PROXY_LOCAL=false`) and runs `SSH_PROXY_RELOAD_CMD` — default `nginx -t && nginx -s reload`;
set it **empty** (in the file, or exported empty by a wrapper) for write-only (never reloads nginx).

Writes are serialized (`flock`), refuse to overwrite a config another run changed since it was read
(the merge is redone), and are atomic locally (temp + rename).

A **failed reload puts the previous config back**, so the next sync retries instead of reporting
"unchanged". `--dry-run` shows the write/upload + reload without doing it. This is the one place the
tool touches the proxy host.

### Auto proxy sync

**Auto proxy sync.** `pods create`, `up`, `rename`, `reimage`, `terminate` (one or `--all`),
`replace` and `migrate cutover/revert` end by re-syncing the proxy through the same merge, from a
fleet-wide listing, and print one line — e.g. `[proxy] after terminate: +0 ~0 -1 =0 (deployed;
removed arena8-apple)` or `… (unchanged)`.

It's best-effort: no proxy configured / no nginx to deploy to → a one-line skip note; a listing or
write error → a warning, never a failed command. `--skip-proxy` (create, terminate, rename, reimage,
replace, migrate) opts out.

A just-terminated pod can still be listed briefly, so its forward may survive that sync; the next
one removes it. A `create`/`up` that fails part-way still syncs for the pods it made.

A partly failed `rename` batch doesn't sync: with `--from-prefix`, re-run the rename *before* any
`proxy apply`/lifecycle command/proxy cron tick, which would drop the not-yet-renamed pods'
forwards.

`migrate cutover` still treats a sync that didn't land (or didn't route the name to the new pod) as
a failure and auto-reverts — except on a **write-only** proxy (`SSH_PROXY_RELOAD_CMD=""`), where
nothing can be verified through nginx: said up front, the forward written, no 45 s probe, no revert;
`replace` only terminates the old pod once the sync routed the name to the new one (else it keeps it
and says why), and **`migrate finish`** now syncs first and refuses to terminate `<name>-old` until
`<name>`'s port routes to the live pod.

Provider list calls are bounded (60s), so a stalled API reads as "failed to list" (forwards kept),
not a hang.

### `ssh-config`

`ssh-config [--proxy] [--out]` — emit the **participant-facing `~/.ssh/config`**: direct pod
endpoints by default, or stable proxy ports (`--proxy`) anchored to each machine's
`MACHINE_NAME_LIST` index. Read-only.

#### Keeping your own `~/.ssh/config` current (`SSH_CONFIG_FILE`)

For one person reaching their own pods: `ssh-config --install [PATH]` (once) and
`SSH_CONFIG_FILE=~/.ssh/arena.conf` in the config. On a box that runs arena's cron lines for a
cohort (backups, the reaper), leave the key unset: each tick would rewrite that user's file.

- **`--install`** writes PATH (default `SSH_CONFIG_FILE`, else `~/.ssh/arena.conf`; `~/` is your
  home) and makes `Include PATH` the **first line** of `~/.ssh/config` — ssh applies an `Include`
  only where it stands, so it must come before any `Host`/`Match` block to count for every host.
  It says what it changes and asks (`--yes` to skip, `--dry-run` to preview), keeps a copy of the
  old `~/.ssh/config` beside it (`config.arena-bak-<UTC time>`, never over an earlier copy), keeps
  that file's mode (a new one is 0600), and prints the `config set` line when `SSH_CONFIG_FILE`
  isn't set yet. A second run changes nothing. An `Include` already inside a `Host`/`Match` block
  is left for you to move. A file at PATH that isn't arena's is replaced only after asking, with a
  copy kept. It needs a complete listing (a provider that failed to list would drop its pods).
- **What it won't edit** (checked before writing anything, and again after asking): a
  `~/.ssh/config` or PATH that is a symlink (edit the file it points to yourself) or isn't a
  regular file; one owned by another user, or in a `~/.ssh` that is (run it as the user whose
  `~/.ssh` it is — not under `sudo`); PATH being `~/.ssh/config` itself; a PATH with whitespace,
  quotes or any of `#$%*?[]\` (ssh's `Include` would read it as something else); a file edited
  while it asked.
- **`--uninstall [PATH]`** takes exactly that `Include` line out — the global one(s); one inside a
  `Host`/`Match` block is yours and stays — with a copy kept; PATH stays.
- **With `SSH_CONFIG_FILE` set**, PATH is kept current: after every pod change (create, up, start,
  restart, reimage, replace, migrate, rename, terminate and the reaper — beside their proxy sync,
  one `[ssh-config] after …: rewrote …` line; `--skip-proxy` skips it too), and on `pods list` and
  each dashboard refresh when an endpoint moved. `up` writes it once its pods are up; `create`,
  which doesn't wait, mostly can't yet — the next `pods list` or refresh adds their endpoints. The layout is the proxy's stable ports when `SSH_PROXY_HOST` is set (no
  listing needed), else each pod's own endpoint (a pod without one is left out, as in
  `ssh-config`). It is written atomically (a temp file renamed over it), owner-only, and only when
  the text changed.
- **Never someone else's file:** arena's file starts with `# managed by arena (`arena
  ssh-config`) — …`; one without that line (an older `ssh-config --out`, a hand-made file), a
  symlink or a folder is never rewritten. A listing that failed somewhere doesn't rewrite it
  either. Each of those, and a failed write, is one warning line — never a failed command.
- The backup archive's `Host` block isn't in it: that one needs its pinned host-key file, which
  `ssh-config --out` writes beside the config. `config check` shows the key's state: off, not
  written yet, not arena's, or arena's and whether `~/.ssh/config` includes it.

### Publishing the fleet page

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

## Setting pods up

### `pods setup`

`pods setup` — provision pods over SSH (ordered steps in `--help`): copy the git deploy key, write
`~/.ssh/config` + `authorized_keys`, **put the repo on the persistent volume** when the pod has one,
point the repo at GitHub, update submodules, write `~/.name`, and export any set **broadcast
tokens** (Hugging Face for gated-repo access, Claude Code; via config or `--hf-token`/`--cc-token`)
— else those steps are skipped and it says so.

It also **auto-distributes per-host API keys** if any `keys/*_api_keys.csv` exist (reporting what it
added, or that none are set up) — to exactly the pods that just provisioned successfully, never one
that failed or timed out — so a `setup` (or `up`, per pod) makes pods fully ready.

A pod that refuses our key (`Permission denied (publickey)`) on a provider with a key API (Vast)
gets the cohort keys re-attached to it and is set up again (up to 3×, 20s apart); elsewhere the
refusal is reported as before.

Confirms first (`--dry-run` previews, token redacted). Uses `GIT_SSH_KEY_LOCAL/REMOTE`,
`ARENA_REPO_OWNER/NAME`, `DEFAULT_BRANCH`.

#### The repo: `origin`, `upstream` and the update

`origin` is pointed at the cohort repo (where backups push); the public course repo
(`ARENA_UPSTREAM_REPO`, default `ARENA-education/ARENA_materials`, `none` to skip) is added as the
checkout's `upstream` remote over https when it has none — so a participant can `git pull upstream
main` the course's own fixes. Setup never fetches it, and an `upstream` already there (a
participant's own) is left alone; `checkout.defaultRemote=origin` makes a plain `git checkout <branch>`
both repos have take origin's. arena's own fetches name origin (init-branches,
set-branch), so an unreachable upstream never stops them.

The repo update fetches **only the default branch, without tags** (a bare `git fetch` would pull
every participant's autocommit branch); a tracked non-default branch pulls just its own upstream;
the default branch is fast-forwarded only when the checkout has no tracked change and no commit of
its own — otherwise it's left as it is with a warning (`--force` checks out the default branch and
resets it).

#### Time budgets

Pods run in parallel and **every step has a time budget** — copies 60s, the image config step 300s,
the hetzner bare-VM script 1800s; `--timeout <secs>` (or config `SETUP_TIMEOUT_SECS`, 1..86400;
`up`/`replace`/`migrate copy` use the config value and reject a bad one — and `up` missing
`ARENA_REPO_*` unless `--no-setup`, or with `--check` a bad `MIN_DRIVER_VERSION` — *before* creating
anything) overrides the main-step budget.

A wedged pod prints `✗ <name> (timed out at <step> after Ns)` and the others finish normally; a
timed-out `ssh`/`scp` is stopped (SIGTERM, so scp also stops its ssh transport; SIGKILL 2s later).
Connection refusals right after create are still retried for ~150s (sshd booting); an auth failure
(`Permission denied (publickey)`) fails at once.

#### Hetzner bare VMs

**Hetzner (bare VM):** the embedded `hetzner_setup.sh` builds the ARENA env with uv on **Python
3.13** (the repo's `requires-python` lower bound when newer; `ARENA_PYTHON` overrides) with CPU
substitutions by pattern, not by exact upstream line (`jax[cuda…]` → `jax[cpu]`, bitsandbytes and
`nvidia-*cuNN` wheels dropped, CUDA indexes and `+cuNNN` pins dropped, `uv pip install
--torch-backend cpu`).

It **fails loudly**: any failed package/venv/install step, or a venv whose python can't `import
torch, transformer_lens` afterwards, exits non-zero with the reason as its last line (`FAILED: …`) —
a VM is never reported set up without a working env. Re-runs skip the install only when that import
gate already passes. Its installers' scratch (uv's above all) goes to a temp dir of its own, removed
when it ends, failure or not: uv's 0-byte `uv-setuptools-*.lock` files, left in `/tmp`, kept a fresh
VM from ever reading as idle to the auto-reaper.

#### VS Code warm-up

**VS Code warm-up** (last step, on every provider, `up`/`restart`/`replace` included): pre-installs
what a participant's first Remote-SSH connect would otherwise download on the pod — the latest
stable VS Code server for the pod's CPU (x64/arm64, resolved from the update API on the pod;
checksum-verified) in the layout Remote-SSH looks for (`~/.vscode-server/code-<commit>` +
`cli/servers/Stable-<commit>/server`, plus the legacy `bin/<commit>`), the extensions
(`VSCODE_EXTENSIONS`, default Python + Pylance + Jupyter; shared by every server version, so they
help even when a client is another release), and the conda env as `python.defaultInterpreterPath` in
the machine settings (added only if unset; other keys kept). Skips whatever is already there; its
own 300s budget; **best-effort** — a failure or timeout prints a warning (`✓ name (warning: vscode
warm-up: …)`) and the pod still counts as set up. `--no-vscode` skips it for a run,
`VSCODE_PREINSTALL=0` everywhere. (`~/.vscode-server` is a dot-dir, so `pods pull` and replace's
home copy already skip it.)

#### The repo on the `/workspace` volume

**Repo on the volume** (image-based pods; its own best-effort step `repo onto volume` before the
config step, 900s budget, never a failed setup; `REPO_ON_VOLUME=0` turns it off): when `/workspace`
is a real mount (`mountpoint`, else `/proc/self/mountinfo`) the repo lives at `/workspace/<repo dir
name>` and `BACKUP_REPO_PATH` becomes a symlink to it, so a restart or stop keeps the participants'
work and every git operation (setup's fetch/reset, `backup`, `set-branch`) acts on the volume copy.

A fresh volume: refused while the repo is in use (a process's working directory **or an open file**
in it) or either disk lacks the room; the checkout is copied there (complete before it counts, the
copy stopped at 360s), refused if anything in it changed meanwhile, then the original is moved aside
to `<repo>.arena-aside-<UTC time>` on the container disk (a reset clears it; on an overlay root `mv`
may have to copy it — one more checkout's time and space) and only then is the copy put in place and
linked; any failure puts the checkout back.

After a reset (the volume kept a copy): the **volume copy wins** — but only over the image's
**untouched** checkout (`git status` clean and nothing in it changed since the container was
created, `/.dockerenv`'s time): it's moved aside, never deleted, the volume copy never overwritten,
and the link recreated (the restart flow's re-setup does this).

If participants already worked in the checkout (a provider restart, a stop + start, before setup
ran), **neither is touched** and the warning says to copy their changes into the volume copy, move
the checkout away and re-run setup — moving it aside would hide that work where no pull looks.

Idempotent, under a lock (a setup whose relocation timed out may still be copying on the pod: the
next waits 120s, and the config step waits for it before the repo update). Without a volume nothing
changes.

Anything odd — `/workspace/<repo>` that isn't a git checkout, the path linking elsewhere, the copy
failing — leaves everything as it is and shows as `✓ name (warning: repo onto volume: …)` (the TUI's
setup line too); the restart gate then stays closed. A pod restarted by its provider (or a stopped
pod started again) shows the image's checkout until `pods setup` re-links it.

**Rolling this out to a live fleet:** the first setup copies each repo (and, on an overlay root, a
second time to keep the original) — run it while participants are idle; a save that lands mid-move
is caught and the move undone, but only between checks.

### `pods copy-keys`

`pods copy-keys` — distribute API keys into each pod's `~/.bashrc`/`~/.zshrc` (idempotent): per-host
keys from `<keys-dir>/<provider>_api_keys.csv` (openai/anthropic/openrouter) **plus broadcast
tokens** — a Hugging Face token (`--hf-token`/`HF_TOKEN`, sets `HF_TOKEN` +
`HUGGING_FACE_HUB_TOKEN`) for **gated repos** (Llama 3, …) and a **Claude Code token**
(`--cc-token`/`CLAUDE_CODE_OAUTH_TOKEN`).

Targets / `--exclude` scope it to specific pods. Confirms first; `--dry-run` lists what would be set
(values redacted). Both broadcast tokens are env-introducible (e.g. `CLAUDE_CODE_OAUTH_TOKEN=… arena
pods copy-keys`). Each pod's write has a 60s budget, so a wedged pod reports `✗ <name>: … timed out`
instead of hanging the command.

### `pods cp` (`pods copy`)

`pods copy <file> [dest]` — scp a local file to every pod (concurrent; `-t <targets>`/ `--exclude`
to scope). With no `dest` it **mirrors the path under the ARENA repo** (a local
`…/ARENA_3.0/foo/bar.py` → `/root/ARENA_3.0/foo/bar.py`); otherwise `dest` is the remote path
(trailing `/` = into that dir). Creates the remote parent dir and **verifies the file landed** (size
check; flags a silent scp non-write or a misplacement when the dest is actually a directory) rather
than trusting scp's exit code.

Per pod it runs mkdir → scp → check and stops at the first failure; the scp's budget scales with
what's sent — 10 min plus 1s per MB × pods (all copies share your uplink; `-r` counts the tree), or
`--timeout <secs>` — and the mkdir/check get 20s each. Confirms first (the preview shows the
budget); `--dry-run` previews.

### `keys` (OpenRouter)

`keys gen|list|rotate|revoke` — manage **OpenRouter** runtime keys via the provisioning API (needs
`OPENROUTER_PROVISIONING_KEY`). `gen <targets|--all>` mints one key per machine (named
`<prefix>-<machine>`) with a USD cap (`--limit`, default `OPENROUTER_KEY_LIMIT` or $5) and writes
`keys/openrouter_api_keys.csv`; `--copy` also pushes them out via `copy-keys` (targets may be
MACHINE_NAME_LIST names with no pod yet; `--all` = every current pod).

`rotate <targets|--all>` deletes + re-mints (leak recovery), `revoke` deletes only, `list` shows
**this iteration's** `<prefix>-*` keys — each one's cap (`/d`, `/w`, `/m` when it starts over daily,
weekly or monthly), what it spent in the cap's period, today, this month (UTC) and all told, and a
total row — noting how many others were hidden (`--all` shows every key in the workspace, disabled
ones included — the listing walks every page).

**Workspaces.** OpenRouter's provisioning ("management") key is the whole account's — in an
organization, every workspace's. Without `OPENROUTER_WORKSPACE`, keys are listed and created in the
Default workspace, as before. Set `OPENROUTER_WORKSPACE` to a workspace's id or slug and every call is
scoped to it: the listing asks for that workspace and drops keys that say otherwise, new keys go
there (a key that landed elsewhere fails the command, which names its hash to delete by hand —
nothing is deleted automatically), and a revoke,
rotate or rename first reads the key and refuses one from another workspace. The workspace is looked
up once (`GET /workspaces/<id or slug>`) and must come back as itself; one that can't be resolved, or
a listing whose keys don't say their workspace, fails the command — it never falls back to the
Default one.
`config check` crosses a value that's neither an id nor a slug.

**In the dashboard.** With a provisioning key set, the TUI reads the keys and the account's credit
on its details cadence, in the background (once a minute, `r` sooner; the listing within 20 s and the
credit within 5 s, each keeping its last answer if its read fails — the `AI$` cells go grey then):
an `AI$` column (each pod's key: spent/cap, yellow from 80%, red at the cap or disabled, `-` no key —
shown from 122 columns), an `ai key:` line in the detail pane (the cap, what's left, today, week,
month, all told), and the summary bar's `openrouter $422.09 left · AI today $3.20`. It leads the bar
in yellow when the credit is less than the enabled keys' caps could still spend, or a key is at its
cap, and in red when the credit is gone. The credit is the account's (an organization's whole),
whatever the workspace.

Keys are found by name, so no local hash bookkeeping; a machine that left the list but still has a
key (or a CSV row) can still be targeted by name. Dry-runs show each key name and whether it exists.
`keys which` shows the local keys file. (`copy-keys` is the *distributor*; `keys` is the
*generator*.)

## Running commands and checking health

### `pods run` / `pods test`

`pods run [-t <targets>] [--timeout <secs>] <cmd>` / `pods test [targets]` — run an arbitrary
command on every (selected) pod (concurrent, confirms first naming the pods; each pod gets
`--timeout`, default 1800s = 30 min — flags go *before* the command: a bare
`-t`/`--exclude`/`--on`/`--gpus`/`--dry-run` after it is refused rather than run on every pod, so
quote a command that takes one, `run 'tmux kill-session -t lab'`; on a timeout the local ssh is
killed, and the remote command dies at its next write to the closed connection; a pod whose command
fails shows what it printed on stdout too, then `exit N: <stderr>` — never `Some(N)`; on a pod with
no zsh yet — a bare VM before setup — it says so and to run `arena pods setup`) / the read-only
torch-version health check (90s per pod; with no targets this cohort's pods, as `--deep`). Every dry run's `ssh`/`scp`/`rsync` line is shell-quoted,
so it pastes as exactly what would run (globs like `'**/.git/**'` don't expand) where the login
shell is a POSIX one; the remote command is shown as written (see below).

**Login shells: POSIX ones and fish.** sshd hands a remote command to the user's login shell, and
arena's commands are written for bash, zsh and sh. fish reads them its own way: an `if …; then`,
`$?` or `${x}` doesn't parse, and a backslash in single quotes can change the command with no error.
So every command arena runs over ssh (`pods run`, setup, the dashboard's probe, deep checks,
backups…) goes as one line that both kinds of shell parse, each running only its own part
(`ssh::for_any_login_shell`). bash, zsh, sh, ksh and ash run the command themselves, through
`eval`: the same startup (once), dialect, `$0` and exit status as before. fish `exec`s it under
bash, else `/bin/sh`. Other login shells (csh, nu) aren't supported (before, they ran plain-word
commands only). A command too big
to encode within one argument (128 KiB) goes bare: POSIX shells run it as before, and fish
refuses it rather than misread it. Interactive ssh (`c` in the dashboard) is untouched, and
`rsync` and `scp` use their own transports.

### Background jobs: `run --background`, `jobs`, `logs`

`pods run --background [-t <targets>] <cmd>` / `pods jobs [targets] [JOB] [--kill JOB]` / `pods logs
[targets] [JOB] [-n N] [-f]` — **detached course-test runs**. `--background` (confirms first;
`--dry-run` shows the wrapper) starts the command on each pod with `setsid -f nohup` (not `&`, which
would start it with SIGINT/SIGQUIT ignored — no `KeyboardInterrupt`; only a `setsid` without `-f`
falls back to that), under `pods run`'s shell + conda env (plus `PYTHONUNBUFFERED=1`), and returns
at once: `[n/N] ✓ <pod>: job <id> (pid …)`. A job that can't record its pid doesn't run (`exit
125`); no pid within 5s is reported as "may have started anyway — check `arena pods jobs` before
retrying", never as a clean failure.

One id per run, e.g. `20261008-142301-pytest-x-k--3fa9c1`: the UTC start, a slug of the command's
shape — its program, script and flag names, never a value, as `pods ask` sends commands (`curl -u
bob:pw …` makes `…-curl-u--…`; a command whose shape is `<command>` gives no slug at all,
`20261008-142301--3fa9c1`) — and six random hex digits, so two runs whose commands differ only in
a value never share one. Ids made before keep their names and work as ever. Everything stays on
the pod under `~/.arena/jobs/<id>/` (`cmd`, `run`, `log` = stdout+stderr, `pid`, `started_at`,
`exit`), so it survives your SSH session and any operator can look — but not a pod restart
(container disk), and logs are never rotated or pruned. No time limit; `--timeout` is refused with
it.

`pods jobs`: a table of each pod's jobs, newest first (`running (pid N)` — the pid must still be
that job's wrapper, not a recycled one — / `exit N` / `lost` = ended without an exit code, or still
no pid after a minute; `starting` before that).

`pods logs`: per pod, the job's status and its last `-n` lines (default 20, of at most the last 1
MiB; JOB defaults to each pod's newest; a JOB-shaped word among the targets is the job). `-f`
re-reads every 3s from where it left off (exact byte offsets, lines printed once, `[pod]`-prefixed
with several pods) until every job has ended, then exits non-zero unless all were `exit 0`; a pod
that fails 5 reads in a row is given up on; Ctrl+C stops following, never a job.

`pods jobs --kill JOB` (confirms) sends SIGTERM to every process group of the job's session where it
is running — the wrapper's, and any a command made of its own (`timeout …` does) — (→ `exit 143
(SIGTERM)`); no SIGKILL follow-up.

Log text is stripped of escape sequences/control characters before printing. Calls are bounded (30s
per pod; the start may have happened if it timed out — the report says so).

### `pods test --deep`

`pods test --deep [targets] [--json] [-v]` — the **is-this-pod-usable** check (read-only), for what
a plain `import torch` misses on a bad host. One embedded script per pod (one SSH exec, 150s budget,
inside the conda env) measures: nvidia-smi GPUs + driver + CUDA version; torch import,
`cuda.is_available()`, device count vs nvidia-smi; a small tensor op on **every GPU** (catches
`cuInit` 999 / `CUDA error: unknown error`); with >1 GPU a GPU→GPU copy that must arrive intact and
an NCCL `all_reduce` across the GPUs (else `skipped (1 GPU)`); a 32 MiB Hugging Face download (no
token sent); free disk on `/` and `/workspace`; host load + uptime; what setup's VS Code warm-up
left (newest server, extensions, default interpreter — an informational `vscode` line, Pass or Skip,
never WARN/FAIL). The provider's maintenance window comes from the API.

**FAIL**: any CUDA/tensor/copy/NCCL error, count mismatch (torch's count against nvidia-smi's, and
against the GPUs the provider reports for the pod: fewer fails), missing torch/nvidia-smi, driver
below the floor, unreachable/timed out.

#### MIG slices

The torch fallback is for **MIG slices only**: where a slice's nvidia-smi can't answer (missing, an
error such as NVML's `Insufficient Permissions`, a timeout, no GPUs listed), torch's own checks
stand in (its count, a tensor op on each, the memory it reports) and the slice PASSes if they all
pass. Anywhere else a failing nvidia-smi FAILs whatever torch says: on whole cards that is how a GPU
fallen off the bus looks, and torch sees only the survivors.

A slice somewhere isn't enough: every GPU of the pod must be shown to be one — the provider's GPU
count, when known, no more than the slices `nvidia-smi -L` lists (else those torch names `… MIG …`),
nor than torch sees where nvidia-smi can't count, and torch naming no whole card beside a slice.
Else (a mixed pod that lost its whole card, its slice still there) it FAILs as whole GPUs do.

With no driver version from nvidia-smi, the driver comes from `/proc/driver/nvidia/version` or,
failing that, the CUDA version the driver supports (libcuda's, else torch's) against the CUDA floor
(the CUDA column marks it `12.8*`).

Under MIG, torch's count is matched with the slices `nvidia-smi -L` lists and the provider's count:
the same PASSes, more FAILs, fewer **WARNs** — `torch sees 1 of the 2 MIG slices — CUDA may show a
process one slice; check each with CUDA_VISIBLE_DEVICES=<MIG uuid>`, as the other slice may be
inaccessible or broken — where every GPU is shown to be a slice (as above), else fewer than the
provider's count FAILs.

A machine with whole cards beside its slices is counted as whole GPUs when `--query-gpu` lists more
cards than there are slices; one whose whole cards don't outnumber its slices can't be told from a
pure MIG one, and isn't supported.

#### Warnings and the driver floor

**WARN**: download < 2 MB/s or unreachable, < 10 GB free, host load above max(32, host CPUs), a
maintenance window, a MIG undercount.

Driver floor: `MIN_DRIVER_VERSION` (`none` = off), else derived from the create's CUDA floor —
`MIN_CUDA_VERSION`, else the lowest of `ALLOWED_CUDA_VERSIONS` / `EXACT_CUDA_VERSIONS` (13.x → 580,
12.8 → 570, 12.4 → 550, …; one that isn't a version is an error, as a malformed `MIN_DRIVER_VERSION`
is) — else no driver check.

Hetzner CPU VMs skip the GPU checks (decided by provider only: a RunPod/Vast pod the API reports
with 0 GPUs still gets them, plus a `provider` warning).

#### Output

Output: a `NAME RESULT GPUS DRIVER CUDA NET NOTES` table, `-v` lists every check, and a `same host?`
line when ≥2 failing pods share a machine (a bad host breaks every pod on it): its IP, or on Vast
its `machine_id` — never a Vast IP, which several machines can share; `up --check` rejects a
replacement on a failed machine by the same rule).

`--json` prints per-pod `{id, name, provider, status, checks, facts}` (no IPs) — `[]` when no pod
could be checked; summary/notes go to stderr. Exits non-zero if any pod FAILs; warnings don't.
Targets scope the run (a named pod with no SSH endpoint is a FAIL; a typo is an error). With no
targets (or `--all`) it checks **this cohort's pods**: `<prefix>-…`, never an `@` staff entry or
another prefix's pod — a staff box may be running someone's job — with one `⊘ skipped N non-cohort
pods …` line for the rest; `--include-non-cohort` checks every pod, and a named staff box is
checked. Two pods sharing a name each get their own row (with a warning naming their ids).

Every morning on cron: `arena cron install --health 07:20` (this box's time zone; log
`~/.local/state/arena-cron/health.log`; see
[`cron`](BACKUPS.md#cron-the-lines-that-run-it-all)).

#### The health cache

Each verdict (and `up --check`'s) is also kept in the **health cache** for `snapshot`:
`$ARENA_STATE_DIR/<prefix>/health.json`, else
`${XDG_STATE_HOME:-~/.local/state}/arena/<prefix>/health.json` — per `MACHINE_NAME_PREFIX`, so the
sandbox and prod never share one; owner-only, written atomically under a lock, keyed by provider +
pod id; records of pods their provider listed OK without are dropped. A corrupt file is one warning
and gets replaced.

It also holds `up --check`'s **bad machines** (`bad_hosts`: machine id → when, pod, failed checks),
dropped 72 h after their failure or when a complete check on the machine passes, never by a listing;
`pods test --deep` adds none (its pods are in use, and a participant's own job can fail a check),
but its passes clear them.

### `pods ask`

`pods ask [targets] [--all] [--ai claude|codex] [--model M] [--effort E] [--timeout S] [--dry-run] ["question"]`
— ask claude (Claude Code) or codex what state a pod is in, or several pods compared, and whether
anything is wrong or needs attention: **read-only** on the pods (one SSH call each), every pod in
one prompt, masked, the AI with no tools; the answer printed and saved with the prompt in the state
dir. The answer is brief (at most about 12 lines): per pod one line of state, then a bullet per
problem, each with the `arena` command that fixes or investigates it; `unknown` where the data
doesn't say. The dashboard's `?` asks the same ([Asking an AI about pods](#asking-an-ai-about-pods));
[`ASK_AI`](#ask_ai) picks the AI or turns it off, and sets each one's model and effort.

```bash
arena pods ask apple                            # the default question about one pod
arena pods ask apple "why is its GPU idle?"     # your own (quote it: the word with a space or ? is the question)
arena pods ask apple..delta -q "who is behind on backups?"
arena pods ask --all                            # a fleet overview: this cohort's pods, one prompt
arena pods ask --all --include-non-cohort --ai codex   # every pod, asked of codex (default: ASK_AI)
arena pods ask apple --model opus --timeout 600
arena pods ask apple --ai codex --model gpt-6-luna --effort low   # a quick answer: ~5 s for the AI
arena pods ask apple --dry-run                  # print the exact prompt and the AI's command line; send nothing
```

Name pods or pass `--all` (this cohort's; `--include-non-cohort` for every pod; `--on`, `--exclude`,
`--gpus` as everywhere). The answer goes to stdout, progress and notes to stderr.

#### What it reads

Read-only, one SSH call per pod (60 s budget; on the pod each `nvidia-smi` within 10 s, the
metrics probe's repo part as a whole within 15 s, each git call of the small probe's within 10 s,
and its `du`, `df`, `ps`, `who` and `tmux` bounded too — each KILLed 2 s after its TERM — so a
wedged GPU or a hung volume costs only its own facts, never the sections after them). It reads: the dashboard's metrics probe (GPUs and their
processes, CPU, memory, the root disk, the repo's branch, origin, commits not pushed, recent commit
subjects, whether `~/.name`, the deploy key and the API/HF/Claude Code tokens are set up — never
`PROGRESS_CMD`, your own command, which may write), `pods jobs`' listing and the newest job's last
40 log lines, and a small probe: the pod's clock and uptime, load, `/`, `/workspace` and the home's
size, logins (user, tty, time — in UTC, whatever the pod's zone — and whether each is an SSH
session, a tmux pane or local — **not** where from), tmux sessions, the busiest processes by CPU and
memory (each command's shape only: below), the repo's working tree (counts, the first 20 changed file **names**,
stashes, the last commit, and an operation in progress by the backup's own test — a merge,
cherry-pick, revert, `git am`, rebase, sequence or a stash pop's conflicts — read as what it means:
the backup is BUSY, how the participant ends it, and whether `pods backup` finishes it after
`BUSY_AUTO_RESOLVE_HOURS`). With it, from this machine: the pod's listing row (status, GPU, $/h,
endpoint, age, lock, maintenance), its last `pods test --deep` verdict, which local file backups
(`pods pull`) exist of it, the backup's record of its last whole copy while BUSY
(`busy-copies.json`: the operation, how long ago, the snapshot label and whether that copy is still
here — never the local path), and the branch today's backups go to. A stopped or unreachable pod is
its listing and why. What couldn't be read — a git that timed out, a list that failed, the health
cache or the backup dir unreadable here — is sent as `unknown`, never as none or 0. A checkout is
"not a git checkout", and an upstream none, only when git says so in its own words (`not a git
repository`; `no upstream configured`, `no such branch`, `HEAD does not point to a branch`): any
other fatal error of git's — a checkout it refuses as another user's (dubious ownership), one it
can't read — is unknown.

Its git — the dashboard's probe's too — writes nothing and runs nothing the participant's repo
configures: no optional locks (not even the index is refreshed), no fsmonitor hook, no clean/process
filter, no `gpg.program`, no submodule's own `git status`; the working tree is read from plumbing
that doesn't refresh, so a file only touched counts as modified until git next refreshes the index.

**Never read or sent**: file contents, shell history, the environment, `~/.arena` beyond the jobs'
status and the newest log tail, where a login comes from, arena's config or keys.

#### Masked before anything leaves

**Commands: never their values.** A process's arguments and a job's command are sent as their shape
alone (`arena_core::shape`): the program's name (`python3`, `jupyter-lab`), the first script or
notebook it runs (`train.py`, `part1.ipynb`, or python's `-m pytest`), each flag's name (`--lr`,
`-p`, `--token=…`), and `…` for every value — `python3 /w/train.py --lr 0.1 --token X` is `python3
train.py --lr … --token …`, `sshpass -p pw ssh host` is `sshpass -p … …`. A shell's or python's `-c`
code is `bash -c …`, a path under the jobs dir `<job>`, and a name that looks like a key (a long run
of letters and digits) a value too. The words are the shell's (quotes and backslashes taken out, a
quoted value with blanks one word), every leading `NAME=value` is one `…` before the program
(`APP_CONFIG='{"password": "x y"}' python train.py` is `… python train.py`), a program's, script's
or module's name shows only as 1–40 of `[A-Za-z0-9._+-]` (else `…`), and a command whose quoting
doesn't close is `<command>` whole — so is one with `$(`, a backtick, `${`, `<(`, `>(` or `<<`
anywhere but in a `-c` script (`X="$(printf "…")" python train.py`: where its words end can't be
told). Only a value spelled exactly like a flag's name (`-x`, `--fast`)
can still show: no shape can tell it without knowing every program's options.

Every other text field is masked through the backup's own key patterns
(`arena_core::backup::SECRET_PATTERNS`: OpenRouter, Anthropic incl. Claude Code tokens, OpenAI,
Hugging Face, GitHub, Google, Groq, AWS — the same replacements, `REDACTED-…`). Also masked: the
whole value of anything named like a secret (`HF_TOKEN=…` and `MY_SECRET='x y'`, `--api-key …`,
`--pass`/`--passphrase …`, `DB_PASS`, `MYSQL_PWD`, JSON/YAML/Python-dict fields such as `"password":
"a b"` or `api_key: …`, `password:x` glued, `Authorization: …`, `Bearer …`, basic auth: `curl -u
user:pass`, `--user user:pass`, `https://user:pass@…`) and a command's own password option
(`redis-cli -a …`, `sshpass -p …`, mysql's `-p…`, glued or not) — a line whose secret value can't be
delimited (an unclosed quote or bracket) or whose quoting can't be told (`Couldn't … PASSWORD='a
b'`) is replaced whole, and in a log so is every line such a value runs on into (a YAML block
scalar, a value on the line after its name, a quote or bracket closed lines later) — any run of 32+
`[A-Za-z0-9_-]` in commit subjects and log lines (`<redacted>`), any run of 40+ of `[A-Za-z0-9+/=]`
with upper case, lower case and digits (base64 key material), and a private-key block — in a log
masked over the whole read (the log's last 64 KB) before its last 40 lines are taken, so neither cut
can keep a key's body without its header, nor a value whose name is above the cut. Where the read
starts part-way into the log, a value whose name it never saw may run on at its top over any lines,
so nothing is shown from its first 16 KB: what's shown begins where that tracking has settled (a
log that fits in the read is tracked from its first byte). One case is left: a multi-line value
that starts more than 16 KiB above the shown lines and contains a short plain password line could
show that line — logs aren't a place for secrets. At the start of what's shown, lines that
look like base64 key material (40+ of `[A-Za-z0-9+/=]`, nothing else) are dropped. The AI CLI's
own error line, printed and saved when it fails, is masked the same way. Each pod's part is at most
6 KB (less when many pods share a prompt: 240 KB in all), trimmed from the log's oldest lines first.

**Framing a pod can't forge.** Each pod's reply is framed by marker lines carrying a random nonce of
that call's own, and so is every line of the small probe's: pod text that runs over lines (a path,
a git config value) can't make one of them. A reply with one of the call's markers twice or out of
order reads `unknown: the probe's reply was malformed`, and a fact the probe gives twice is
`unknown`. In the prompt, the pod data sits between delimiters with a tag random per prompt, and no
pod string can spell one (a run of `=` is cut to three).

#### Where it goes

claude sends the prompt to **Anthropic**, codex to **OpenAI**, through your own `claude`/`codex`
login on this machine (a one-line note says so on first use). Both get **no tools** — the prompt
says the pod data is data, never instructions, and they can't act on it anyway: both run as
`scripts/ai-review` runs them, claude as `claude -p --safe-mode --tools '' --strict-mcp-config
--mcp-config '{"mcpServers":{}}' --disable-slash-commands --no-session-persistence --output-format
text`, codex as `codex exec -s read-only --skip-git-repo-check --ephemeral --ignore-user-config
--ignore-rules` with `mcp_servers` emptied, web search off and every tool feature disabled (shell,
unified exec, code mode, apps, plugins, browser and computer use, sub-agents, image generation and
viewing, hooks) — and so is every web search feature, sleep tool, skill search, tool suggestion,
goal or worktree feature it lists. **It fails closed**: neither is sent anything until `claude
--help` lists every one of those flags, or `codex features list` names every one of those features
and then, run again with the ask's own overrides (and an empty `CODEX_HOME`, as the ask reads no
`config.toml`), names each one it turns off and shows nothing on beyond a reviewed list of features
known to give it no tool (`arena_core::ask::ai::CODEX_SAFE_FEATURES`, from codex 0.163). Both
listings are read strictly — every row `<name> <stage> true|false`, the state exactly `true` or
`false`, a stage codex is known to print — so a changed format can't read as "off". A check that
fails, times out, can't be read whole, or finds one missing or a feature on it doesn't know refuses
the ask (`codex: couldn't confirm its
tools are off — not asked (…)`, naming it). The prompt goes on stdin (never argv), in an empty
private temporary directory, with a minimal environment (`PATH`, `HOME`, the locale, XDG and proxy
variables, and the AI's own config dir and API key — not the rest of yours), and in a session of its
own: no controlling terminal, so it can't draw on the dashboard or read your keys through
`/dev/tty`. `--timeout` (default 300 s), Ctrl-C, a hangup or a TERM (to `pods ask`: handled as
Ctrl-C; to the dashboard: it ends at once, exit status 129 or 143), the dashboard's terminal hanging
up, or closing the dashboard stops it and everything it started —
TERM, then KILL 2 s later for whatever ignores it — and so does its own exit: what it leaves behind
goes the same way. Should arena be killed outright (SIGKILL, a crash), the kernel TERMs the AI CLI
itself (Linux: `PR_SET_PDEATHSIG`), though what that started is then the CLI's own to stop. A missing
binary is an error naming the other one.

#### Where answers are saved

In arena's state dir, never in the home or the working directory — `<state>/<prefix>/ask/<UTC
time>-<pod or N-pods>/` with `prompt.txt` (exactly what was sent), `answer.md` (or why there is
none) and `run.txt`; folders 0700, files 0600, the newest 50 kept. `<state>` is `$ARENA_STATE_DIR`,
else `$XDG_STATE_HOME/arena`, else `~/.local/state/arena`. A `--dry-run` saves nothing.

## Fleet status and money

### `snapshot`

`snapshot [--json] [--public] [--out FILE] [--no-probe]` — the fleet in one **read-only** picture
(one list per provider + the details query, the local proxy file, the health cache, and a
**reachability probe**: a TCP connect to the SSH endpoint of each billing machine on the prefixed
`MACHINE_NAME_LIST` — never staff `@` boxes or off-list pods — all at once, ≤ 3 s, waiting only for
sshd's `SSH-…` greeting — nothing sent, no login, no command; **no checks**): `pods list`'s columns
plus `SSH` (`ok`/`down`/`-` = not probed), `PROXY` (`:9500`, `:9500 stale`, `?` = remote proxy) and
`HEALTH` (`fail 2h GPU error`).

Public `up` needs the probe's answer: a pod listed running whose SSH port is silent reads `starting`
(the vocabulary stays `up|starting|down`, documented in `web/fleet.html`); `--no-probe` skips it
(then running + endpoint = `up`).

`--json` = the internal snapshot (ids, endpoints, costs — never publish it).

`--public` = the dashboard JSON, an **allowlist by construction**: only *prefixed*
`MACHINE_NAME_LIST` pods (`@` staff boxes and off-list pods are left out) by short name, with GPU,
`up|starting|down`, health + age + a reason from a fixed vocabulary (never check text), maintenance
start/end (only a complete RFC 3339 time, re-printed in UTC — anything else is dropped),
`updated_at`, `complete`. No IPs, hosts, ports, ids, providers, costs or keys (pinned by a leak
test).

`--out` is atomic and the public file is 0644 whatever the umask; if no provider answers — or the
one that failed leaves no machine to show over a page that listed some — it fails and leaves the old
file (the page shows it going stale, never "No machines.").

### `teardown --check`

`teardown --check [--json]` — the **end-of-program audit** (read-only; deletes nothing): a `✓ ✗ ? –`
checklist of what is still billing or scheduled, each with the exact command that cleans it up.

- Every pod on every configured provider in **any** state (a stopped pod keeps its name and still
  bills its disk; only `TERMINATED` is left out) — fix `pods terminate --all` only when every
  listing answered and every pod is this cohort's (`{prefix}-…`), else one `pods terminate <id>` per
  cohort pod; staff boxes (`@` list entries) and other pods are labelled and left to you, never in a
  fix;
- RunPod **network volumes** (REST v2 `GET /v2/network-volumes`, else GraphQL
  `myself.networkVolumes`; GraphQL field names taken from existing clients, not live-verified) with
  size and an **estimated** ~$/month at $0.07/GB/month — fix a `curl -X DELETE
  …/v2/network-volumes/<id>` (irreversible);
- this cohort's **enabled OpenRouter keys** with usage — fix `keys revoke <names>` (by name: `--all`
  only reaches machines that still have a pod; staff boxes' keys are only named in a note);
- this user's crontab — arena's block (`cron remove`) and hand-added lines mentioning arena
  (`crontab -e`);
- pending **`at` jobs** whose command (from `at -c`, never its environment) mentions arena or a
  legacy fleet script (`destroy_pods`, …) — fix `atrm <ids>` (`at` not installed is said, not
  failed);
- and forwards left in the **local** proxy file — fix `proxy apply`.

A source that couldn't be read (a provider's 429, a remote proxy, an unreadable `at` job, a proxy
`server` block arena can't parse) is `?` — **never "empty"** — and, like anything remaining, makes
the exit non-zero; a provider with no key is `–` (not checked). Exit 0 only when all clear.

Only this user's crontab/`at` queue are read (not root's, not `/etc/cron.d`); shown commands have
secret-looking values redacted (`NAME=…`, `--flag=…`/`--flag …` named key/token/secret/pass,
`sk-`/`rpa_`/`hf_` words; tab-separated fields too). `--json` = the same checklist for scripts.

After the verdict come `info:` lines with the prepaid balances (and the spend RunPod reports that
arena doesn't list: volumes, stopped disks, serverless) — informational, never part of the verdict
or the exit; `--json` carries them as a top-level `balance`.

### `balance`

`balance [--json] [--check] [--no-live] [-v]` — money left on each prepaid account, how long it
lasts and **where the number comes from** (read-only). A balance is

- **live** — **RunPod** GraphQL `myself { clientBalance currentSpendPerHr spendLimit underBalance }`
  (a field RunPod drops costs that field: the query is retried with `clientBalance` alone), **Vast**
  `GET /api/v0/users/current/` → `credit` (that body carries the email and API key: only `credit` is
  read, and it is never quoted), **Shadeform** `GET /account` (undocumented: `balance` and
  `hourly_burn` only — the body's email, phone, tax id and address are dropped at parse and never
  quoted; at $0 Shadeform's behaviour is undocumented: keep Auto Top Up on) — when read now or under
  5 min ago;
- else **est.**: the last live reading (or `balance set`) minus max(billed, metered) spend since —
  *billed* from RunPod's REST v2 hourly billing history (every resource on the account, but it lags:
  live, a bucket appears about an hour after its usage, filled in ~5-min steps — the hour in
  progress isn't there, nor is an empty one — so for the first hour metering alone carries the
  estimate), *metered* from arena's own burn samples (each step charged at its higher end), taken
  per UTC hour as the larger of the two — shown `~$X est. (live n/a 2d, billed)`, for up to 14 days;
  else `?` with `arena balance set <acct> <USD>`.

Estimates err low (they warn early, never late) and never rise between readings.

#### What the burn counts

**Pods nobody priced** (RunPod v1's REST list has no $/h; `arena balance` and teardown don't fetch
details; after GraphQL nothing does) are counted at the last price seen for that pod (remembered in
the state from any surface that saw one — an enriched `pods list`, the TUI, a v2 listing, the result
of any create — `create`, `up` and its replacements, `replace`, `migrate copy`, the TUI's add-pod
form — only from pods that bill by the hour, and kept while the pod keeps being listed, 15 days
after that), else at a stand-in that errs high: a known GPU at the most it is known to cost
(RunPod's recorded catalog, the presets, the dearest live catalog/offer price seen in
`offers`/`create`/`up`), an unknown GPU type at the ceiling `BALANCE_UNPRICED_PER_GPU` (default $8,
above the dearest recorded catalog price) per GPU — shown `~$X/h` with a note.

**Disks**, from the listing's sizes: a running RunPod pod's container and volume disk at RunPod's
$0.10/GB-month on top of its listed $/h (live: a listed $0.13/h 3070 with a 60 GB disk drained
$0.138/h); a stopped RunPod pod's volume disk alone at $0.20/GB-month (its container disk isn't
billed — live, a stopped pod with no volume cost $0.00/h); a stopped Vast instance's disk at a
$0.50/GB-month ceiling (a running one's price includes it) — so after a nightly `pods stop --all` a
healthy account reads ok.

A running pod with no GPU information, a stopped pod whose (volume) disk size isn't listed, or a
burn not known at all make the account `unknown` ("burn unknown: …", never "no burn").

A listing that fails (a Vast 429 is retried once first, when it asks for ≤ 2 s) is stood in for by
the burn observed under 30 min ago, marked stale (`-v` says so) — older, a $0 is unknown and
anything more a floor.

RunPod's own rate, while it is read, replaces the stand-ins; it lags a create, stop or terminate by
minutes, so "RunPod reports $X/h more than arena lists" needs two readings at least 5 min apart.

**Known undercounts:** a pod created and destroyed between two observations while billing is
unavailable; and spend arena doesn't list that falls inside billing's lag.

Vast's negative `balance`, when reported, is shown as "Vast reports $X owed" (never subtracted: what
it means is for the live console comparison to settle).

#### Burn, runway and levels

**Hetzner** is postpaid: €/h, no runway, never a warning; volumes aren't counted.

Burn = the higher of the provider's own rate and this fleet's billing pods there (a provider that
failed to list is an unknown burn — never $0/h); runway = balance ÷ burn.

Levels: **critical** under 24 h, at ≤ $0, or on RunPod's `underBalance` (at $0 RunPod *terminates*
pods without a network volume); **low** under `BALANCE_WARN_HOURS` (default 72, `0` = runway levels
off; malformed → 72 and a warning here — `config check` fails on it).

#### The reading and the fallback check

Lists the fleet (no details) while reading every account (≤ 10 s), records the reading in
`balance.json` (0600, next to the health cache, per prefix, scoped by a 32-bit key fingerprint),
then runs the **fallback check**: the estimate from a reading 30 min–~1 day old against the live
truth, metered from the listing's burns alone (what the fallback will have once RunPod's rate is
gone), with this run's billing and with metering alone — either reading high is a ⚠ (`fallback check
✓ over 26h, checked 3m ago (the estimate read $0.40 low)`), so a broken fallback shows up while
GraphQL still works.

If the state file can't be saved (a probe writes and fsyncs a state-sized file first, so a full disk
or a spent quota counts), inline surfaces read no live balance at all — they'd read it on every look
— and say nothing; `arena balance` reads live and says so.

`--no-live` shows what the fallback says (no live call, nothing written); `-v` adds the burn's
parts, spend limit, anchors, billing buckets, last failures and the state file.

Exit 0; with `--check` 2 if any account is critical, else 3 if any is unknown while burning, else 1
if any is low. A failing source is one note here and a ⚠ row in `config check` — nowhere else does
it print or change an exit.

### `balance set`

`balance set <runpod|vast|shadeform> <USD>` — record the balance read off the console when every API
is dead (a manual anchor; a local write, scoped to the configured key; the next live reading
replaces it).

`create`/`up` print the target account's balance next to the plan — the runway after the new pods
(`n ×` the dearest option; with no `--gpu` list or `--max-price`, the spec's GPU and tier priced
live alongside, within the same 5 s — else the live price `offers`/placement saw for that GPU and
tier under a day ago, `~` with its age — else a stand-in), and RunPod's refusals ahead of time:
under an hour's credit for them, over the spend limit, `underBalance` — never blocking.

## Config, plans and the provider APIs

### `config check | set | which`

`config check` — validate that the keys the selected provider + proxy + backup need are present
(never prints secret values; exits non-zero if a required key is missing or any key is set to a
value it can't use — every such key is a ✗ row and is named in the error; ⚠ rows, e.g. a key file
this user can't read or no `ARENA_START_DATE`, are advice). Copy `config.env.example` to get
started.

`config check | set | which` — `check` is the read-only doctor (keys + setup readiness); `config set
KEY VALUE` writes a key (e.g. an API key) into config.env — or give just `KEY` and **pipe the value
on stdin** (`printf %s "$TOK" | arena config set HF_TOKEN`, script-friendly, keeps secrets out of
`argv`/`ps`), or with no args prompt interactively (the picker shows which keys are already set);
`config which` shows the active config file (path, readable/**writable**), what parsed, and any keys
coming from the environment.

#### `ASK_AI`

Which AI [`pods ask`](#pods-ask) and the dashboard's `?` ask: `claude` (the default), `codex`, or
`off` — which turns `pods ask` and `?` off whatever `--ai` says (`--ai` picks between claude and
codex only). `config check` shows it in its "Asking an AI (`pods ask`, the dashboard's `?`)" section
(and where the binary is, or a ⚠); any other value fails the check and refuses to run.

Each AI's model and effort: `ASK_CLAUDE_MODEL`, `ASK_CLAUDE_EFFORT`, `ASK_CODEX_MODEL`,
`ASK_CODEX_EFFORT` — unset, its CLI's own. The model is passed as claude's `--model` / codex's `-m`
(`sonnet`, `gpt-6-luna`: letters, digits and `. _ - : / @ [ ]`); the effort is `low`, `medium`,
`high`, `xhigh` or `max` (claude's `--effort`, codex's `model_reasoning_effort`). `--model` and
`--effort` go over them for one run (so does the environment: `ASK_CODEX_MODEL=gpt-6-luna arena tui`);
the dashboard's Tab switches AI with each one's own. A small
model at `low` is the quick choice: on one pod's prompt (2026-10-27) codex `gpt-6-luna` at `low` took
about 5 s, `gpt-6.1-sol` at `low` 7–9 s, claude's default 12–19 s. `config check` shows the ones set;
a malformed one fails it, and `pods ask`/`?` refuse to use it.

### `plan check | show`

`plan check | show` — a scheduled provisioning plan (`arena-plan.json`, see
`arena-plan.example.json`): per-day target fleets with **GPU-first fallback chains** (e.g. `A4000`
across community→secure→vast, then `3090`, then `A5000`) and a night **window** + caps. `check`
validates + prints the detected local time/timezone and every day's resolved date; `show [--date]`
previews the fallback order and fill-to-target vs the live fleet. Read-only today; the timed
executor + `arm`/`disarm` (which only fires inside the night window, in system-local time) are the
next step.

### `gpus`

`gpus` — the old name for RunPod's part of [`offers --all`](#offers), kept for habits and scripts
(hidden from `--help`), unchanged: the same table, `gpus --json` the same array of rows, and with
another `--provider` the presets, as before.

RunPod's **full live catalog** (via `GET /v2/catalog/gpus` for both tiers, the default — STOCK
shown as `community/secure`, counted over hosts at or above the CUDA floor, and the best signal the
answer carries (a data center's level, a CUDA version tagged available) — no GraphQL; via GraphQL on
`RUNPOD_API=v1`) when `RUNPOD_API_KEY` is set, else the local presets.

Shows VRAM, **live** community/secure $/hr (a tier that doesn't offer the card shows `-`: v1's
GraphQL quotes phantom prices for those, dropped by its `secureCloud`/`communityCloud` flags) and
RunPod's stock hint (`~` marks a preset estimate). On v1, if RunPod rejects the priced query (HTTP
200 with errors, or a non-auth 4xx/5xx), the plain catalog query is tried before falling back to
presets.

`--json` emits rows `{id, display_name, memory_gb, community_price, secure_price, stock_status,
creatable, source, price_source}` (+ `secure_stock_status` on v2; incl. catalog entries v1's create
API rejects, `creatable: false`; on v2 every row is creatable).

### `api get`

`api get <runpod|runpod-v1|runpod-v2|vast|hetzner|shadeform> <path> [--raw]` — **read-only**
passthrough for what no command covers (`pods/<id>`, `catalog/datacenters`, `network-volumes`,
`billing/pods`; Vast `users/current/`; Hetzner `locations`; Shadeform `instances/types`, its key in
`X-API-KEY`; `sshkeys` there refuses `--raw`): one GET (there is no other verb) with the configured
key, printed as pretty JSON.

The path is relative to the provider's API base — absolute URLs, `//host`, `..` and `#` are refused
before the key is read, redirects aren't followed — so the key only goes to the provider. A leading
copy of the base's own path is dropped (`/v2/pods` on v2 is `pods`, not `…/v2/v2/pods`); another
version (`v1/…` against v2) is refused, naming `runpod-v1`.

Secrets are redacted (env values, key/token/password fields, `rpa_`/`sk-`/`hf_` tokens, SSH key
material) unless `--raw`; the API key itself is never printed, `--raw` included (scrubbed from the
body as received, before an error message clips it).

## Backups, branches and the archive

These keep the participants' work, and have their own page, [BACKUPS.md](BACKUPS.md):

| Command | In BACKUPS.md |
| --- | --- |
| `pods backup` | [`pods backup`: the full save](BACKUPS.md#pods-backup-the-full-save) |
| `pods set-branch` | [`pods set-branch`](BACKUPS.md#pods-set-branch) |
| `pods init-branches` | [`pods init-branches`](BACKUPS.md#pods-init-branches) |
| `pods pull` | [`pods pull`: the file backup](BACKUPS.md#pods-pull-the-file-backup) |
| `pods baseline` | [`pods baseline`](BACKUPS.md#pods-baseline) |
| `pods restore` | [`pods restore`](BACKUPS.md#pods-restore) |
| `pods replace` / `pods migrate` | [Moving a pod: `pods replace` / `pods migrate`](BACKUPS.md#moving-a-pod-pods-replace--pods-migrate) |
| `cron install / remove / show` | [`cron`: the lines that run it all](BACKUPS.md#cron-the-lines-that-run-it-all) |
| `pods idle`, `pods reap` | [Idle pods and the auto-reaper](BACKUPS.md#idle-pods-and-the-auto-reaper) |
| `archive …` | [Backup archive (`arena archive`)](BACKUPS.md#backup-archive-arena-archive) |

`archive up|status|sync|resize|access|ssh-config|ssh|forget|unforget|down` — the **backup archive**:
an always-on Hetzner VM with two volumes (dated snapshots, all files) holding copy #2 of
`LOCAL_BACKUP_DIR`, pushed by cron, readable by participants once `access on`; never part of the
fleet. See [Backup archive](BACKUPS.md#backup-archive-arena-archive).

## The dashboard (`arena-tui`)

`arena-tui` (TUI) — interactive dashboard (ratatui): pods from the configured provider plus, per
pod, GPU stats via `nvidia-smi`, git branch, a setup-health check, and an optional progress signal
(`PROGRESS_CMD`) — all over SSH in **one** probe per pod. Fetching runs in a **background task** so
the UI never freezes; it auto-refreshes (`ARENA_REFRESH_SECS`, default 5) and `f` cycles the cadence
live (2/5/10/20/60s). Provider via `ARENA_PROVIDER` (default `runpod`), config via `ARENA_CONFIG`.

### Columns

**Columns**: a provider badge (`R`/`V`/`H`), NAME (short `apple` by default; `s` toggles full
`arena8-apple` and the choice persists to `~/.config/arena-tui/prefs`), STATUS, a **SET** health
glyph (`✓/✗/·` for `~/.name`, the deploy key, git origin→GitHub), GPU (live from `nvidia-smi`, e.g.
`2×RTX A4000` — the provider list API omits this), GPU%/MEM/TEMP, **BRANCH** (autocommit branches
shortened to their `w1d2` label; `main`/others shown as-is), and progress. Below ~105 columns (or in
the detail view's split) TEMP, MEM, BRANCH and DISK drop out in that order, so
NAME/STATUS/GPU/$/H/HEALTH/PROXY stay whole down to 73 columns.

### Fleet columns from the core snapshot

**Fleet columns from the core snapshot** — every refresh is one `snapshot::build` (`arena
snapshot`'s builder) over the per-provider listing, the local proxy file and the health cache, so
these read exactly as the CLI prints them: `$/H` (provider currency, `-` unless billing), an **M**
badge for a host maintenance window (detail pane: window + note) — else **L** for a locked pod
(detail pane: `status: … · locked`; restart/stop/terminate on it are refused with the unlock hint,
and the dashboard never unlocks), **HEALTH** = the last `pods test --deep` verdict + age (`pass
12m`, `fail 2h`; detail pane: worst issue + the failing check's text), **PROXY** = the stable port,
green live / yellow stale (`:9501 stale` when wide), `-` no forward, `?` remote proxy. The **summary
bar** is `pods list`'s footer (`$` and Hetzner `€` kept apart, unpriced pods counted) plus per day,
**led** by a yellow `⚠ vast failed to list — its pods (and their cost) are missing` when a provider
didn't answer (first, so a narrow terminal can't cut it off), and ending with the account prepaid
balances (`arena balance`'s views, observed in the details round — at most one live read per account
per 5 min — and recorded in the CLI's state file; a low account moves to the front in yellow, a
critical one in red, with its amount: `‼ runpod $1.20 ≈1.9d`; an unfunded opt-in account with
nothing billing there stays at the end, `shadeform $0.00 unfunded`). API calls per refresh are
unchanged (one list per provider); the details query (GPU/$/maintenance) runs every 60 s or on `r`
(≥10 s apart); one that fails part-way still shows what it filled (as `pods list` does) over the
last good answer, with a footer warning.

### Deep checks and marked sets

**`d`** deep-checks the cursor pod (or the marked set) **in the background** — no modal; the row
says `checking`, the footer reports the tally when done, and the verdicts go to the same health
cache `arena snapshot` reads. **`D`** deep-checks every running pod whose GPUs it has read and found no one using: the
first press says how many and names the busy ones it leaves out (any GPU with a compute process,
over 5% — the reaper's idle line — or holding over 1 GiB: a deep check loads the GPUs for about a
minute and on a busy one reads as a failure), and counts those it leaves out as not read yet (no
metrics so far), whose GPUs couldn't be read (the read failed, or `nvidia-smi` gave no GPU or no
utilization: a pod too loaded to answer must not look idle — `d` checks one on purpose) or
CPU-only (Hetzner, or 0 GPUs listed: always a fail). A second `D` within 5 s starts the pods the
first press counted that still qualify — never one a refresh added in between. In HEALTH, a pass older than 26 hours is grey: it says little about today (the
daily `cron install --health` keeps it fresh; the two hours absorb its drift). **`/`** marks pods with the CLI's selector syntax (`alpha..delta`,
ids, `-x`/`!name`, `--on`, `--gpus`); a typo marks nothing and leaves the marks as they were, saying
why in the footer; a valid one replaces the marks. Marks are per `provider:id` (a Vast and a Hetzner
id can collide). `all` is refused, and since `/first..last` marks a cohort in one line,
terminate/restart/stop on a marked set of more than 5 pods — or of every listed pod — ask for `ALL`,
like the whole fleet (smaller sets: the pod count). A marked-set confirm names the pods once (the
first 10, then `… and N more`), with the token prompt always on screen. `Esc` on the list clears the
marks (it never quits).

### Asking an AI about pods

**`?`** asks claude or codex (`ASK_AI`) about the cursor pod — or the marked set, compared in one
prompt — exactly what [`arena pods ask`](#pods-ask) does: a one-line question, prefilled with the
default, naming the AI with its model and effort and where the data goes (`ctrl-u` clears it; **Tab**
switches to the other AI, as [`ASK_<AI>_MODEL`/`_EFFORT`](#ask_ai) set it up, and the next `?` opens
with the one last picked); Enter asks **in the background** like `d` (the footer says `asking claude
about devtest-alpha…`), and the answer opens in its own scrollable popup when it lands (Esc closes
it; a modal open meanwhile keeps it waiting), its Markdown rendered: bold, commands in cyan, a
bullet's wrapped lines under its text. Saved like
`pods ask`'s. With `ASK_AI=off` it opens nothing and the footer says how to turn it on. A marked set
keeps to this cohort's pods, as `pods ask --all` does: a staff box or another cohort's pod marked
along is left out (the input names it, the footer says how many); to ask about one, put the cursor
on it with nothing marked.

### SSH time budgets

Every pod SSH call (metrics, test/run/backup/set-branch/setup, deep check) goes through `Remote`
with a budget (90 s / 30 min / 5 min / 2 min / setup's per-step / 150 s), so a wedged pod ends an
action with `timed out after …`; only `c` (your interactive shell) is a plain `ssh`.

### Navigating

**Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU breakdown, full
branch/origin/health, util/temp **sparklines**). `Ctrl-C`/`q` quit. A hangup or a TERM ends it at
once (exit status 129 or 143), and so does its terminal hanging up (a dropped SSH session, a killed
tmux server: 129) — never waiting on its event loop, which a dead terminal stalls. However it ends,
nothing new is started from then on, the terminal is put back first (as far as it can be; not while
`c`'s ssh has it — that's ssh's), and then every child still running is stopped — a question's AI,
an action's, a refresh's or a backup's ssh, scp and rsync, `c`'s ssh — TERM, then KILL 2 s later;
the wait for that is never on a raw screen. `Esc` only steps back (detail → list; on the list it
clears the marks).

### Acting on pods

**Act** on the selected pod with `a` (restart / stop / terminate / backup / setup / test / run /
set-branch), on the **whole fleet** with `A` (backup / setup / test / run / set-branch; restart,
stop and terminate only on a **marked** set — a stop there skips, and names, pods not running and
pods whose provider has no stop (Shadeform), and refuses a set of only those, as `pods stop` does),
or **add pods** with `n` (it lists the fleet in the background first). `run` and `set-branch` pop a
text-input modal to type the command / branch; `run` shows each pod's whole output (its last 40
lines, stderr too when it fails); `test` is read-only. Every mutation goes through a confirmation
modal — the *only* place the TUI mutates anything. Lifecycle actions (restart/stop/terminate)
require **typing the pod's exact name**; restart is destructive like terminate (red, and its modal
says it wipes the container disk on RunPod/Vast; Hetzner: "disk kept"); whole-fleet mutations
require typing **ALL** (a marked set: its count, or `ALL` as above); every confirm says what will
happen in plain words (`alpha: commit + push the current branch`), never the generated `ssh …` line
— `v` shows that for backup/setup/test — and backup/setup/test take a single `y`. The dashboard's
reads stay reads.

### Adding pods

**Add-pod (`n`)** is an interactive form: `↑↓` moves between fields, `←→` changes the value. Pick
the provider (unavailable ones — no API key — are greyed out), cloud type (RunPod only), GPU type
(full option list shown) and count, and how many pods; it previews the names it will allocate, then
creates on `enter`.

## Testing

```bash
cargo test --release   # everything offline: pure planners, fake providers, FakeRemote, fixtures
```

### The live smoke test

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
public IP fails as `FAILED endpoint` (see [`docs/TODO.md`](TODO.md)); the guard still cleans it up.

### The live attestation check

The offline tests verify a recorded attestation (`crates/core/src/fixtures/attestation_*.json`),
so they can't notice Sigstore rotating a key or GitHub changing its bundles. This opt-in test
does: it reads the latest release's `arena-x86_64-unknown-linux-gnu` attestation from GitHub
(2 anonymous API requests, no account, nothing spent), verifies it as `arena update` does, and
checks it doesn't verify as another workflow's or another repository's.

```bash
ARENA_LIVE_ATTEST=1 cargo test --release -p arena-core --lib attest -- --ignored --nocapture
```

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
- **No tools, and it fails closed.** Everything the reviewer needs is in the prompt, so it runs with
  none, in an empty temporary directory, with a minimal environment (`PATH`, `HOME`, the locale, XDG
  and proxy variables, and the reviewer's own config dir and API key — not the rest of yours),
  exactly as [`pods ask`](#pods-ask) runs it: claude with `--tools ''`, `--safe-mode` (no hooks, MCP
  servers or CLAUDE.md), `--strict-mcp-config` with an empty `--mcp-config` and no saved session;
  codex `exec` read-only and ephemeral, without your `config.toml` or execpolicy rules
  (`--ignore-user-config --ignore-rules`: `--model-codex` and `--effort` still pick), with
  `mcp_servers` emptied and its shell, code mode, apps, plugins, browser and computer use,
  sub-agents, image generation and viewing, hooks and web search turned off (every web search
  feature it lists too, and its sleep tool, skill search, tool suggestion, goals and worktrees).
  Neither is sent anything until it has confirmed it knows every one of those switches — `codex
  features list` names each feature, `claude --help` lists each flag — and codex until that list,
  run again with the review's overrides and an empty `CODEX_HOME`, names each one it turns off and
  shows nothing else on but the features `pods ask` allows; both listings are read strictly, as
  `pods ask` reads them (every row `<name> <stage> true|false`). A check that fails, times out
  (20s), can't be read whole, or finds one missing or one more on refuses that reviewer (`ai-review:
  codex: couldn't confirm its tools are off — not asked (…)`), and the other still runs. (Here
  codex's sandbox can't run a command anyway: bubblewrap fails.)
- **Output** goes to `${XDG_STATE_HOME:-~/.local/state}/arena-review/<UTC time>-<range|prN>/`
  (`--out DIR` for another parent): `prompt.txt`, `codex.md`, `claude.md`, and each reviewer's
  stderr in `codex.log`/`claude.log`. It ends by printing those paths and each review's last line,
  `Blocks merging: yes|no (…)`. Exit 0 when every reviewer that ran answered, whatever it found; 1
  when one failed, timed out (`--timeout`, 3600 s each) or couldn't confirm its tools are off, or
  none could run (a missing `codex` or `claude` is skipped with a message); 2 for a usage error, an
  empty diff or key patterns it can't read (refused).
- **Cost.** It runs on the operator's own codex and Claude Code logins, so it spends their
  subscriptions' usage, not the project's API keys: a 16-commit, ~420 KB prompt is roughly 125k
  input tokens per reviewer, and took codex 2½ minutes and claude 5. `--effort` (default `high`,
  for both) sets how hard they think: codex's own default here is `none`, a skim that answers in
  25 s. `--model-codex`/`--model-claude` pick the models.
- It's a second opinion, not a gate: read the findings, fix what's real, note what isn't.

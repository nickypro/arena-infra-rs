# Feature map — `nickypro/arena-infra` (legacy) → `arena-infra-rs`

Parity tracker between the legacy Python/bash scripts and the Rust rewrite
(`arena` CLI + `arena-tui`), as of phases 0–4. Legend: ✅ ported · 🟡 partial · ❌ missing ·
➕ new (no legacy equivalent).

Every fleet command picks pods with **one selector syntax** (`arena_core::selector`): bare
`apple`, full `arena8-apple`, absolute `@james-gpu`, provider ids, ranges `apple..mayor`
(`MACHINE_NAME_LIST` order), `all`/`--all`, narrowed by `--exclude <targets>`, `--gpus N`,
`--on <provider>`. Typos and selections that narrow to nothing are errors; `run`/`cp`/`pull`
take the targets as `-t`. The TUI marks pods with the same syntax (`/`). "Selector" below
means this.

## Pod lifecycle

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `create_new_pods.py -n/-a/<names>` `--gpu-type --gpu-count --cloud-type --docker-image --disk-space-in-gb --volume-space-in-gb` | `pods create [names…] -n/-a --gpu --gpus --cloud --disk --volume --image [--bootstrap]` (+ `--keep-trying --retry-mins --retry-secs`) | `n` add-pod form | ✅ | Plus **multi-option placement**: `--gpu A4000,3090 --cloud community,secure --max-price X --order cheapest\|listed` — one create at a time per name, capacity blocks an option for the round. `--gpu` is checked against RunPod's live catalog (aliases `3070`, `L4`, …; "did you mean"). Ends with a proxy sync (`--skip-proxy`). |
| — | `pods up [names…] -n/-a …` (create's flags) `--check --check-attempts --timeout --interval --no-setup --no-wait` | — | ➕ | One-command spin-up as **one pipeline per pod**: endpoint → proxy → setup → [`--check`: deep check; a FAIL host is terminated and the name recreated, never on a host that already failed] → API keys → `[name] READY`/`FAILED <stage>`; summary table; non-zero exit unless every name is READY. |
| — | `offers [--gpu --cloud --max-price --gpus --order --json]` | — | ➕ | Read-only: the option plan `create`/`up` would try ($/h per pod, price source, stock hint, what the cap dropped). |
| `list_pods.py` | `pods list` (`--json --probe --no-probe`) | List view | ✅ | NAME PROVIDER ID STATUS GPU $/H ENDPOINT MAINT + `fleet: $X/h across N billing pod(s)` footer (billing per `status::bills_hourly`: up or on its way up, or any existing Hetzner server; € kept apart). GPU/$/maintenance from one read-only GraphQL query; GPU confirmed over SSH (`nvidia-smi`) for the table. |
| `stop_pods.py --include --exclude` (bulk, all RUNNING) | `pods stop <selector> / --all [--wipe-ok]` | Menu→Stop `[s]` (typed name) | ✅ | Stops every selected **billing** pod. A stopped RunPod pod keeps no container disk, so it needs `--wipe-ok` unless the repo is on a `/workspace` volume (setup links it there when the pod has one; each volume pod is asked where its repo really is). |
| `delete_pods.py --include --exclude` (EXITED only) | `pods terminate <one pod> / --all [--revoke-key]` | Menu→Terminate `[t]` · `/`-marked set via `A` | ✅ | One pod (name/id; a shared name is refused) or the whole fleet — no ranges, it's irreversible. `--revoke-key` also deletes its OpenRouter key(s). Ends with a proxy sync. |
| `kill_pods.py --timeout` (stop→wait→delete) | — | — | ❌ | `pods kill` was dropped (3604c0c): `terminate` deletes directly; use `stop` first if you want the old sequence. |
| `setup_em.sh --force` | `pods setup [selector] [--force --timeout --hf-token --cc-token --zsh-install --no-vscode]` | Menu/Fleet→Setup `[p]` | ✅ | Every step on a budget (copies 60 s, config 300 s, Hetzner script 1800 s; `--timeout`/`SETUP_TIMEOUT_SECS`); a wedged pod reports `timed out at <step>`, the rest carry on. Fetches only the default branch. Distributes per-host API keys to the pods that set up OK. Ends with the best-effort **VS Code warm-up** (server + extensions + default interpreter; 300 s; a failure is a warning, never a failed pod). With a `/workspace` volume the repo is **moved onto it** — its own best-effort step (900 s, `REPO_ON_VOLUME=0` off): `BACKUP_REPO_PATH` becomes a link; after a reset the volume copy wins over the image's *untouched* checkout (kept aside), and work done in the checkout since the reset is never moved aside. |
| — | `pods restart <one pod> [--wipe-ok --no-setup --skip-proxy]` | Menu→Restart `[r]` · marked set | ➕ | **Wipes the container disk** on RunPod (Vast treated the same): refused without `--wipe-ok` unless the repo is on a `/workspace` volume — the pod is asked where it really is (setup links it there); afterwards re-runs `setup` (re-links it) and syncs the proxy. Hetzner keeps its disk. |
| — | `pods rename <old> <new>` / `--from-prefix P` | — | ➕ | Metadata only (no restart); rewrites `~/.name`, moves the OpenRouter key row/name, syncs the proxy. |
| — | `pods reimage <selector> [--image]` · `pods replace <one>` · `pods migrate copy/cutover/finish/revert/status` | — | ➕ | In-place image swap (wipes the disk); blue-green replace keeping name + files; staged migration with a verified, auto-reverting cutover. |

## Git / branches / backup

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `sync_git.sh -m --exclude` (commit+push current branch) | `pods backup [selector] [--message --no-pull]` | Menu/Fleet→Backup `[b]` | ✅ | The full save: git-push **whatever branch the pod is on** (skips `main`/`master`, never switches) **and** the rsync file backup (`--no-pull` = git only). 5 min per pod's push; the rsync has no time budget yet. |
| `init_branches.sh <day>` (make autocommit branch) | `pods init-branches [selector] [--week --day]` | — | ✅ | Creates + pushes each pod's `autocommit-{prefix}-wNdM-{name}` branch, no commit. |
| `list_branches.sh` (table of current branch) | `pods run 'git -C … branch --show-current'` | BRANCH column | 🟡 | No dedicated command; the TUI shows it. |
| `backup.sh <label>` (**rsync pod files → local**) | `pods pull [label] [-t … --dir --max-size --remote-path --no-git --no-big]` | — | ✅ | Rsync home → `<dir>/<label>/<pod>/` (size-capped) plus a complete `big/` mirror; keeps `.git`. A repo linked onto the volume is pulled as its tree, in the same place. No time budget yet. |
| — | `pods restore <one pod> [--from <label>\|big --path --dir --timeout --overwrite-newer --with-git --dry-run]` | — | ➕ | Pushes a backup back (default: the newest snapshot) over the direct endpoint. Never deletes; never reverts files newer on the pod (`--overwrite-newer` does); replaced files kept in `/workspace/.arena-restore/<time>/` (volume mounted) else `~/.arena-restore/<time>/`; written through the repo's link onto the volume; no `.ssh`/rc files/`.name`/`.claude*`, no `.git` from a snapshot (`--with-git`); a pushed repo `.git` is checked after. Refuses a missing/empty backup (listing every label there is); confirms with source, destination, size, and a warning when the snapshot was written after the pod's last reset; 2 h budget. |
| — | `pods set-branch <branch> <selector>/--all [--hard]` | Menu→set-branch `[g]` · Fleet | ➕✅ | Gentle ff-only by default; `--hard` resets to `origin/<branch>`. |
| `test_em.sh` (torch version) | `pods test [selector]` | Menu→test `[e]` · Fleet→test | ✅ | 90 s per pod. |
| — | `pods test --deep [selector] [--json -v]` | `d` (background) + HEALTH column | ➕ | Is-this-pod-usable: per-GPU tensor op, driver ≥ floor (`MIN_DRIVER_VERSION`, else from `ALLOWED_CUDA_VERSIONS`), device count, GPU↔GPU copy + NCCL (>1 GPU), HF download speed, disk, host load, maintenance. FAIL/WARN/PASS per pod, `same host?` grouping, non-zero exit on FAIL. Verdicts go to the health cache. |
| `run_cmd.sh <cmd>` (sequential) | `pods run [-t …] [--timeout] <cmd>` (concurrent) | Menu→run `[x]` · Fleet→run | ✅ | Inside the conda env; 30 min per pod by default; a selection flag or `--dry-run` after the command is refused rather than run. |
| — | `pods run --background <cmd>` · `pods jobs [JOB] [--kill JOB]` · `pods logs [JOB] [-n N] [-f]` | — | ➕ | Detached course-test runs: one job id, everything on the pod under `~/.arena/jobs/<id>/` (survives your SSH session, not a restart); started with `setsid -f`, so SIGINT reaches the command as in a foreground run; `logs -f` follows by byte offset. |
| `names.sh` (write ~/.name) | folded into `pods setup` (and `rename`) | — | ✅ | |
| — | `pods cp <file> [dest] [-r --timeout -t …]` | — | ➕ | scp to every pod, mirroring the repo path by default; verifies the file landed. |

## Proxy / SSH config distribution

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `proxy/nginx_pods.py -v` (render nginx stream cfg) | `proxy plan [--out]` | PROXY column (live/stale) | ✅ | Read-only. A **sticky merge** of the current config with a per-provider listing: `+ ~ - =` per machine; a forward is removed only when its pod is confirmed gone (absent from a provider that listed OK). |
| `proxy/update.sh` (regen + reload nginx) | `proxy apply [--dry-run]` + auto-sync after `create`/`up`/`rename`/`reimage`/`terminate`/`restart`/`replace`/`migrate cutover\|revert` + `cron install --proxy` | — | ✅ | Serialized (`flock`), atomic, refuses to overwrite a config another run changed; a failed reload restores the old config. `SSH_PROXY_RELOAD_CMD` (empty = write-only). |
| `proxy/setup_nginx.sh` (install nginx-full, dirs) | — | — | 🟡 | `apply` assumes nginx is installed (or write-only); no first-time install. |
| `ssh_config_manual.py` (participant `~/.ssh/config`, direct IPs) | `ssh-config [--out]` | — | ✅ | Direct pod endpoints from the live list. |
| `ssh_config_proxy.py` (participant `~/.ssh/config`, proxy ports) | `ssh-config --proxy [--out]` | — | ✅ | Stable proxy ports by machine-list index. |

## Monitoring / keys / ops

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `gpu-top.sh [interval]` (live nvidia-smi dashboard) | — | dashboard + sparklines | ✅ | The TUI is the live monitor (per-pod probe over `Remote`, 20 s budget). |
| — | `snapshot [--json] [--public] [--out FILE]` + `web/fleet.html` + `cron install --snapshot DIR` | every refresh is one `snapshot::build` | ➕ | Read-only fleet picture (list columns + proxy port live/stale + last deep-check verdict and age). `--public` = an allowlisted JSON (list names, GPU, up/starting/down, health + fixed-vocabulary reason, maintenance times — no IPs, ports, ids, costs) for the static page. |
| — | `teardown --check [--json]` | — | ➕ | End-of-program audit: pods on every provider in any state, RunPod network volumes, enabled OpenRouter keys, arena cron lines, `at` jobs, proxy forwards — ✓ ✗ ? – with fix commands; unknown is never "empty"; non-zero unless all clear. Deletes nothing. |
| `copy_api_keys.py` (CSV → pod `~/.bashrc`/`~/.zshrc`) | `pods copy-keys [selector] [--keys-dir --hf-token --cc-token]` | — | ✅ | Per-host CSVs (openai/anthropic/openrouter) + broadcast Hugging Face and Claude Code tokens. Idempotent; 60 s per pod. |
| — | `keys gen/list/rotate/revoke/which` | — | ➕ | OpenRouter runtime keys via the provisioning API (one per machine, USD cap), written to `keys/openrouter_api_keys.csv`. |
| — | `gpus [--json]` | add-pod GPU list | ➕ | RunPod's live catalog (VRAM, community/secure $/h, stock), else presets. |
| — | `config check / set / which` | — | ➕ | `check` shows set/missing only (and which RunPod API is active); `set` can read the value from stdin, keeping it out of `argv`. |
| — | `plan check / show` (scheduled provisioning) | — | ➕ | Executor + arm/disarm still pending. |
| — | `cron install/remove/show` (`--pull --proxy --snapshot DIR --start-date`) | — | ➕ | Edits only arena's block of the user's crontab. |

## Config keys added in phases 0–4

| Key | Default | What it does |
|---|---|---|
| `RUNPOD_API` | `v1` | RunPod REST API: `v1` (RunPod retires it 2026-11-15) or `v2`; anything else is an error. Settable from the environment (`RUNPOD_API=v2 arena pods list`). |
| `SSH_PROXY_RELOAD_CMD` | absent → `nginx -t && nginx -s reload` | Run after a proxy write; **empty = write-only** (never reloads nginx). Settable (empty) from the environment, as the sandbox wrapper does. |
| `SETUP_TIMEOUT_SECS` | 300 (image pods) / 1800 (Hetzner script) | The main setup step's budget, 1..86400, for `setup`, `up`, `restart`, `replace`, `migrate copy`; `setup --timeout` wins. Checked before `up` creates anything. |
| `ARENA_STATE_DIR` | `${XDG_STATE_HOME:-~/.local/state}/arena` | Root of the health cache, `<dir>/<prefix>/health.json` (absolute path). Settable from the environment, so a cron job and the operator can share one. |
| `VSCODE_PREINSTALL` | `1` | `0` turns off setup's VS Code warm-up everywhere (`pods setup --no-vscode`: one run). Anything but 1/0/true/false/yes/no/on/off is an error. |
| `REPO_ON_VOLUME` | `1` | `0` turns off setup's move of the repo onto a `/workspace` volume (and the re-link after a reset). Anything but 1/0/true/false/yes/no/on/off is an error. |
| `VSCODE_EXTENSIONS` | `ms-python.python,ms-python.vscode-pylance,ms-toolsai.jupyter` | What the warm-up installs (and the deep check's `vscode` line expects): marketplace ids, comma-separated, or `none`. A malformed id fails `setup`/`up` before any pod is touched. |
| `MIN_DRIVER_VERSION` | derived from `ALLOWED_CUDA_VERSIONS` (13.x → 580, 12.8 → 570, …) | The deep check's driver floor (`580` or `580.65.06`; `none` = no check). A malformed value fails `up --check` before anything is created. |

## Provider status

| Provider | Status | Notes |
|---|---|---|
| RunPod REST v1 | ✅ | Default until the switch; retired by RunPod on 2026-11-15. |
| RunPod REST v2 (`RUNPOD_API=v2`) | ✅ | Live-verified on a sandbox pod (list/create/setup/proxy/rename/reimage/stop/terminate). Rename and maintenance stay on GraphQL. |
| Vast | 🟡 | Listing works (live-checked 2026-10-08); **create is broken against today's API** — the offer search sends a flat body that Vast now rejects (it wants `{"q": {…}}`, and `gpu_name` with spaces); PLAN "Later / maybe". No live prices before create. |
| Hetzner | ✅ | CPU VMs (`cx23` by default); restart keeps the disk. |

## CLI ↔ TUI parity

TUI per-pod menu (`a`): test `[e]`, run `[x]`, set-branch `[g]`, backup `[b]`, setup `[p]`,
restart `[r]`, stop `[s]`, terminate `[t]` — lifecycle actions need the pod's name typed back
(restart is flagged as wiping the container disk on RunPod/Vast).
TUI fleet menu (`A`): the same minus stop; restart and terminate only for a **marked** set
(`/` + selector; `x` clears marks), which needs its count — or `ALL` for more than 5 pods or
the whole fleet — typed back. `d` deep-checks the cursor pod or the marked set in the
background; `n` adds pods. Every fleet datum (cost, maintenance badge, proxy port + state,
last health) comes from the same core `FleetSnapshot` as `arena snapshot`.

Deliberately CLI-only (awkward or risky in a live dashboard): `up`/`create` placement flags,
`rename`/`reimage`/`replace`/`migrate`, `run --background`/`jobs`/`logs`, `pull` (local file
IO), `copy-keys` (reads local CSVs), `cp`, `keys`, `ssh-config` (prints a file),
`snapshot --public`/`teardown --check`, `config`/`cron`/`plan`. In the TUI, stop stays
per-pod by design, and restart/terminate on many pods need an explicit marked set (no "stop
everything" footgun).

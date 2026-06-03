# Feature map — `nickypro/arena-infra` (legacy) → `arena-infra-rs`

Parity tracker between the legacy Python/bash scripts and the Rust rewrite
(`arena` CLI + `arena-tui`). Legend: ✅ ported · 🟡 partial · ❌ missing ·
➕ new (no legacy equivalent).

## Pod lifecycle

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `create_new_pods.py -n/-a/<names>` `--gpu-type --gpu-count --cloud-type --docker-image --disk-space-in-gb --volume-space-in-gb` | `pods create [names…] -n/-a --gpu --gpus --cloud --disk --volume --image` (+ `--keep-trying --retry-mins --retry-secs`) | `NewPod` form | ✅ | Create-by-name + `--image` now supported. Rust adds capacity-retry loop. |
| `list_pods.py` | `pods list` (`--json --probe --no-probe`) | List view | ✅ | Rust probes GPU over SSH (provider list omits it), adds cost/branch/health. |
| `stop_pods.py --include --exclude` (bulk, all RUNNING) | `pods stop <target> / --all --include --exclude` | Menu→Stop (one) | ✅ | Bulk stop with include/exclude filters now supported. |
| `delete_pods.py --include --exclude` (EXITED only) | `pods terminate <target> / --all` | Menu→Terminate · multi-select `x` | ✅ | `terminate` = delete; `--all` covers bulk. |
| `kill_pods.py --timeout` (stop→wait→delete) | `pods kill <target> / --all --include --exclude --timeout` | — | ✅ | Stop → poll for EXITED → terminate, Ctrl+C-interruptible. |
| `setup_em.sh --force` | `pods setup --force` | Menu/Fleet→Setup | ✅ | |
| — | `pods restart <target>` | Menu→Restart · Fleet→Restart | ➕ | |

## Git / branches / backup

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `sync_git.sh -m --exclude` (commit+push current branch) | `pods backup --message --week --day` | Menu/Fleet→Backup | ✅ | Rust commits+pushes to `autocommit-{prefix}-wNdM-{name}`. |
| `init_branches.sh <day>` (make autocommit branch) | folded into `pods backup` | Backup | ✅ | |
| `list_branches.sh` (table of current branch) | `pods run 'git -C … branch'` | branch column | 🟡 | No dedicated command; branch shown in TUI + list. |
| `backup.sh <label>` (**rsync pod files → local**) | `pods pull [label] --dir --max-size --remote-path` | — | ✅ | Rsync home → `<dir>/<label>/<pod>/`, size-cap + excludes, concurrent. |
| `test_em.sh` (torch version) | `pods test` | Menu→test `[e]` · Fleet→test | ✅ | Now in the TUI too. |
| `run_cmd.sh <cmd>` (sequential) | `pods run <cmd>` (concurrent) | Menu→run `[x]` · Fleet→run | ✅ | TUI collects the command in an input modal. |
| — | `pods set-branch <branch> <target>/--all` | Menu→set-branch `[g]` · Fleet | ➕✅ | TUI collects the branch in an input modal. |
| `names.sh` (write ~/.name) | folded into `pods setup` | — | ✅ | |

## Proxy / SSH config distribution

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `proxy/nginx_pods.py -v` (render nginx stream cfg) | `proxy plan [--out]` | — | ✅ | |
| `proxy/update.sh` (regen + reload nginx) | `proxy apply [--dry-run]` | — | ✅ | |
| `proxy/setup_nginx.sh` (install nginx-full, dirs) | partly in `proxy apply` | — | 🟡 | `apply` assumes nginx installed; no first-time install. |
| `ssh_config_manual.py` (participant `~/.ssh/config`, direct IPs) | `ssh-config [--out]` | — | ✅ | Direct pod endpoints from the live list. |
| `ssh_config_proxy.py` (participant `~/.ssh/config`, proxy ports) | `ssh-config --proxy [--out]` | — | ✅ | Stable proxy ports by machine-list index. |

## Monitoring / misc

| Legacy script | Rust CLI | TUI | Status | Notes |
|---|---|---|---|---|
| `gpu-top.sh [interval]` (live nvidia-smi dashboard) | — | TUI dashboard + sparklines | ✅ | The TUI is the live monitor. |
| `copy_api_keys.py` (CSV → pod `~/.bashrc`/`~/.zshrc`) | `pods copy-keys --keys-dir --hf-token` | — | ✅ | Per-host CSVs (openai/anthropic/openrouter) **+ broadcast Hugging Face token** (gated repos: Llama 3 …). Idempotent. |
| — | `config check / set` | — | ➕ | |
| — | `plan check / show` (scheduled provisioning) | — | ➕ | Executor + arm/disarm still pending. |
| — | `cron install/remove/show` (backup cron) | — | ➕ | |

## CLI ↔ TUI parity

TUI per-pod menu: restart `[r]`, stop `[s]`, terminate `[t]`, backup `[b]`, setup `[p]`,
**test `[e]`**, **run `[x]`**, **set-branch `[g]`**.
TUI fleet menu: restart, backup, setup, **test**, **run**, **set-branch** (+ add-pod,
multi-select terminate).

Remaining deliberate CLI-only commands (awkward/low-value in a live dashboard):
`pull` (local file IO), `copy-keys` (reads local CSVs), `ssh-config` (prints a file),
`config`/`cron`/`plan`. Fleet-wide stop/terminate stay per-pod-or-multi-select by design
(no "stop everything" footgun). This is the "really annoying things" set left out.

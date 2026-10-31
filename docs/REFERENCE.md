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
  - [What works where](#what-works-where)
  - [RunPod (REST v2, the default)](#runpod-rest-v2-the-default)
  - [RunPod rate limits](#runpod-rate-limits)
  - [RunPod REST v1 (retiring)](#runpod-rest-v1-retiring)
  - [Vast.ai](#vastai)
  - [Hetzner Cloud](#hetzner-cloud)
  - [Hetzner: pre-built images](#hetzner-pre-built-images)
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
  - [Claude Code accounts (`claude`)](#claude-code-accounts-claude)
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
  - [`spend`](#spend)
  - [Running a second fleet from the same control box](#running-a-second-fleet-from-the-same-control-box)
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
version, `v<base>.YYMMDD.HHMM`: the base (`BASE` in `crates/cli/build.rs`, e.g. `2.3`) and the
commit's time in UTC. `arena --version` prints it with the commit: `arena 2.3.261030.1500
(5274b7a1c2)`. The base goes up a minor (2.0 → 2.1) when a command, flag or `--json` shape changes in a way a script or
habit notices, a major for a config or on-pod layout break that needs action; otherwise the date
stamp alone tells releases apart. A newer base is always newer: a 2.0 build updates to 2.1.

```bash
arena update --check     # newer release? what changed? (exit 10 = yes, 0 = up to date, 1 = couldn't tell)
arena update --dry-run   # also download and check it (provenance included), install nothing
arena update             # … then ask, and swap both binaries in (--yes to skip the question)
sudo arena update        # where root runs arena from someone else's ~/.cargo/bin (prod)
```

- **What changed:** the release's `CHANGELOG.md` entries that this build's own copy (compiled
  in) lacks, with their **Action needed** and **Config** lines first. `update` shows them only
  once the binaries are proven, saying whether the attestation also names this `CHANGELOG.md`
  (releases from 2026-10-28 on: then it must be that one, and be there, or the update stops);
  `--check` shows them unverified.
  Control characters in them are shown as `?`.
- **Checks before anything changes:** both binaries must match `SHA256SUMS`, must be proven
  built by this repository's release workflow (below), and the new `arena --version` must run
  here and name the release. A forged, wrong-platform or cut-short download never replaces what
  cron runs, and a downloaded binary is only run once it is proven.
- **Build provenance:** `release.yml` attests its binaries (Sigstore: one statement naming
  each file and its SHA-256, signed with a certificate GitHub Actions' OIDC token got for the
  run, logged in Rekor). `update` reads the attestation from GitHub's API (anonymous) and
  verifies it here, natively, with no `gh` and no login: the signature; the certificate's chain
  to Sigstore's public-good root (compiled in) and its certificate-transparency timestamp; the
  Rekor entry (inclusion proof, checkpoint, signed timestamp); SLSA provenance naming *both*
  downloaded binaries with their digests (so they were built together: a genuine `arena-tui`
  from an older release can't be slipped in); and who signed: `.github/workflows/release.yml`,
  started by CI's success (`workflow_run`), on `refs/heads/master` of `nickypro/arena-infra-rs`
  (GitHub's id 1368824951), on a GitHub-hosted runner. It says which run built them.
  `SHA256SUMS` alone comes from the same release, so it can't prove that. No attestation, or
  none that verifies: nothing is installed (nor run), and it says why. The new `arena` must
  also be the release's version before it runs: the statement names a `VERSION` file holding
  it (releases from 2026-10-28 on), or, for older ones, the verified bytes carry its
  `--version` line. An older genuine build served under a newer tag is refused there.
- **`--skip-attestation <sha256>,<sha256>`:** this build doesn't check provenance; it installs
  only binaries whose SHA-256s are the two given (`arena`'s and `arena-tui`'s, either order),
  and says so loudly. `SHA256SUMS` is still checked. It's for one case: a genuine release this
  build can't verify, because Sigstore changed its keys or GitHub its attestation format after
  this build was made. Verify this platform's two binaries elsewhere, with a current `gh`
  (prod's 2.45 has no `gh attestation`), then pass their digests:

  ```bash
  # any machine with a current gh; the release's binaries for the box's platform (prod: aarch64)
  t=aarch64-unknown-linux-gnu; tag=vX.Y.YYMMDD.HHMM   # the release's tag (`arena update --check` names it)
  gh release download $tag --repo nickypro/arena-infra-rs -p arena-$t -p arena-tui-$t
  for f in arena-$t arena-tui-$t; do   # both must print the same run, or don't go on
    gh attestation verify $f --repo nickypro/arena-infra-rs --deny-self-hosted-runners \
      --cert-identity https://github.com/nickypro/arena-infra-rs/.github/workflows/release.yml@refs/heads/master \
      --source-ref refs/heads/master --format json --jq '.[0].verificationResult.signature.certificate.runInvocationURI'
  done
  sha256sum arena-$t arena-tui-$t
  # then, on the box:
  sudo arena update --skip-attestation <arena's sha256>,<arena-tui's sha256>
  ```

  It installs, and runs, only those bytes: a release changed in between is refused. The new
  build has the new root, so the next update verifies again. `--check` never reads
  attestations.
- **The swap:** where this `arena` really is (symlinks resolved), the current binaries become
  `arena.prev` / `arena-tui.prev` (an older `.prev` is replaced). Each new one takes the old
  one's mode and owner, and lands by rename, so the path always holds a whole binary.
  Roll back with `mv -f arena.prev arena && mv -f arena-tui.prev arena-tui`.
- **Afterwards:** it lists the `arena-tui` dashboards still running the replaced build, to
  restart. Cron picks the new `arena` up on its next run.
- It needs no config and no GitHub login (the API allows 60 unauthenticated requests an hour;
  an update makes 2: the latest release, and its attestation).

### Command shorthands

Subcommands accept any **unambiguous prefix** (Cisco-style), at every level:

```bash
arena po li         # == arena pods list
arena tui           # launch the dashboard
arena co c          # == arena config check
arena po bac         # == arena pods backup (prompts to confirm)
```

An ambiguous prefix errors and lists the candidates — e.g. `arena p` is rejected
because it matches `pods`, `proxy` and `plan` (use `po`/`pr`/`pl`); likewise `c` →
`config`/`cron` (use `co`/`cr`), `arena po l` → `list`/`lock`/`logs`, and `arena po ba` →
`backup`/`baseline`.

`arena --help` and `arena pods --help` list their commands in groups — `pods`: Look, Make,
Change, Move, Save, Stop; the top level: Fleet, Access, Money, Automation, Setup. A test keeps
every visible command in exactly one group. Hidden: `gpus`, RunPod's GPU list as it always printed.
clap's own `help` command isn't listed (`-h`/`--help` are); `arena help pods` still works.

Every command's help is compact: its options one per line, no blank line between them. clap used its
"long" layout for `--help` whenever any text had a second paragraph — every option's help on a line
of its own and a blank line after each (`offers --help` was 50 lines for 12 options). So arena moves
the long texts where the short layout prints them: a command's description above its usage, an
option's extra paragraphs onto the lines under it (wrapped to end by column 100; clap wraps nothing without
its `wrap_help` feature). `-h`, `--help` and `arena help <cmd>` print the same page, with every word
they had (a test renders every command's help three ways and checks both).

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
  runpod|vast|hetzner|shadeform` (not `--provider`, which picks where `create`/`up` make pods).

**Typos fail loudly**: a target *or* `--exclude` that matches no pod is an error naming it
and the closest pod names, and a narrowed selection (names/filters) that ends up empty is an
error saying how each step narrowed it — nothing is touched. Targets + `--all` together is
refused, and so is an empty argument (`""`, `" "`, `","` — e.g. an unset `$POD` in a
script), which never means "the whole fleet". Dry-runs and prompts list the resolved pods. `--include X` is still accepted as an
old spelling of a target (`stop --all --include X` keeps meaning "only X").

| Command | Targets | Nothing given | Execution |
| --- | --- | --- | --- |
| `run` | `-t <targets>` (the command is positional) | every pod | parallel |
| `test`, `test --deep` | positional | this cohort's pods with an SSH endpoint, as `backup` (`--include-non-cohort`: every pod) | parallel |
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

Any key in `config.env` but the file-only `REAP_*` ones can be overridden by an **environment
variable of the same name** (env wins; it can't introduce brand-new keys — except a short allowlist such as
`ARENA_START_DATE`, `SSH_PROXY_RELOAD_CMD`, `RUNPOD_API`, `DISCARD_MAX_GB`, `CONTROL_MIN_FREE_GB`, `NEAR`/`NEAR_MAX_EXTRA_PCT`, `CLAUDE_TOKENS_FILE` and the `ASK_<AI>_MODEL`/`_EFFORT` keys, e.g. `RUNPOD_API=v1 arena pods
list` to use the retiring RunPod v1 backend for one command, until 2026-11-16; v2 is the default). This is how you point at an SSH
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

### What works where

Not every operation works on every provider. One table in `arena-core`
(`crates/core/src/capability.rs`) says which, and everything reads it:

- **Commands** check every pod they were given before they ask anything or touch anything. A pod
  whose provider can't do it is named, with why and what to do instead. A command that acts on
  all of its pods or none (`reimage`, `rename`, `terminate`, `replace`, `migrate`) refuses the
  whole selection, and says how to leave those pods out (`--exclude`). A batch that already
  skipped pods it can't act on (`stop`, `start`, `lock`/`unlock`) still does the rest, names the
  skipped ones and exits non-zero.
- **Create flags** (`pods create`/`up`, and `replace`/`migrate copy` where they take the flag) that
  the provider would ignore are refused before anything is listed, priced or created: `--disk`
  on Hetzner, `--bootstrap` on Vast, `--volume` on Vast. `--volume 0` means "no volume", so it is
  accepted everywhere. Config values (`DISK_GB`, `VOLUME_GB`, …) still apply only where the
  provider uses them.
- **The dashboard** greys out what the pod's provider can't do, with the reason, and refuses
  the pick with the full sentence. Its add-pod form shows (and sends) only the fields the
  provider's create takes.
- **Commands that only reach a pod over SSH** work on every provider: setup, backup, pull, run,
  test, restore, cp, set-branch, init-branches, copy-keys and `claude assign`/`push`/`off`.

RunPod is REST v2, the default. RunPod v1 is `RUNPOD_API=v1`.

<!-- capability matrix: generated from crates/core/src/capability.rs (its module doc says how) — don't edit by hand -->
| | RunPod | RunPod v1 | Vast | Hetzner | Shadeform |
|---|:-:|:-:|:-:|:-:|:-:|
| `pods start` | ✓ | ✓ | ✓ | ✓ | ✗ [1] |
| `pods stop` | ✓ [2] | ✓ [2] | ✓ [3] | ✓ [4] | ✗ [5] |
| `pods restart` | ✓ [6] | ✓ [7] | ✓ [8] | ✓ [9] | ✓ [10] |
| `pods terminate` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `pods rename` | ✓ | ✓ | ✓ | ✓ [11] | ✓ [12] |
| `pods reimage` | ✓ | ✓ | ✗ [13] | ✗ [14] | ✗ [15] |
| `pods lock / unlock` | ✓ | ✗ [16] | ✗ [17] | ✗ [18] | ✗ [19] |
| `pods replace` | ✓ | ✓ | ✗ [20] | ✗ [21] | ✓ [22] |
| `pods migrate` | ✓ | ✓ | ✗ [23] | ✗ [24] | ✗ [25] |
| setup's SSH key repair (via the API) | ✗ [26] | ✗ [26] | ✓ | ✗ [27] | ✗ [28] |
| the auto-reaper | ✓ | ✗ [29] | ✗ [30] | ✓ | ✗ [31] |
| **create flags** (`pods create`/`up`; `replace` and `migrate copy` take --gpu, --gpus, --cloud, --disk, --volume, --image, --max-price) | | | | | |
| `--gpu / --gpus` | ✓ | ✓ | ✓ | ✗ [32] | ✓ [33] |
| `--server-type` | ✗ [34] | ✗ [34] | ✗ [34] | ✓ | ✗ [34] |
| `--cloud` | ✓ [35] | ✓ [35] | ✓ [36] | ✗ [37] | ✗ [38] |
| `--disk` | ✓ | ✓ | ✓ [39] | ✗ [40] | ✗ [41] |
| `--volume` | ✓ [42] | ✓ [42] | ✗ [43] | ✗ [44] | ✗ [45] |
| `--image` | ✓ | ✓ | ✓ | ✓ [46] | ✗ [47] |
| `--bootstrap` | ✓ | ✓ | ✗ [48] | ✗ [49] | ✗ [50] |
| `--max-price` | ✓ | ✓ | ✓ | ✗ [51] | ✓ [52] |
| `--api-json` | ✓ | ✓ | ✓ | ✓ | ✓ |

1. `pods start` on Shadeform: no — Shadeform has no stop: an instance bills until it's deleted, idle or not. Instead: nothing is ever stopped there, so there's nothing to start.
2. `pods stop` on RunPod, RunPod v1: a stopped pod keeps no container disk: only a /workspace volume survives (`--wipe-ok` gate).
3. `pods stop` on Vast: may lose the container disk (unverified): gated like a wipe.
4. `pods stop` on Hetzner: keeps the disk: a graceful shutdown, powered off after 90 s if it doesn't.
5. `pods stop` on Shadeform: no — Shadeform has no stop: an instance bills until it's deleted, idle or not. Instead: back it up (`arena pods backup <name>`), then `arena pods terminate <name>`.
6. `pods restart` on RunPod: resets the container to its image: only a /workspace volume survives (`--wipe-ok` gate).
7. `pods restart` on RunPod v1: may reset the container (seen keeping it): gated like a wipe.
8. `pods restart` on Vast: may reset the container (a stop + start, unverified): gated like a wipe.
9. `pods restart` on Hetzner: keeps the disk: a hard reset of the VM.
10. `pods restart` on Shadeform: may reset the instance (unverified): gated like a wipe.
11. `pods rename` on Hetzner: also sets the VM's hostname.
12. `pods rename` on Shadeform: the new name must keep the `<prefix>-` prefix.
13. `pods reimage` on Vast: no — Vast can't swap an instance's image in place. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider vast pods up <name> --volume 0` (its --gpu) and `arena pods restore <name>`.
14. `pods reimage` on Hetzner: no — arena can't reimage a Hetzner VM yet (Hetzner's rebuild action could). Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider hetzner pods up <name> --volume 0` (its --server-type) and `arena pods restore <name>`.
15. `pods reimage` on Shadeform: no — Shadeform can't swap an instance's image in place. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider shadeform pods up <name> --volume 0` (its --gpu) and `arena pods restore <name>`.
16. `pods lock / unlock` on RunPod v1: no — only RunPod REST v2 can lock a pod. Instead: remove RUNPOD_API=v1 (v2 is the default).
17. `pods lock / unlock` on Vast: no — Vast can't lock an instance. Nothing to use instead.
18. `pods lock / unlock` on Hetzner: no — arena can't lock a Hetzner VM yet (Hetzner's delete/rebuild protection could). Nothing to use instead.
19. `pods lock / unlock` on Shadeform: no — Shadeform can't lock an instance. Nothing to use instead.
20. `pods replace` on Vast: no — arena can't read a Vast instance's spec back yet. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider vast pods up <name> --volume 0` (its --gpu) and `arena pods restore <name>`.
21. `pods replace` on Hetzner: no — arena can't read a Hetzner VM's spec back yet. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider hetzner pods up <name> --volume 0` (its --server-type) and `arena pods restore <name>`.
22. `pods replace` on Shadeform: not live-tested.
23. `pods migrate` on Vast: no — arena can't read a Vast instance's spec back yet. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider vast pods up <name> --volume 0` (its --gpu) and `arena pods restore <name>`.
24. `pods migrate` on Hetzner: no — arena can't read a Hetzner VM's spec back yet. Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider hetzner pods up <name> --volume 0` (its --server-type) and `arena pods restore <name>`.
25. `pods migrate` on Shadeform: no — the cutover's proxy check reads RunPod's pod id, so it can't pass on Shadeform (it reverts itself). Instead: `arena pods replace <name>`.
26. setup's SSH key repair (via the API) on RunPod, RunPod v1: no — RunPod authorizes keys only at create. Instead: `arena pods replace <name>` (a new pod gets today's keys).
27. setup's SSH key repair (via the API) on Hetzner: no — Hetzner authorizes keys only at create (cloud-init). Instead: back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena --provider hetzner pods up <name> --volume 0` (its --server-type) and `arena pods restore <name>`.
28. setup's SSH key repair (via the API) on Shadeform: no — Shadeform authorizes keys only at create (its bootstrap script). Instead: `arena pods replace <name>` (a new pod gets today's keys).
29. the auto-reaper on RunPod v1: no — the reaper acts on RunPod pods only through REST v2 (their lock state). Instead: remove RUNPOD_API=v1 (v2 is the default).
30. the auto-reaper on Vast: no — the reaper never terminates a Vast instance: no lock state, no verified identity. Nothing to use instead.
31. the auto-reaper on Shadeform: no — the reaper doesn't read a Shadeform instance's identity (its create nonce) yet. Nothing to use instead.
32. `--gpu / --gpus` on Hetzner: no — Hetzner creates CPU VMs sized by server type. Instead: pass --server-type (e.g. cx23, cpx42; `arena offers hetzner` lists them) or set HETZNER_SERVER_TYPE.
33. `--gpu / --gpus` on Shadeform: each GPU type must be one Shadeform sells.
34. `--server-type` on RunPod, RunPod v1, Vast, Shadeform: no — only Hetzner VMs are sized by server type. Instead: pass --provider hetzner, or size a GPU pod with --gpu / --gpus.
35. `--cloud` on RunPod, RunPod v1: COMMUNITY or SECURE.
36. `--cloud` on Vast: host classes: datacenter, verified, unverified.
37. `--cloud` on Hetzner: no — Hetzner has no tiers. Instead: HETZNER_LOCATION picks where it runs.
38. `--cloud` on Shadeform: no — Shadeform picks the cheapest cloud in stock. Instead: set SHADEFORM_CLOUDS.
39. `--disk` on Vast: the offer's disk is at least this.
40. `--disk` on Hetzner: no — a Hetzner VM's disk comes with its server type. Instead: pick a bigger --server-type (`arena offers hetzner`).
41. `--disk` on Shadeform: no — a Shadeform instance's disk comes with its type. Nothing to use instead.
42. `--volume` on RunPod, RunPod v1: a /workspace volume: what survives a stop, restart or reimage.
43. `--volume` on Vast: no — arena's Vast create takes no volume. Instead: work survives through `arena pods backup` (and the archive).
44. `--volume` on Hetzner: no — arena can't attach a volume to a Hetzner VM yet (a VM keeps its disk across a stop and a restart). Instead: work survives through `arena pods backup` (and the archive).
45. `--volume` on Shadeform: no — Shadeform's CUDA-13 cloud has no volumes. Instead: work survives through `arena pods backup` (and the archive).
46. `--image` on Hetzner: the VM image: `arena` (the newest arena-built snapshot), a snapshot id or a name like ubuntu-24.04.
47. `--image` on Shadeform: no — Shadeform picks the OS image by the CUDA floor. Nothing to use instead.
48. `--bootstrap` on Vast: no — not needed on Vast: its launcher brings up SSH on any image. Instead: leave it out.
49. `--bootstrap` on Hetzner: no — a Hetzner VM runs no container start script: setup installs what it needs. Instead: leave it out.
50. `--bootstrap` on Shadeform: no — Shadeform instances come up through arena's own bootstrap script. Instead: leave it out.
51. `--max-price` on Hetzner: no — Hetzner quotes no price before a create. Instead: `arena offers hetzner` lists each server type's price: pick the --server-type.
52. `--max-price` on Shadeform: capped at SHADEFORM_MAX_PRICE too.
<!-- end of the capability matrix -->

### RunPod (REST v2, the default)

`provider::runpod_v2` — RunPod REST **v2** backend (`api.runpod.io/v2`), the **default**:
`RUNPOD_API` unset or empty means v2 (`v1`/`v2` in any case select one; any other value is an error,
which quotes the value only if it's short and version-like — never a pasted key; `config check`
shows which is active, `(default)` when unset).

Why the default: v1 retires on 2026-11-16, and v1 can only ask for hosts on CUDA ≤ 13.0 (below),
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
a proxy-only host (live: two 4090s did, and a 3070 on 2026-10-30): `up` calls it a host failure
after `--endpoint-timeout` (5 minutes) and replaces it, with or without `--check`; SECURE pods
usually have a public IP.

Neither API reports a pod's CUDA constraint, so `replace`/`migrate copy` re-apply the configured one
to the replacement (as `create` does).

### RunPod rate limits

Since 2026-10-27 every RunPod public API is rate-limited (RunPod's service email, 2026-10-28). REST
v1 is capped at 180/min, 3,600/h and 43,200/day, halved on Nov 2 and again on Nov 9, until its sunset.
The v2 management API gives each user **one quota per window** (minute, hour, day), every request
counting against all three, and **every process using the key shares it**: prod's dashboards and
its backup cron draw on one quota. The quotas aren't published; every v2 answer carries them:
`RateLimit-Policy: "minute";q=…;w=60, "hour";q=…;w=3600, "day";q=…;w=86400` (q requests per w
seconds) and `RateLimit: "minute";r=…;t=…, …` (r left, t seconds to that window's reset). A refusal
is `429` with `Retry-After` (seconds) and a problem+json `detail` naming the window. No headers
means no limit.

Every provider request goes through one path (`http::exchange`, `ratelimit`):
- Before a request is sent, a quota that is **held** — a 429's wait, or a window with `r` ≤ 1 until
  its reset (the last request is left to the other processes on the key) — is waited out. If the
  caller can't wait that long, the request fails at once, rate-limited and not sent:
  `list pods: not sent — RunPod REST v2 rate limit (the minute window is spent): next request in 12 s`.
  A dashboard call waits at most 2 s, a listing at most its 60 s bound, and anything else at most
  65 s (a minute window's reset). An hour or day window that is spent fails at once.
- A **429** is retried for any method (it is refused before processing, so a create is safe to
  send again) after `Retry-After` (else Vast's body `retry_after`, else the reset of a window the
  answer says is spent, else 1, 2, 4… s) plus up to 1 s of jitter, at most 3 times. Then the error
  says so: `… HTTP 429 …: rate limit exceeded for the minute window — RunPod REST v2 rate limit:
  still refused after 3 retries; next request in 12 s`.
- A **5xx or a network error** is resent only for a RunPod **read** (GET), twice, after 1 s and
  2 s. A POST/PATCH/DELETE that got one may have done something, so it is the caller's (a create
  says "may have been created", and is never resent). Other providers' requests aren't resent
  here: their callers have their own rules.
- RunPod's three APIs (REST v2, REST v1, GraphQL) are separate quotas here, as are two keys.
  Nothing says the APIs share one. The commands' own retries (`retry` module) still apply on top,
  each attempt behind the same hold.

`arena api get runpod pods --limits` shows what RunPod reported (below); the dashboard's footer
shows the request rate (`ok · API 13/min`), its info panel what each quota has left this minute.

### RunPod REST v1 (retiring)

`provider::runpod` — RunPod REST **v1** backend: list / create / stop / terminate. RunPod sunsets
REST v1 on **2026-11-16**; used only with an explicit `RUNPOD_API=v1` (which `config check` warns
about, with the date and `remove RUNPOD_API=v1 (v2 is the default)`). From then every v1 request
answers `410 Gone`, which arena reports as a config error: `list pods: RunPod REST v1 is gone (HTTP
410 — RunPod sunset it on 2026-11-16): remove RUNPOD_API=v1 (v2 is the default)`.

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

**Host classes are Vast's `--cloud` tiers**, as `COMMUNITY`/`SECURE` are RunPod's (`vast::Host`, from
each offer's own fields, read-only 2026-10-29): `datacenter` (verified, and `hosting_type` 1 — the
vast CLI's `datacenter` filter), `verified` (`verification` is `verified`), `unverified` (anything
else, `deverified` included — a datacenter host Vast doesn't call verified too). A tier is the least
a create accepts, nested: `--cloud datacenter` rents verified datacenter hosts only, `verified` any
verified host (datacenter ones count as verified), `unverified` any host — the same opt-in as `VAST_ALLOW_UNVERIFIED=1`, for one run. No `--cloud`, or a
RunPod tier (config `CLOUD_TYPE` usually holds `COMMUNITY`): the default, verified, or any host with
`VAST_ALLOW_UNVERIFIED=1` — what Vast rented before there were tiers. A list (`--cloud
datacenter,verified`) is one option per tier through the same planner as RunPod's, each priced by its
own search (the search filters `hosting_type`/`verified` server-side, and every offer's class is
checked again before renting), so `up --provider vast --cloud datacenter` rents only datacenter hosts.
Anything else is refused before a search: ``--cloud `cheap`: Vast's cloud types are datacenter,
verified and unverified``.

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
- The account's limit — Hetzner's 403 `resource_limit_exceeded`, e.g. `shared core limit
  exceeded` or `Primary IP limit exceeded` — reads as that, not as a bad token: `Hetzner refused:
  the account's resource limit is reached (…; limit: primary_ip_limit) — delete a primary IP
  nothing needs (…), or ask Hetzner to raise the limit (Hetzner Console: Limits tab → Request
  change → Limit increase)`. What it suggests follows the limit Hetzner names: a smaller type or a
  server for cores, a server for the number of servers, a primary or floating IP, a volume, a
  snapshot. A limit counts
  every project of the Hetzner account, so a project with no server can still hit it (live
  2026-10-30). `create`/`up` keep what they made and stop there (the names after it would hit the
  same limit); it isn't waited out (`--keep-trying` waits on capacity only); with several
  `--gpu`/`--cloud` options only that option is skipped. Hetzner's 403 `maintenance` says to try
  later. Only a 401 or a 403 `forbidden` reads as "authentication failed".
- One location (`nbg1`) pins creates there, with no lookup. A list (`nbg1,fsn1`) is tried in its
  order, and `nbg1,any` tries nbg1, then the rest.
- An `--api-json` `datacenter` is one create where it says.

VMs come up on a real public IP with SSH on `:22`, so they use the same proxy/`pods up` flow as GPU
providers. Root lets in the project's key (`HETZNER_SSH_KEY`, else the one named after the prefix)
and, by cloud-init (the create's `user_data`), every machine's keys — the shared, deploy and admin
keys' public halves, as RunPod, Vast and Shadeform pods get them at create; an `--api-json`
`user_data` replaces that one (setup still authorizes them). A refused create is explained from the project (read-only `GET /ssh_keys` + `GET
/server_types`): a missing SSH key names the project's keys and what to set or upload; a type the
location doesn't sell lists the ones it does.

### Hetzner: pre-built images

A RunPod or Vast pod starts from the cohort's Docker image with the env in it. A Hetzner VM starts as
bare Ubuntu, and setup builds everything (packages, docker, uv, ~4 GB of CPU wheels): about 10
minutes a VM. From an **arena image** — a snapshot of a VM that setup built, with everything of that
machine's scrubbed off — setup takes about a minute.

```bash
arena image build --dry-run      # the plan: VM type, what's scrubbed, cost
arena image build                # ~20 min; asks first (-y: doesn't); --server-type picks the bake VM
arena image list                 # this cohort's images; which one `arena` picks; are they current?
arena image prune [--keep 1]     # delete older ones (and any build VM left behind); asks first
arena image prune --keep 0       # the programme's end: every one of this cohort's images
```

Then `HETZNER_IMAGE=arena` in the config (or `--image arena` on `pods create` / `pods up`, for one
run): the newest of this cohort's arena images that fits the server type, else `ubuntu-24.04` with a
one-line note. `up`/`create` print which (`image arena → snapshot 1234 (devtest-image-261029-1542,
built 2026-10-29, 2h old, commit 3eed962b9bd8)`). A snapshot id or a system image name works as
before.

**What `image build` does.** It creates `<prefix>-image-<yymmdd-hhmm>` from Ubuntu — by default the
smallest-disk type of `HETZNER_SERVER_TYPE`'s architecture with at least 4 GB of memory (a cx23:
40 GB) — and runs `pods setup`'s steps on it, without any token and without swap (a swap file
holds pages of the VM's memory; the VMs made from the image make their own). Then it scrubs it: the deploy key,
every file in `~/.ssh` but `known_hosts` (the authorized keys, the github/fleet/archive blocks — the
new server's own key is written by cloud-init at its first boot), the token exports in the rc files,
`~/.claude.json`, `~/.claude`, Codex's `auth.json`, the Hugging Face token, git/gh credentials,
shell history, `~/.name` and the MOTD's name, `~/.arena`, uv's and pip's caches, the swap file and
its fstab line, `/etc/machine-id` (emptied: systemd makes a new one), the SSH host keys and
`cloud-init clean --logs` (so the image boots as a new instance and cloud-init makes new host keys
and sets the new hostname), `/tmp` and the logs. A swap that's on and can't be turned off and
deleted stops the bake. Then it **scans** the homes and `/etc` for the
backup's secret patterns (`SECRET_PATTERNS`: OpenRouter, Anthropic, OpenAI, Hugging Face, GitHub,
Google, Groq and AWS keys) and private key files (a PEM header on a line of its own), skipping the
venv (third-party packages) and `.git`. Any hit and it **refuses to snapshot**, naming the files —
never what matched; a scan that couldn't read something refuses too. Last, it zeroes the disk's
free space (a file of zeros until the disk is full, then gone, then `fstrim`), so what was deleted
doesn't stay in free blocks the snapshot copies. (The deploy key and the broadcast tokens are on
every VM anyway — setup and `copy-keys` put them there — so this is about the bake VM's own
identity, its history and its logs.) Then it powers the VM off and snapshots it, labelled
`arena-image=1`, `arena-prefix`, `built` (unix), `repo` (`owner.name`, for people) and `repo-id`
(a hash of `owner/name`, what `arena` matches on), `commit` (the checkout's), `req` (a hash of the
repo's `install_uv.sh`, `requirements.txt` and `pyproject.toml`: what the env install read),
`arch`, `disk` and the bake's own id. The VM is deleted whatever happens — a failed setup, a scan
hit, a failed snapshot, Ctrl+C. A create or snapshot whose answer never came (a dropped
connection, Ctrl+C mid-request) is looked for by the bake's id for a minute (never by name: two
bakes in the same minute share one) and deleted; one that doesn't turn up is said loudly, with the
label to look for — never "nothing was made". A create Hetzner refused (4xx) made nothing. A delete
that keeps failing is printed loudly with the VM's id. The zeroing is flushed to the disk before
its file goes, and anything but a full disk ending it (or too little written) stops the bake, as
does a `/proc/swaps` it can't read or a swap still on after `swapoff`. A build VM is never part of the fleet
(no backup, proxy, reap or `pods` command sees it); `teardown --check` lists one still there, and
`image prune` deletes one over 3 hours old.

**A VM from an image** gets the usual setup, with two differences. Before the setup script, its
checkout is updated as an image-based pod's is (a clean default branch fast-forwards to origin's;
anything of the participant's is left alone). Then, on the first setup only, the script compares the
requirements the image's env was built from with the repo's now: the same, and the env is kept;
different, and it's rebuilt as on a fresh VM (~10 min). The per-machine steps run as usual — swap,
`~/.name`, the MOTD, the keys and tokens (`copy-keys`). The ✓ line says which image it came from and
how old it is: `image: devtest-image-261029-1542, built 2026-10-29 (2 h ago), commit 3eed962b9bd8;
requirements unchanged, env kept`.

**When to rebuild.** When the course repo's requirements change (`image list`'s REQ says `differ`,
and `config check` says so): every VM from the old image would reinstall. When the image is over 14
days old (`config check` notes it): VMs fast-forward further at setup, and the system packages age.
After a change to the setup script. Build, check `image list`, then `image prune`: it keeps the
newest `--keep` (default 1), the one `arena` picks now and one `HETZNER_IMAGE` names by id — but
`--keep 0` deletes them all (it warns when `HETZNER_IMAGE` names one).

**Scope and the rules Hetzner sets** (checked 2026-10-29):
- An image belongs to the **Hetzner project** it was made in. The sandbox project and prod's don't
  see each other's: build one in each. Within a project, `arena` only picks images labelled with
  this cohort's prefix and repo; a snapshot made by hand is never picked, pruned or counted as ours.
- **Location:** none. An image has no location in the API, and one image made servers in hel1 and
  nbg1, so the create's location walk is unchanged.
- **Architecture:** an x86 image boots only x86 types (`cax*` are arm).
- **Disk:** a server's disk must be at least the image's `disk_size` — the disk of the VM it was
  taken from. Baked on a cx23 (40 GB), it boots on cx23, cx33 and every larger type.
- **Cost:** storage only, on the image's size (the data, ~6 GB), not its disk: `GET /pricing`'s
  `image.price_per_gb_month` — $0.0199 per GB-month on the sandbox account (gross, in the account's
  currency), so ≈ $0.10 a month an image (5.2 GB). The bake VM: ~20 minutes of a cx23, about a cent.
  `teardown --check` lists the images as **kept** (◷: on purpose, never against the check) with
  their cost; at the programme's end `arena image prune --keep 0` deletes them.

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

Design and recorded fixtures: `crates/core/src/provider/shadeform.rs`'s module doc,
`crates/core/src/provider/fixtures/shadeform/`.

## Library modules (`arena-core`)

A few of them; every module, with what it does and what is pure, is in
[ARCHITECTURE.md](../ARCHITECTURE.md#core-modules).

- `config` — tolerant parser for the existing `config.env` (incl. the `MACHINE_NAME_LIST` bash
  array), so this tooling reads the *same* config as the legacy bash/python scripts.
- `provider::Provider` — the single trait every backend implements.

- `naming` — next-free machine-name allocation, mirroring the legacy logic.
- `snapshot` — one read-only picture of the fleet (`FleetSnapshot`, built by one pure `build` that
  the CLI and the TUI share), the **health cache** of the last deep check per pod, and the
  allowlisted `PublicSnapshot` for the web page.
- `teardown` — the pure judge behind `teardown --check`: listings, volumes, keys, cron/`at` and
  proxy inputs in, a `✓ ✗ ? –` checklist with fix commands out (unknown ≠ empty).
- `spend` — what each machine instance cost and who it was for: the `spend.json` ledger folded
  from listings and lifecycle events, $/h segments and flagged estimates, AI keys per owner, the
  report per owner or instance (RunPod corrected from its per-pod billing), its renderers.
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
Hetzner, Shadeform opt-in), commit/backup, the GPU/progress dashboard, and proxy/port-forwarding.

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

`list --json` emits the pods incl. `gpu_count`/`cost_per_hr`/`maintenance`, and `location` when
the provider says where a pod runs (RunPod's data centre — a COMMUNITY pod has none —, Vast's
`geolocation`, Hetzner's location, Shadeform's cloud and region). It times nothing: the RTT is the
dashboard's.

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

**`reimage`** (RunPod only: a selection with a pod on another provider is refused whole, before
the wipe gate and the confirm — [What works where](#what-works-where)) gets the same
`--wipe-ok` gate as restart/stop (a reimage is a fresh container; the dry run says when it would be
refused) and the same come-back: settled endpoint, **setup again** (`--no-setup`), proxy sync.

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

`rename <old> <new>` / `rename --from-prefix <p>` renames the pod on any provider (RunPod's name,
Vast's label, the Hetzner server's name — its labels untouched —, Shadeform's name, which must keep
the `<prefix>-`; metadata only, no restart; a name the provider refuses is explained) and then
brings along what's keyed by the name: rewrites `~/.name` over SSH (setup's exact `export
MACHINE_NAME='<short>'` line; an unreachable pod is reported with a `pods setup <name>` hint, never
fatal), on a Hetzner VM also its hostname (best-effort; kept across a reboot; `pods setup` sets it
when a rename couldn't), moves the machine's row in the keys CSV (the per-cohort file the symlink points at — or,
after a prefix change, *into* the current prefix's file, repointing the symlink and carrying the
other live pods' rows, so the next `keys gen` doesn't strand them) and renames its key on OpenRouter
in place (`PATCH /keys/{hash}` `name`; skipped quietly with no CSV / no provisioning key; a new name
that already has its own row or key is left alone and warned about), then syncs the proxy.

A RunPod or Vast container's `MACHINE_NAME` env var keeps the old name (env only changes with a
reimage) — the output says so; bare VMs (Hetzner, Shadeform) have no such variable. `--dry-run`
lists every step.

### `pods lock` / `pods unlock`

`pods lock [targets]` / `pods unlock [targets]` (targets or `--all`; RunPod on REST v2, the default)
— a **locked** pod is refused stop, restart and terminate by RunPod itself, from **any** client
(this tool, scripts, RunPod's web console; live-verified 2026-10-08) — the protection for a running
cohort. Setup, backups, `run` and SSH are unaffected. Already-locked pods are left alone; a pod on a
backend that can't lock (`RUNPOD_API=v1`, Vast, Hetzner, Shadeform) is reported with why (on v1: `only
RunPod REST v2 can lock a pod — remove RUNPOD_API=v1 (v2 is the default)`) and makes the exit non-zero.

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
an exact id (any case), the catalog's display name as `arena offers runpod` prints it (`RTX PRO 6000`, `B300
MIG 34GB` — checked before the aliases, so `RTX PRO 6000` is the Server Edition, as listed), an
alias (`3070`, `4080super`, `L4`, `2000ada`, `A4500`, …) or a unique catalog short name (`RTX 3070`,
`H100 SXM`) — must be in RunPod's live GPU catalog (`GET /v2/catalog/gpus` on v2, the default;
GraphQL on `RUNPOD_API=v1`), else the command stops before anything is created: ``--gpu: `3070x`
isn't a RunPod GPU type — did you mean `NVIDIA GeForce RTX 3070` (RTX 3070)?``.

If the catalog can't be fetched it warns and passes the flag through unchecked.

**Types the public catalog doesn't list.** RunPod rents some it lists nowhere: ARENA had a `NVIDIA B300
SXM6 AC MIG 1g.34gb` pod (rented at $0.50/h) that neither v2's catalog nor GraphQL `gpuTypes` nor the
website lists, on ARENA's account or the sandbox's (2026-10-29) — so no catalog can teach it. `--gpu`
takes such a type when arena has *seen* it, by its exact id (any case) or an alias that maps to it:
arena's own list (`gpu::UNLISTED`: that B300 slice, also `B300 MIG`, `B300-MIG-34`), config
`RUNPOD_EXTRA_GPU_TYPES` (exact ids, comma-separated; also from the environment for one run), or a
pod on the account (one `GET /v2/pods` walk, only when the catalog and the config didn't already
match). It says ``warning: --gpu `…` isn't in RunPod's public catalog — trying it; RunPod's create
decides`` and goes ahead; v2's create schema has no enum for `gpu.id` (its OpenAPI document,
2026-10-29), so only a create tells. A typo is still refused, the seen types among the suggestions.
With no catalog price it's unpriced: `--max-price` drops it ("no known price"), as it drops any
option it can't check.

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

Cloud tiers exist on RunPod (`COMMUNITY`/`SECURE`) and Vast (its host classes: `datacenter`,
`verified`, `unverified` — [Vast.ai](#vastai)); Hetzner and Shadeform collapse them (Hetzner the GPU
list too), with a note. On RunPod and Vast, options near the proxy come first ([Near the
proxy](#near-the-proxy-near-and-near_max_extra_pct)).

Vast options are priced **live**: one marketplace search per GPU and host class (needs `VAST_API_KEY`), the
cheapest fitting offer's price for the disk asked for — a snapshot, since each create searches again
(never above `--max-price`); a GPU with no offer right now is unpriced, with a note. Hetzner's
option is unpriced.

`--keep-trying` stays single-option (use `--retry-mins`). One `--gpu`, one `--cloud` and no
`--max-price` is exactly the old single-spec path; a list that collapses to one option (`--gpu
A4000,a4000`, `--cloud community,`) takes it too, creating the parsed option (never the raw list
text).

`--dry-run` prints the option table and the names it would attempt; the option order is fixed once
confirmed.

#### Near the proxy: `NEAR` and `NEAR_MAX_EXTRA_PCT`

Every participant's SSH and VS Code traffic goes through the proxy box (on prod: Hetzner nbg1,
Nuremberg), so a pod far from it adds its round trip to every keystroke — a US pod about 100 ms for a
European cohort — to save cents. So on RunPod and Vast, `create`/`up` (and `replace`/`migrate`,
`offers` and the dry-runs, through the same planner) try options **near the proxy first**, unless
they cost more than a set margin over the cheapest acceptable option:

- `NEAR` — `europe` (the default when unset: prod's config predates the key) or `north-america`;
  `any`, `off`, `none` or empty turn it off (options anywhere, cheapest first, as before).
- `NEAR_MAX_EXTRA_PCT` — the margin, default `25` (`25%` reads too). The human's call: latency
  over small savings (25% of an A4000's $0.17/h is 4 cents).

Both can come from the environment for one run (`NEAR=any arena pods up -n 3`) without editing the
shared config; `config check` shows them, defaults marked, and fails a value it can't read.

What "near" is, per provider (live, 2026-10-28):

- **RunPod** — `dataCenterIds` on the create: Europe is every `EU-…`/`EUR-…` data centre
  (`EU-CZ-1`, `EU-FR-1`, `EU-NL-1`, `EU-RO-1`, `EU-SE-1`, `EUR-IS-1`…`5`, `EUR-NO-1`/`2`; `api get
  runpod catalog/datacenters` lists them with their region). The price is per GPU and tier, so a
  near option costs what the same option anywhere costs. **SECURE only**: COMMUNITY hosts are in no
  data centre (a community pod's `dataCenterId` is null, and a community create pinned to every data
  centre finds no capacity) — a community-only create keeps its single path, and `offers` says why
  nothing is near. With SECURE in `--cloud`, the near option is weighed against the cheapest
  (community) one: a SECURE A4000 ($0.25) is +47% over a community one ($0.17), so it's tried near
  first only with `NEAR_MAX_EXTRA_PCT` ≥ 47. The v2 catalog's per-data-centre listing skips a near
  option the region doesn't offer right now (one create saved), and its stock is the region's.
  `CREATE_EXTRA_JSON` / `--api-json` naming its own `dataCenterIds` turns near off (that pin wins).
- **Vast** — each offer's `geolocation` country (the search filters on it: `"geolocation": {"in":
  […]}`): Europe is the EU, the EEA, Switzerland, the UK, the Balkans, Ukraine and Moldova (not
  Russia, Belarus, Turkey or the Caucasus). A near option is priced at the cheapest near offer and
  capped at the margin — its create searches again and never rents dearer than the plan allowed.
- **Hetzner** already creates in the nearest location (`HETZNER_LOCATION`); **Shadeform** has its
  own region list. Neither reads `NEAR`.

The order: cheapest-first — every near option within the margin (cheapest first among them), then
every option anywhere, as before; `--order listed` — each option near just before it's tried
anywhere, so the GPU order still comes first. The option table gets a WHERE column (`Europe`/`any`)
and a note with the margin and what wasn't tried near, and why. A near option with no capacity falls
through to the next, never a failure — and on RunPod is out **for the rest of the run**: its
anywhere twin covers the same data centres, so a cohort spends one create on a full region, not one
per name and round (RunPod limits requests per key, shared by every dashboard and cron on it). A
10-pod `up` with Europe full: 11 creates; with it off: 10. On Vast it's out for the round, like any
capacity block: its "no capacity" is one market search's answer, and the next round searches again. `--keep-trying` waits on one pool, so it keeps the single
path (anywhere) and says so; `--retry-mins` keeps near.

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
command. Underneath, each request already waits out a 429 and a held quota, and RunPod reads are
resent after a 5xx (see [RunPod rate limits](#runpod-rate-limits)).

Every backend reads a response's **status before its body** (`http` module), so a bad key's `401`
with an HTML or empty body is `Auth` (`… HTTP 401 Unauthorized: …`), never "error decoding response
body".

### `offers`

`offers [PROVIDER|all] [--gpu …] [--cloud …] [--max-price …] [--gpus N] [--server-type T] [--order …]
[-v] [--json]` — read-only. Two views: with no provider, what `create`/`up` would try, on every
provider at once; with one (`runpod`, `vast`, `hetzner`, `shadeform`) or `all`, everything it sells.

**`arena offers`**: one table over the providers with a key in the config, in the fleet's order
(RunPod, Vast, Hetzner): PROVIDER, `#` (the order `create`/`up` would try that provider's options in),
OPTION, CLOUD, SIZE (VRAM per GPU — the live catalog's, else the preset's — or vCPU/RAM for a Hetzner
VM), PER POD (`$`/h; `~` = preset estimate; Hetzner's in `€`, as it bills, never converted), SOURCE
(`live`, `estimate`, or `list` for Hetzner's list price, which placement doesn't use), STOCK, and
WHERE when near the proxy is on (`Europe` for an option tried only there, `any`: [Near the
proxy](#near-the-proxy-near-and-near_max_extra_pct)). Under it, a provider that failed, then one
line: `# = the order `up` tries · -v: 4 notes, 1 over --max-price, shadeform skipped · everything on
sale: `arena offers all``. **`-v`** prints instead what the cap dropped and why, each provider's
notes and the providers left out (the explanations that used to fill four lines under every table
are in `offers --help`). With no key at all, RunPod alone (priced from the presets), as before.

Each provider's rows are made by the functions `create`/`up` run — the `--gpu` check, the request
from the flags and the placement planner — so the order and prices are theirs, and a change to
placement shows here unchanged. `create`/`up` use **one** provider per run (`--provider`, default
`runpod`): `#` orders the options within a provider, never across. `--gpu`, `--cloud`,
`--max-price` and `--server-type` preview a setup other than the config's.

- `--provider X` on the command line: that provider alone (its default, `runpod`, doesn't count —
  clap's value source tells them apart). **Shadeform** shows only this way: it's opt-in overflow,
  never in `up`'s default (with its key set, `-v` says it was skipped). One catalog read prices every
  option at its cheapest in-stock entry (allowed clouds, CUDA floor, `SHADEFORM_MAX_PRICE`); an
  option with nothing in stock is noted and left unpriced.
- **Filters** apply per provider exactly as `create` applies them. A flag that doesn't apply to a
  provider (`--gpu`/`--gpus` on Hetzner, `--server-type` elsewhere) leaves it out (`skipped:`, with
  `-v`) — or, with that provider named, fails as `create` does. A provider whose planning fails (a
  `--gpu` typo, a server type Hetzner doesn't have or sells nowhere `HETZNER_LOCATION` allows) is
  listed as `failed:` while the others still show, and the command exits non-zero; named, it just
  fails. A read that doesn't answer (a catalog, a Vast search) is a note, never a failure, as in
  `create`'s own plan. Flags that apply to no provider in scope (`--server-type` with no Hetzner
  key) are an error: nothing to show. `--cloud` is RunPod's tiers and Vast's host classes
  ([Vast.ai](#vastai)); a RunPod tier on Vast means its default, with a note when it was one of
  several.
- **Hetzner's row** is the VM `up --provider hetzner` would create: `--server-type` over
  `HETZNER_SERVER_TYPE` (default `cx33`), its vCPUs and RAM, and its gross €/h where the create's
  location walk starts (one `GET /server_types`, plus the same nearest-first timing a create does;
  the walk is in a note). Placement itself quotes no price for it, so `--max-price` drops it ("no
  known price"), exactly as `create --provider hetzner --max-price …` would. Without
  `HETZNER_API_KEY`, or if the read fails, the row keeps the type's name, size and price unknown;
  a type the catalog says create would refuse is a failure, not a row.
- `--json` (unchanged, the dashboard's new-pod form reads it): `{"providers": [...], "skipped":
  [{provider, why}], "failed": [{provider, why}]}`. Each provider is its option plan as `create`
  sees it — `provider, gpu_count, order, max_price, options, dropped, notes`, `near` when on — with
  `vram_gb` added to each option, and on Hetzner a `server_type` object (`name, cores, cpu_type,
  architecture, memory_gb, disk_gb, eur_per_hour, locations`).

**`arena offers <provider>`**, and **`all`** for every configured provider's (Shadeform left out
unless named, as above; `--all` is still taken, hidden). Each is a heading, a table and a line or
two of notes; `#` marks what `up` with the config (and the flags) would try, in its order — from
the same plan as the default view (a config `up` can't plan with — a `CLOUD_TYPE` the provider
doesn't take — leaves the list unmarked, a note saying why). `--gpu`, `--cloud` and `--max-price`
(per pod of `--gpus`) filter the rows; a row whose price is unknown doesn't pass a cap. Vast
searches only the classes `--cloud` admits, so a failed search can't hide behind another's.

- **RunPod**: every GPU type in its catalog once per tier that sells it (`COMMUNITY`, `SECURE`),
  with that tier's $/h per GPU and stock — so B200/B300 and the MIG slices show, the secure-only ones
  as `SECURE`, whatever `CLOUD_TYPE` says. A type no tier sells now stays as one row, `not sold now`;
  on `RUNPOD_API=v1` a type its create enum lacks says `v1: not creatable`. Then the types the
  catalog lacks that arena has seen (above): `unlisted`, priced only by what a pod of it on the
  account bills (`pod`), else `?`. The last column is the `--gpu` name. RunPod's CPU pods
  (`/v2/catalog/cpus`: `cpu3c/g/m`, `cpu5c/g/m`, secure only, from $0.03 per vCPU-hour) aren't
  listed, and a note says so: v2's create takes `cpu` *instead of* `gpu`, and arena always sends
  `gpu` (an `--api-json` `cpu` would be refused by RunPod: exactly one of the two).
- **Vast**: the GPUs on the market now — three searches, one per host class, each for a pod of
  `--gpus` GPUs with the config's disk and CUDA floor, any card, its 1000 cheapest offers (with
  `--gpu`, one per class and card instead, so a pricier card isn't crowded out; a search that
  filled its 1000 says so) — and per card and class the cheapest offer: $/h per GPU and where it
  is. A class `up` wouldn't rent as configured says `--cloud unverified`. `#` is on the row each
  option tried anywhere would rent: the cheapest of the classes its tier admits (a near option
  rents in its region, which these rows don't show). Each create searches again. Every search
  failed (an expired key): the list failed; some: a note.
- **Hetzner**: every server type on sale somewhere now — vCPUs, RAM, disk, €/h in its cheapest
  location, where it's sold — grouped by family with a line heading each: `cx` (shared x86,
  cost-optimised), `cpx` (shared AMD: the newer `cpx?2` first, then the older, US-only `cpx?1`),
  `cax` (shared Arm), `ccx` (dedicated x86), then any other; by size within a family (vCPUs, then
  RAM: cx23, cx33, cx43). `HETZNER_SERVER_TYPE` (or `--server-type`) is starred, or a line says it's
  on sale nowhere now. `--json` keeps the same order.
- **Shadeform**: a marketplace priced per GPU — a note says to use `offers --provider shadeform
  --gpu …`.

`--json`: `{"catalogs": [{"provider", "gpu_count", "rows": [...], "server_types": [...] (Hetzner
only), "notes": [...]}], "skipped": [{provider, why}], "failed": [{provider, why}]}`. A row is
`{gpu, name, vram_gb, cloud, price_per_gpu, price_source, stock, location?, try_order, note?}`:
`gpu` the `--gpu` name, `cloud` the tier or host class (`null`: unknown or unsold), `price_source`
`live`/`estimate`/`pod`, `try_order` the `#` (`null`: `up` wouldn't try it). A list that can't be
read (no key, an error) is `failed` — the others still show, then the exit is non-zero; named, the
command just fails. **Changed:** `offers --all --json` was `{"runpod": [rows as gpus --json],
"hetzner": [...], "notes": [...]}`; `gpus --json` still prints that RunPod array.

### `pods up`

`pods up -n N` — one-command spin-up: create, then **one independent pipeline per pod** (never a
batch), each started **the moment its create returns** — while placement may still be retrying for
other names under `--retry-mins` (placed pods used to sit idle, billing, until the window ended):
wait for *its* SSH endpoint and sshd answering (`--timeout`, default 600s, per pod) → sync the
proxy (one writer at a time) →
provision it (`--no-setup` skips) → [`--check`: deep check] → copy its API keys (when
`keys/*_api_keys.csv` exist) → [`--lock`: lock it] → `[name] READY after 4m10s — …` or `[name] FAILED <stage>: …`, printed
the moment that pod is done — a slow pod never holds up another.

One fleet listing per `--interval` serves every pod's endpoint wait.

**A pod up without an SSH endpoint is replaced, not left billing** (ops feedback 2026-10-29: a
RUNNING 4090 with no public IP or ports stayed `FAILED endpoint: no SSH endpoint after 600s — left
running` until someone terminated it and ran `up` again, READY in a minute). Up without an endpoint for
`--endpoint-timeout` minutes (default 5, config `ENDPOINT_TIMEOUT_MINS`, 1..120) is a host
failure, with or without `--check`, within `--check-attempts` placements per name (default 2;
the flag works without `--check`): `up` says where it was (the machine and data centre its
provider names), terminates it (and waits until it's gone) and recreates the name with the same
options, the ones that haven't failed first; the last one is terminated too, and the name FAILs at
`endpoint` saying nothing of it is left billing.
- On RunPod "up" means its container is running but reachable through RunPod's SSH proxy only — a
  host with no public IP (`no public endpoint (RunPod ssh proxy only)` in `pods list`). RunPod
  lists a pod `RUNNING` from its create, while its image is still being pulled (live 2026-10-30:
  6½ minutes with no container — `publicIp: ""`, `portMappings: null` on v1 — then proxy-only), so
  the clock doesn't start at the create: a slow pull is no host failure. Elsewhere "up" is a pod
  listed running without an IP/port (Hetzner `running`, Vast `running`, Shadeform `active`).
- It always gets a whole `--endpoint-timeout` without an endpoint before it's judged — past
  `--timeout` if its container came up late (a slow pull onto a host with no public IP is still
  replaced, not left running at `--timeout`). One that `--timeout`'s own last look is the first to
  see up without an endpoint (it came up between two listings) gets it too, once: `[lightning] no
  SSH endpoint after 600s, and only now up without one: giving it its 5m00s (--endpoint-timeout)`.
- Not counted: the time a backend holds an endpoint back on purpose — Shadeform shows an `active`
  instance's IP only 60 s after it went active, so there "active, no IP" gets 60 s more
  (`--endpoint-timeout 1` is two minutes there).
- Nothing is terminated on an old look: before replacing, `up` asks the pod's own backend again.
  An endpoint after all goes on with the pipeline; a pod that isn't up any more (restarting), or
  can't be read just then, is waited for again, its clock started over by the listings after
  that look, to the same `--timeout`.
- **A RunPod pod with no container when `--timeout` ends** — a `--timeout` of 10 minutes or more,
  as the default 600 s — never started its image, prod's case above: a failed placement, terminated
  and its name recreated the same way (`[lightning] still listed RUNNING with no container 10m00s
  after its create (its image never started) — before it came up: a failed placement`). RunPod
  can't tell a pull that will never finish from a slow one; healthy pulls took 1½–7 minutes live.
  With a `--timeout` under 10 minutes such a pod is left running instead, as below.
- Anything else still coming up when `--timeout` ends (Vast `loading`, Hetzner `initializing`, a
  RunPod pod under a short `--timeout`, a pod whose state can't be read just then) is left running
  as before, and the reason says so: `no SSH endpoint after 300s, still listed RUNNING (no
  container yet — its image may still be pulling) — left running, still billing ($0.69/h): `arena
  proxy apply` once it has an endpoint, or `arena pods terminate lightning``. An endpoint that
  came with the deadline goes on with the pipeline.

A listing that momentarily lacks the pod, or a RunPod endpoint lookup that failed, doesn't restart
the clock; a listing that shows it not up (a restart) does. A pod `up` didn't create in this run is
never terminated; Ctrl+C terminates nothing. On
Shadeform a pod listed `error`, or not up within `SHADEFORM_READY_MINS` or `--timeout` (whichever
comes first), is a failed placement the same way — deleted at once and the name tried on the next
option. There that deadline is the end, grace or not (an instance can't be stopped and bills until
deleted): one up without an endpoint is never waited for past it, and one whose last look can't
be read, or shows it not up, is deleted all the same — never left running. That last look comes
first: an endpoint that came just before the deadline goes on, and one deleted elsewhere is only
said.

Ends with the usual one-line proxy sync and a `NAME GPU $/H PROXY PORT HEALTH STATUS READY AFTER`
table (READY AFTER = first create → confirmed ready, replacements included: start participants'
clocks at READY), plus a line per name that failed, warned or was replaced — a requested name that
got no pod is a `FAILED create` row; exits non-zero unless every requested name is READY (and when
nothing was created at all).

A pod that fails is left running, except where `up` replaces or deletes it. A host FAIL under
`--check` is terminated and its name recreated, within `--check-attempts` — the last attempt's pod
is left running for a look. A pod up without an SSH endpoint for `--endpoint-timeout`, or a RunPod
pod with no container at a `--timeout` of 10 min or more, is replaced the same way, and the last
one is terminated too. A Shadeform pod not ready by its deadline is deleted (it bills until then).

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
everything after the create (not with `--check`, `--check-attempts` or `--lock`).

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

**Auto proxy sync.** `pods create`, `up`, `start`, `restart`, `rename`, `reimage`, `terminate` (one
or `--all`), `replace`, `migrate cutover/revert/finish` and the reaper end by re-syncing the proxy through the same merge, from a
fleet-wide listing, and print one line — e.g. `[proxy] after terminate: +0 ~0 -1 =0 (deployed;
removed arena8-apple)` or `… (unchanged)`.

It's best-effort: no proxy configured / no nginx to deploy to → a one-line skip note; a listing or
write error → a warning, never a failed command. `--skip-proxy` (create, start, restart, terminate,
rename, reimage, replace, migrate) opts out.

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
`MACHINE_NAME_LIST` index. Read-only. `--admin`: staff's config with the admin key instead
([below](#staff-on-the-proxy-box-the-admin-key---admin-ssh_admin_config_file)).

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

#### Staff on the proxy box: the admin key (`--admin`, `SSH_ADMIN_CONFIG_FILE`)

`ADMIN_SSH_KEY_PATH` (optional) names a key whose private half **only the proxy / control box
holds**. Every machine authorizes its public half — the create (each provider's; Hetzner's by
cloud-init), `pods setup`, `pods copy-keys`, `reimage`, Vast's key re-attach — cohort pods and `@`
staff boxes alike, and nothing ever copies the private half anywhere (the pods hold the shared and
deploy keys, never this one). arena offers it **first** wherever this user can read it, so arena on
the proxy box gets in even where the cohort key was revoked; elsewhere it isn't offered (sshd's
`MaxAuthTries` counts every key tried).

- **`ssh-config --admin`** prints staff's config: a self-contained `Host` block for every machine
  — this cohort's pods (a parked `-old` twin too) and the `@` staff boxes, named as `pods list`
  names them; nothing else on the account — at its **live endpoint** (never a proxy port: it's used
  on the proxy box itself), `IdentityFile` the admin key, `IdentitiesOnly yes`. A machine without
  an endpoint yet is left out. `--out FILE` also writes it (0600). Refused without
  `ADMIN_SSH_KEY_PATH`.
- **`ssh-config --admin --install [PATH]`** keeps staff's own file (default
  `SSH_ADMIN_CONFIG_FILE`, else `~/.ssh/arena-admin.conf`) and its `Include` first in
  `~/.ssh/config`, exactly as `--install` does for the participants' file (same checks, asks, keeps
  copies). Refused unless this user can read the admin key, and for a key path an ssh config
  can't take as one plain argument (whitespace, quotes, `#$%\`); never the participants' file (its
  path, or a file under their first line). Its first line is its own (`# managed by arena (`arena
  ssh-config --admin`) — …`), so neither sync ever rewrites the other's file. `--admin
  --uninstall` takes its `Include` out.
- **One of the two per user.** Both files name the same hosts, and ssh takes each host's address
  from whichever `Include` comes first (and offers both files' keys): the participants' proxy ports
  would shadow staff's live endpoints, or the other way round. So staff's file isn't installed or
  kept while `SSH_CONFIG_FILE` is set too (the proxy box needs no participants' file), and neither
  file is installed or kept while `~/.ssh/config` pulls in the other — Includes read as ssh reads
  them (relative under `~/.ssh`, quoted, `*`/`?` patterns, nested). One it can't follow (a `${…}` or
  `%` token, a backslash escape, a `[…]` class, braces) refuses an install rather than guesses, and
  stops a sync while the other file exists where arena keeps it. To use the other anyway: `--out
  FILE` and `ssh -F FILE <name>`.
- **With `SSH_ADMIN_CONFIG_FILE` set**, it's kept current like the participants' file: after every
  pod change, on `pods list`, `up` and each dashboard refresh, from a complete listing only — and
  only for a user who can read the admin key (anyone else has no use for it; nothing is said).
  `ADMIN_SSH_KEY_PATH` unset, or the other file in use (above), is a warning line.
- **`config check`**: the admin key's row beside the shared and deploy keys — `·` unset, `✓`
  readable here with its public half, else `⚠` and why: unreadable by this user (fine anywhere but
  the proxy box), no `.pub` and `ssh-keygen -y` fails, the same file or key as
  `SHARED_SSH_KEY_PATH` or `GIT_SSH_KEY_LOCAL` (then participants or every pod hold it), or a mode
  that lets other users at it. With the key or the file set, the `SSH_ADMIN_CONFIG_FILE` row too
  (✗ when the key is unset or the other file is in use; `·` for a user who can't read the key).
- **Rolling it out:** generate it on the proxy box (`ssh-keygen -t ed25519 -N '' -f
  /root/.ssh/arena_admin_key`), `arena config set ADMIN_SSH_KEY_PATH /root/.ssh/arena_admin_key`,
  `arena pods copy-keys --all` (machines that already exist), then `arena ssh-config --admin
  --install` and `arena config set SSH_ADMIN_CONFIG_FILE '~/.ssh/arena-admin.conf'`. New machines
  get it from their create and setup.

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
`~/.ssh/config` + `authorized_keys` (every machine's keys: shared, deploy and the admin key — a
bare VM in a step of its own, `authorize keys`, before its script), **put the repo on the persistent volume** when the pod has one,
point the repo at GitHub, update submodules, write `~/.name`, and export the Hugging Face
**broadcast token** for gated-repo access (via config or `--hf-token`) — else that step is skipped
and it says so.

It also **auto-distributes per-host API keys** if any `keys/*_api_keys.csv` exist (reporting what it
added, or that none are set up), and puts each pod on its own **Claude Code account** with that
account's token ([`claude`](#claude-code-accounts-claude): the least-loaded account for a pod with
none; `--cc-token` puts one token on every pod instead) — to exactly the pods that just provisioned
successfully, never one that failed or timed out — so a `setup` (or `up`, per pod) makes pods fully
ready.

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

**Git identity:** every provider's setup gives the checkout the backup's identity (`Arena
Autocommit <autocommit@arena.education>`, repo-local) for `user.name` / `user.email` when git finds
none anywhere (repo, global, system) — before, a fresh machine had none until its first backup on a
day branch, and a participant's first `git commit` or `git merge` stopped at "Please tell me who you
are". A participant's own is never overridden; one they set globally later takes over at the next
setup (ours, while unchanged, is unset again — a repo-local value would shadow theirs). The backup
sets the same, the same way, so the two never undo each other.

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

Its first step makes **swap**: a `/swapfile` of `VM_SWAP_GB` GB (default 8; `0` = none; at most 64),
turned on, in `/etc/fstab` and with `vm.swappiness = 10`. Without swap, a notebook that loads a model or two past
a cx23's 4 GB (or a cx33's 8 GB) gets its kernel killed; with it, the box slows down instead. The file sits at the
filesystem's top, which the auto-reaper's idle probe doesn't scan, so it never makes a VM look busy. It is never
fatal: with too little disk (the swap must leave 10 GB free), a swap already on that's at least as big (a
provider's), or a `mkswap`/`swapon` that refuses, it's a `WARN` line and no file. A re-run keeps a swap file
that's already on, even at another size (resizing a live one isn't safe under memory pressure); `swapoff
/swapfile && rm /swapfile` and set up again to resize. Shadeform's GPU VMs run the same script, so they get it too;
container pods (RunPod, Vast) can't swap.

On a VM that already has the checkout — one from an arena image ([Hetzner: pre-built
images](#hetzner-pre-built-images)), or a re-run — a **repo update** step runs before the script:
the deploy key is wired to github.com and the checkout updated as an image-based pod's is (a clean
default branch fast-forwards; `--force` resets it). A fresh VM has no checkout yet: the script
clones it. A fresh install records what it was built from (`.venv/.arena-req`: the hash of
`install_uv.sh` and `requirements.txt`).

#### VS Code warm-up

**VS Code warm-up** (last step, on every provider, `up`/`restart`/`replace` included): pre-installs
what a participant's first Remote-SSH connect would otherwise download on the pod — the latest
stable VS Code server for the pod's CPU (x64/arm64, resolved from the update API on the pod;
checksum-verified) in the layout Remote-SSH looks for (`~/.vscode-server/code-<commit>` +
`cli/servers/Stable-<commit>/server`, plus the legacy `bin/<commit>`), the extensions
(`VSCODE_EXTENSIONS`, default Python + Pylance + Jupyter; shared by every server version, so they
help even when a client is another release), and the conda env as `python.defaultInterpreterPath` in
the machine settings (added only if unset; other keys kept).

On a machine short of RAM it also adds **memory-saving settings** there (a cx33's 8 GB ran out with
Pylance and the kernels): `python.analysis.indexing: false`, `python.analysis.diagnosticMode:
"openFilesOnly"`, `python.analysis.exclude` (the venv, `.git`, caches, `__pycache__`, `data`,
`infrastructure`, `node_modules`, dot-dirs), and `files.watcherExclude` / `search.exclude` for the
venv, caches, `__pycache__`, `data` and `infrastructure` (`files.exclude`: `__pycache__` only, so
the Explorer still shows the rest; `.git` stays watched for the Source Control view). Which machines:
**every Hetzner VM** (a CPU box: the kernels' models share its RAM with VS Code; images built with
`arena image build` carry them), and on other providers only one with **under 20 GiB of RAM** (a
container's own memory limit counting) — a GPU pod with plenty keeps Pylance's indexing
(auto-imports, workspace symbols). Each key is added only where absent; to an exclude map already
there only the patterns it doesn't name (one set to `false` stays `false`). The file is edited as
JSONC, the way VS Code writes it: comments, trailing commas and every byte already there stay, the
edit is read back before it's written, and a file it can't parse the way VS Code does is left alone
with a warning. It's swapped into place in one step (`renameat2`) and the file it replaces is kept
beside it (`settings.json.arena-<random>`), so a save VS Code makes meanwhile is never lost: that
one stays in place (or, saved twice, in the kept file), with a warning; where no such swap can be
done, nothing is written. A window already open picks the change up after **Developer: Reload Window**.

Skips whatever is already there; its
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
keys from `<keys-dir>/<provider>_api_keys.csv` (openai/anthropic/openrouter) **plus a broadcast
token** — a Hugging Face token (`--hf-token`/`HF_TOKEN`, sets `HF_TOKEN` +
`HUGGING_FACE_HUB_TOKEN`) for **gated repos** (Llama 3, …) — and each pod's own **Claude Code
account's token** ([`claude`](#claude-code-accounts-claude); `--cc-token` puts one token on every
selected pod instead, moving nobody).

Every reachable target also gets the **fleet's ssh**, keys or not: every machine's keys authorized
(shared, deploy, admin) and the host map in `~/.ssh/config` (pods reach each other with their deploy
key). So `copy-keys` is how machines that already exist come to let in a key added since — the admin
key, say — staff boxes included; with no key file, token or archive, that is all it writes.

Targets / `--exclude` scope it to specific pods. Confirms first; `--dry-run` lists what would be set
(values redacted; each pod's Claude Code account by name). `HF_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN` and
`CLAUDE_TOKENS_FILE` are env-introducible (e.g. `HF_TOKEN=… arena pods copy-keys`). Each pod's write
has a 60s budget, so a wedged pod reports `✗ <name>: … timed out` instead of hanging the command; a
pod whose Claude Code token didn't go fails the run too (`arena claude push` retries it).

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

`keys gen|list|rotate|revoke|limit|pause|resume` — manage **OpenRouter** runtime keys via the provisioning API (needs
`OPENROUTER_PROVISIONING_KEY`). `gen <targets|--all>` mints one key per machine (named
`<prefix>-<machine>`) with a USD cap (`--limit`, default `OPENROUTER_KEY_LIMIT` or $5) and writes
`keys/openrouter_api_keys.csv`; `--copy` also pushes them out via `copy-keys` (targets may be
MACHINE_NAME_LIST names with no pod yet; `--all` = every current pod).

`rotate <targets|--all>` deletes + re-mints (leak recovery), `revoke` deletes only, `list` shows
**this iteration's** `<prefix>-*` keys — each one's cap (`/d`, `/w`, `/m` when it starts over daily,
weekly or monthly), what it spent in the cap's period, today, this month (UTC) and all told, and a
total row — noting how many others were hidden (`--all` shows every key in the workspace, disabled
ones included — the listing walks every page).

**Caps, pause, resume — in place.** `limit <targets|--all> <USD|+USD>` changes each machine's key cap
where it is (`PATCH /keys/{hash}`: the secret, its spend and the pod's copy of the key all stay —
nothing to copy, the participant's next call works): `10` sets the cap, `+5` raises each key's own
cap by $5 (`arena keys limit apple +5`, `arena keys limit --all +5`). The amount is always the last
argument. `--reset daily|weekly|monthly|never` changes when the cap starts over (00:00 UTC, weeks
from Monday; `+0 --reset daily` keeps the cap); `--no-cap` takes the cap away — never an amount. A
change that tightens a cap (lower, a first cap, another reset period) under what the key has spent
in the new cap's period — today's for a daily cap, …, its whole life's without a reset — or when that
isn't known, is refused for every target before anything changes; `--force` sets it anyway (its
calls fail at once: `keys pause` is the way to stop a key). A raise that still leaves a key under
its spend goes ahead and says `still at its cap`. `pause` disables each machine's enabled keys (their
calls fail; the `AI$` cell says `off`) and `resume` enables the one the dashboard shows for the name
— the newest — never an older, rotated-out key (aliases `disable`, `enable`). The dashboard's
`paused` is something else: a $0 cap nothing was spent against, which `limit <name> 5` lifts. A key
already as asked is left alone. Each command: `--dry-run`, a confirm (`--yes`), then one line per key
— `devtest-alpha: cap $0.10 → $0.20 · spent $0.00 in all`, `on → off` — and `--json` (below).

**Changes made meanwhile.** One of these commands changes keys at a time on a box (a lock in the
state directory, `openrouter-keys.lock`; a second waits up to 60 s). After the confirm, machine by
machine — every one named, a skipped one too — the plan is made again from keys read at most 2 s
before that machine's changes (a resume: right before): a machine whose keys moved since the preview (another operator, the
dashboard: a cap, reset or on/off changed; a key replaced, added or renamed away), or whose spend now
makes the cap one it would refuse, is left alone and fails the command — run it again. Each key is
read once more in the request just before its change and refused if it moved, or (a cap) has spent
past the new cap; OpenRouter has no conditional update, so that moment is all that's left.
Afterwards each key is checked against what OpenRouter lists (a change it answered OK for but didn't
make fails; caps compared exactly, not to the cent), and by name: a key still on under a paused
machine's name is said, and a key a resume woke that sits beside a newer one under its name (made
meanwhile, on or off) is turned off again if it's still as read, then read back to say what it is
— each failing the command. Keys that can't be read
back leave the changes `unverified`, and the command fails.

`--json` prints `{action, dry_run, keys: [{machine, name, hash, before, after, result, error?,
note?}], changed, failed}` (`before`/`after`: `{limit, reset, disabled, spent}`, `after` as listed
afterwards — `null`, with a top-level `reread_error`, when the keys couldn't be read back; `result`:
`changed`, `would change`, `unverified`, `unchanged`, `no key`, `failed`). Of several keys under one name (a
rotation's disabled leftover), a cap change and a resume act on the one shown — the enabled one,
then the newest — and name the others. With a workspace, each key is read first and one from
another workspace refused, as for a rename. `OPENROUTER_KEY_LIMIT` stays what `gen` and `rotate`
mint with.

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
month, all told), and the top line's `OpenRouter $3.20 today · $422.09 left` (the info panel:
`openrouter $422.09 left · AI today $3.20`). Its group turns yellow, `⚠` in front, when the credit
is less than the enabled keys' caps could still spend (`⚠ OpenRouter $5.00 left < caps' $8.00`) or a
key is at its cap, and red when the credit is gone (`⚠ OpenRouter $0.00 left, calls fail`). The keys
at their cap are named — `key at cap: arena9-foo (no pod)`, `3 keys at cap: arena9-foo (no pod),
arena9-bar +1 more`, two at most (on a narrow terminal just `key at cap`) — those no listed pod is
named after first (`(no pod)`: no row shows it, e.g. its machine was terminated or renamed); the
detail pane's `ai caps:` line lists them all and points at `arena keys list`. A key with a $0 cap that has spent
nothing in its cap's period is **paused**, not at its cap: its cell says `paused` (grey) and the
warning leaves it out. The credit is the account's (an organization's whole), whatever the workspace.

**Acting on a key from the dashboard.** With a provisioning key set, the action menu (`a`) has the
pod's key rows: **`ai cap`** (`a`, so `a a` from the list) opens a box prefilled `+` one new key's
cap (`OPENROUTER_KEY_LIMIT`, else `+5`) that shows what the key spent of its cap and the new cap as
you type — a cap the CLI would refuse stays in the box, saying why (the dashboard never forces) —
and Enter runs `arena keys limit <pod> <amount> --yes`; **`ai pause`** / **`ai resume`** (`o`,
whichever the key needs) a `y` confirm with the key's state now; **`ai rotate`** (`n`, red) runs
`arena keys rotate <pod> --copy --yes` — the old key stops at once, the new one (cap
`OPENROUTER_KEY_LIMIT`) is saved in `./keys` where the dashboard was started and copied onto the
pod. A **marked set** gets `ai cap`, `ai pause` (`o`) and `ai resume` (`O`), one run for those of its
pods with a key (the confirm names the ones left out). The rows are there whenever a provisioning key
is configured, so none moves under the cursor; for a pod without a key, or before the keys are read,
they're grey with why, and refused if picked. They run as CLI children in the runs panel (`p`), so
the CLI and the dashboard share one path; the raise-cap box and each confirm run on the pods and keys
they were opened with (a refresh meanwhile changes neither the preview nor the run), passed as
shown (`--expect HASH=CAP/RESET/ON|OFF`, hidden), and the command refuses, before changing
anything, a key replaced or changed since — look again (a name with two enabled keys, which the
dashboard shows as one: the CLI); when one ends the keys are read again at once, and the
`AI$` cell and the top line show the change within seconds. The at-cap warning says `(a → raise
cap)` when a listed pod carries the key, and the detail pane's `ai key:` line starts `at its cap (a
→ raise cap)` or `disabled (a → ai resume)`.

Keys are found by name, so no local hash bookkeeping; a machine that left the list but still has a
key (or a CSV row) can still be targeted by name. Dry-runs show each key name and whether it exists.
`keys which` shows the local keys file. (`copy-keys` is the *distributor*; `keys` is the
*generator*.)

### Claude Code accounts (`claude`)

`claude accounts | assign | push | off` — each machine on **one Claude Code account** from a pool,
with only its own account's token on it. One token broadcast to every pod put the whole cohort on
one account's usage limits (one heavy user throttled everyone), made a leaked token everyone's
rotation, and said nothing about which machine used what.

**The pool.** `CLAUDE_TOKENS_FILE` names a tokens file in the format of `clods` (its `tokens` file
works as is): one account per line, its token made by `claude setup-token` while logged in as it.

```text
# email -> token
alice@arena.education -> sk-ant-oat01-…
bob@arena.education = sk-ant-oat01-…
7 carol@arena.education -> sk-ant-oat01-…    # a leading number pins clods' profile number
```

`=` or plain spaces work as well as `->`; `#` lines are comments; a line without a number is the
number after the line before. The file must be **owner-only** (`chmod 600`): one other users can
read is refused, as is a number or an email twice. A line that can't be read is skipped and named by
its first word (never its token). An account is its **email**: clods numbers lines in order, so a
number moves when a line above it goes, and assignments are kept by email; the number is for typing
(`assign apple 2`). Without a tokens file, config's single `CLAUDE_CODE_OAUTH_TOKEN` is a pool of
one — every machine on it, as before (beside a tokens file it's unused, and `config check` says
so). `config check` counts the accounts, or says why the file can't be used; never a token.

**Who's on which.** Each machine *instance* — provider + id, as spend's owners: a name created again
is a new machine and inherits nothing — has one account, recorded in `claude.json` in the state dir
(0600, under the state dir's lock):

- **spread**: the account with the fewest *people*. A spend owner (`pods up --owner`, `spend
  assign`) counts once however many machines they have, and a person's next machine joins the
  account their others are on; a machine with no owner counts as one, so without owners it's the
  fewest machines. Ties go to the lowest number. A new machine is spread when it's set up (`pods
  setup`, `up`, `start`, `restart`, `replace`, `copy-keys`); `assign --spread` spreads again.
- **pinned**: `arena claude assign apple bloom 2` (a number, an email, or `none`).
- **its owner's pin**: `arena claude assign --owner alice 3` — alice's machines now, and the ones she
  gets later; `--owner alice --spread` takes the pin off and spreads hers again.
- **off**: `arena claude off apple` (= `assign apple none`): no token on it, and it stays so.

A machine whose account has left the file is spread again at its next push; `accounts` lists it
under the old email until then.

**The push.** A machine gets its own account's token and nothing else, as
`export CLAUDE_CODE_OAUTH_TOKEN="…"` in `~/.bashrc` and `~/.zshrc` — the channel copy-keys always
used — with every earlier such line taken out (a moved machine has the new token only; `off` takes
them all out), and Claude Code's onboarding marked done in `~/.claude.json`. The token goes **on the
ssh command's stdin**: never in a command line (not in `ps` at either end, nor in any log or dry
run) and never in a file, here or on the pod, that a dropped connection could leave behind. Each rc
file is rewritten beside itself (keeping its mode; a link's file, not the link) and renamed over
it, so a full disk or a lost connection leaves the old file whole. A re-push changes nothing on a
box that's already right. Each push has 60 s. Two pushes to one machine take turns (a lock per
machine), each reading what the machine should have when its turn comes. What went is recorded (the
account and a fingerprint of its token), so a moved machine or a rotated token shows as out of
date. Shells already open keep their old token until they start again. A machine terminated by
arena, or missing from a complete listing (`pods list`, the dashboard, the snapshot cron line,
`teardown --check`, `balance`), is forgotten, so it stops counting as its account's load.

- `arena claude accounts [--json]` — the pool: each account's number, email, machines and people;
  `*` marks a machine whose box may not have its account's current token yet; the machines that are
  off or not on an account yet; owner pins. Lists the fleet, and forgets machines that are gone (only
  from a provider that listed).
- `arena claude assign <pods> <N|email|none>`, `assign <pods> --spread`, `assign --owner NAME
  <N|email|none>` / `--owner NAME --spread` (`--no-push`: record only) — then pushes the pods that
  moved; a stopped one gets its token at its next setup.
- `arena claude push [pods | --all]` — with no pods named, every cohort pod that's out of date:
  never pushed, moved, its account's token changed in the file, its account gone from the file, or
  not on an account yet (it's spread first). Named pods, or `--all` (this cohort's), are pushed
  whatever their state.
- `arena claude off <pods>`.

Every change confirms first (`--yes` skips it; `--dry-run` previews) and works on every provider:
it's SSH only. In `up`, each pod's line says `[apple] claude ✓ #2 bob@arena.education (new, by
spread)`; a push that fails is a warning with the retry (`arena claude push apple`), and the pod is
still READY. `--cc-token TOKEN` on `pods setup`/`copy-keys` puts that one token on every selected pod
instead and moves nobody (`accounts` then shows them out of date). The dashboard's detail pane shows
a pod's account on its `tokens:` line (`(not on it yet)` when the last push was another's), and `c`
in its menu moves it (`arena claude assign` in the runs panel: a number, an email, `none` or
`spread`).

**Usage per account** isn't shown: a `claude setup-token` token is inference-only, and Anthropic
publishes no usage API for subscription tokens (Claude Code's own `/usage` needs a full login).

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

With no targets the check ends with **this box** (where the backups and copies land; under `--json`
on stderr): its disk and what arena left on it, as in `config check`, then every pod whose copy onto
it was skipped for space in the last day (`✗ arena9-bloom: copy skipped 12× (since 09:00 UTC, last
10:15 UTC) — only 3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB)`) — which fails the check like
a pod's FAIL ([Room on this box](BACKUPS.md#room-on-this-box-control_min_free_gb)).

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
- with a Shadeform key, the account's **Shadeform volumes** (arena never makes one; any there bills
  per GB-month), priced at Shadeform's listed rate — fix a `curl -X POST …/v1/volumes/<id>/delete`
  (irreversible; or the Shadeform console);
- this cohort's **enabled OpenRouter keys** with usage — fix `keys revoke <names>` (by name: `--all`
  only reaches machines that still have a pod; staff boxes' keys are only named in a note);
- this user's crontab — arena's block (`cron remove`) and hand-added lines mentioning arena
  (`crontab -e`);
- pending **`at` jobs** whose command (from `at -c`, never its environment) mentions arena or a
  legacy fleet script (`destroy_pods`, …) — fix `atrm <ids>` (`at` not installed is said, not
  failed);
- forwards left in the **local** proxy file — fix `proxy apply`;
- the **backup archive** ([`arena archive`](BACKUPS.md#backup-archive-arena-archive)) in its Hetzner
  project: this cohort's is ◷ **kept** until `ARCHIVE_KEEP_UNTIL` (on purpose, never against the
  check); past that date, with no date, orphan volumes or another cohort's archive it is ✗, with
  its monthly cost and the fix `archive access off`, then the typed `archive down --delete-volumes
  --confirm <prefix>-archive` (another cohort's: with that cohort's config);
- and the fleet's **Hetzner project's snapshots**: an `arena image build` VM still there is ✗
  (it bills by the hour; fix `image prune`); this cohort's arena images are ◷ **kept** (made on
  purpose for `HETZNER_IMAGE=arena`, cents a month — never against the check), with their size and
  monthly cost and a note to `image prune --keep 0` at the end; any other snapshot in the project
  (made by hand, another cohort's) is a note with its cost, never touched;
- and what arena **left on this box** (local, read-only): replace/migrate's staging copies
  (`$TMPDIR/arena-replace-<pod id>`), an interrupted `pods baseline` capture, rsync temp files a
  hard-stopped pull left — ◷ **kept**, never against the check, and never removed by it: a staging
  copy can be the only copy of a pod's work. Each with its size, age and what made it, a
  note with the commands to look at it (and, for a staging copy, what to check first), and its
  `rm -rf` as the fix; one a running arena is using now has no fix. See [Room on this
  box](BACKUPS.md#room-on-this-box-control_min_free_gb).

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

A listing that fails (a 429 is waited out and retried first, within the listing's bound) is stood in for by
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

### `spend`

`spend [--by owner|machine] [--since DATE] [--csv|--json]` — what each machine instance cost and
who it was for, compute and AI, for the organisers (read-only towards the providers; it records
what it read in the local ledger). `spend assign <machine|instance id|key hash> <owner>` (or
`--clear`) sets or fixes an owner.

**On the fleet you already run** — a project week included: it needs no fleet of its own. Give
new machines an owner as you make them, existing ones afterwards, then ask:

```bash
arena pods up apple bloom --owner alice      # new machines: who each is for
arena spend assign cedar bob                 # machines that already exist (or a forgotten --owner)
arena spend                                  # one row per owner, then (no owner), then totals
arena spend --since 2026-11-02               # only from that day (UTC)
arena spend --by machine --csv > spend.csv   # one row per instance, for a spreadsheet
```

Nothing else to set up: the snapshot cron line (`cron install --snapshot DIR`, every 2 min) records
each listing, and so do `pods list`, the dashboard and every create, stop, start and terminate.
Without that line the ledger moves only when one of those runs (a gap over 30 min is an estimate).
The ledger is in the state dir (`ARENA_STATE_DIR`, else `~/.local/state/arena`), so run `arena
spend` as the user the cron line runs as, or set `ARENA_STATE_DIR` in the config for both.
Tracking starts once a build with `arena spend` (2.3) is installed: a machine terminated before
then has no row, and one already running counts from when arena first sees it (an estimate,
erring low — RunPod's from its creation where RunPod's billing covers it).

**Rows are instances, owners are a field.** An instance is provider + provider id: a terminated
machine keeps its row, and a name created again is a new row (`--by machine` shows both). The
owner is recorded by `pods up/create --owner NAME` for the instances that run makes (`--dry-run`
says what it would record), by `spend assign` later (a machine name means its running instance),
and kept by `replace`/`migrate` for the instance that replaces it. Nothing else in arena knows
about owners. `--by owner` (default) is one row per owner — machines, running hours, compute per
currency, AI in USD, total — then `(no owner)` (where a forgotten `--owner` shows up), the totals,
and the notes.

**Where the numbers come from.** A ledger per fleet, `spend.json` next to `balance.json` (0600,
`~/.local/state/arena/<prefix>/`), folded from what arena already sees: every complete listing
(`pods list`, the dashboard, `teardown --check`, `arena balance`, `snapshot` — so the snapshot cron
line, every 2 min, keeps it current with nobody at a terminal) and every create, stop, start and
terminate arena makes, recorded the moment it succeeds (a terminate's end time is exact). Each
instance carries $/h segments: its listed price (a running RunPod pod's disk on top, as the balance
meters it), a stopped pod's disk only (RunPod: its volume at $0.20/GB-month; Vast: a $0.50
ceiling), a price change. Between two observations the cost is a step at the higher end. A
provider that failed to list ends nothing; only this fleet's names (`{prefix}-…`, `@` entries) get
rows, so another fleet's pods on a shared account stay out. An unreadable or newer-version ledger
is moved aside (`spend.json.ignored-<time>`), never overwritten, and `arena spend` notes from when
the new one counts. While a ledger can't be read, `--owner` stops a create (whose keys are whose is
unknown) unless `--keep-key`; after a restart, so does `--owner` until the keys are listed again
(`arena spend` lists them), and then for a name whose key was made before the restart and has had
no owner recorded since — it may be someone's, so it isn't given to a machine's owner either —
until `spend assign <key hash> <owner>`, `keys rotate <name>`, or `--keep-key`. A listing is recorded as of when it began, so a stop or terminate recorded while it
was in flight stands.

**RunPod's own numbers.** RunPod's REST billing splits per pod: `GET /v2/billing/pods`, hourly,
one record per pod (checked live 2026-10-30: the records sum to `/v2/billing`'s total). `arena
spend` reads it for the window: an hour two hours old is what RunPod billed that pod (`SOURCE`
billed) — an hour without a record for a pod the answer has is $0 — and a younger one the higher
of billing and metering (billing lags about an hour). With `--since` mid-hour, that first part-hour
is metered (a bucket is a whole hour). The last
note says how much of the account's billing in the window the rows account for — the rest is other
pods, volumes or serverless.

**Estimates** are marked `~`, and the notes say why and which way they err:

| Note | Charged | Errs |
| --- | --- | --- |
| not observed for over 30 min | at the higher of the rates either side | either way (it may have been stopped, or started, meanwhile) |
| vanished between two observations | up to the first listing without it | high, by at most that gap |
| first seen well after its provider says it was made | back to then, at its first rate | either way |
| may have run before arena first saw it (made before tracking began, or found after its provider went unlisted for over 30 min) | from the first observation (RunPod: from its creation where billing covers it, then not an estimate) | low |
| no price known | 0 | low |
| stopped on Vast | its disk at a storage ceiling | high |

A listing older than what the ledger already has (a slow command, a clock that went back) changes
nothing: it neither ends a machine seen since nor reopens one ended since.

Hetzner bills a server at most its monthly price in a month (about 26 days of hourly price): not
modelled, a week never reaches it.

**Currencies are never added together.** RunPod, Vast and Shadeform bill USD; Hetzner the
account's currency, read from `GET /pricing` by `arena spend` (prod's project bills EUR, the
sandbox's USD; arena assumes EUR until it's read). Totals are per currency.

**AI keys** (OpenRouter, by hash). A key's owner is its machine's owner when arena minted it (`keys
gen`/`rotate`), or the first owner its machine gets (a key minted before the machine), or `spend
assign <hash> <owner>`. Its spend is its all-time `usage`; with `--since`, the rise from its usage
at the window's start. The snapshot cron line reads every key's usage hourly, so there is a reading
near any start: when the last one before it is older (the usage then rose before the next one), the
rise in between is counted, `~`, erring high; a key first read after the start counts from that
reading, `~`, erring low. Before arena deletes a key — `keys rotate`/`revoke`, `pods terminate
--revoke-key`, a handover — its usage as listed just before is recorded with the delete, so it
stays its owner's; a key deleted outside arena keeps its last seen usage, flagged.

**Key handover.** `pods up/create --owner bob apple`, when `apple`'s key was minted for alice:
the plan says so, and after the prompt (before the create) `apple`'s key is rotated — alice keeps
what it spent, bob's machine gets a fresh key (cap `OPENROUTER_KEY_LIMIT`). `--keep-key` keeps the
old key (and what's spent on it stays alice's). A handover that fails (OpenRouter down, no
`OPENROUTER_PROVISIONING_KEY`) stops the command before anything is created — the new machine would
get alice's key, or one already deleted (whose row then leaves the keys CSV). Re-run once `arena
keys rotate apple` works, or pass `--keep-key`.

`--csv`: per owner `owner,machines,instances,hours,compute_<cur>…,ai_usd,total_<cur>…,estimate`;
`--by machine --csv` one row per instance (`name,id,provider,gpu,owner,start,end,end_how,hours,
currency,cost,source,estimate,estimated`). `--json`: the whole report. A source that couldn't be
read (a provider's listing, the keys, RunPod's billing, Hetzner's currency) is one warning line;
the exit is 0.

### Running a second fleet from the same control box

Optional, and **not needed for a project week**: that runs on the normal fleet, with `--owner`
and `arena spend` as above. A second fleet is for another program that wants its own machines,
names, AI keys and ledger on the same control box and proxy: its own config file, prefix,
OpenRouter workspace and proxy file. Nothing in arena has a second-fleet mode; a second config
is all it is.

What must differ in the second config (`/root/p2/config.env`, from `config.env.example`):

| Key | Why |
| --- | --- |
| `MACHINE_NAME_PREFIX=p2` (and its own `MACHINE_NAME_LIST`) | names, state dir, ledger, backup branches; avoid `@` entries the other fleet lists. **Not `<other prefix>-…`** (say `arena9-p2`): its machines and keys would be named `arena9-…`, and the main fleet would count them as its own (and the other way round) |
| `OPENROUTER_WORKSPACE` | its own workspace: its keys, and the AI spend per key |
| `SSH_PROXY_NGINX_CONFIG_PATH=/etc/nginx/streams-enabled/p2.conf` | a fleet's `proxy apply` owns its file: two fleets in one file drop each other's forwards |
| `SSH_PROXY_STARTING_PORT=9200` | ports are start + list position: keep well clear of the other range (prod 9000+) |
| `SSH_ADMIN_CONFIG_FILE=~/.ssh/p2-admin.conf` (or `SSH_CONFIG_FILE`, whichever the other fleet uses) | the default file is the other fleet's |
| `LOCAL_BACKUP_DIR`, `ARENA_START_DATE` | if it backs up: its own folder and day numbering |

Then, as root on the proxy box (`/home/dev/.cargo/bin/arena` is where prod's `arena` is, and
where `arena update` installs: next to the running binary — `readlink -f "$(command -v arena)"`
prints it on another box):

```bash
mkdir -p /root/p2                                     # its own folder: keys/ is relative to it
p2() { (cd /root/p2 && /home/dev/.cargo/bin/arena --config /root/p2/config.env "$@"); }
p2 config check
p2 proxy plan && p2 proxy apply                       # writes p2.conf only
p2 pods up apple --owner alice                        # each person's machines
p2 spend                                              # per person; --by machine --csv for a sheet
p2 tui                                                # its dashboard
```

- `arena` reads `--config` (only `arena-tui` reads `ARENA_CONFIG`; `arena tui` passes `--config`
  on) — hence `p2()`. Open the second fleet's dashboard with `p2 tui` only: its actions run
  `arena` in the folder it was started from, and only `/root/p2` has the second fleet's `keys/`.
- **Cron: never `arena cron install` for the second fleet.** A crontab has one arena block, and
  its lines are known by command, not config: the second fleet's install would replace the first
  fleet's lines (`cron remove` would remove them), and both would share the locks in
  `~/.local/state/arena-cron/`. Write its lines by hand where arena never looks, e.g.
  `/etc/cron.d/arena-p2` — the snapshot line is the one that keeps its ledger current. Name the
  binary by its full path, as prod's own crontab does (cron's `PATH` is `/usr/bin:/bin`: a bare
  `arena` fails there, quietly):

  ```
  */2 * * * * root mkdir -p /root/.local/state/arena-cron-p2 && cd /root/p2 && flock -n /root/.local/state/arena-cron-p2/snapshot.lock /home/dev/.cargo/bin/arena --config /root/p2/config.env snapshot --public --out /root/p2/fleet.json >> /root/.local/state/arena-cron-p2/snapshot.log 2>&1
  ```
- **Shared provider accounts:** listings are per account, so the second fleet's `pods list` shows
  the first fleet's pods, and `--all` (or a command without targets) acts on them too. Name your
  targets with `p2`, or give the second fleet its own Hetzner project (its own token) and RunPod
  account. `teardown --check` lists every machine on the account.

## Config, plans and the provider APIs

### `config check | set | which`

`config check` — validate that the keys the selected provider + proxy + backup need are present
(never prints secret values; exits non-zero if a required key is missing or any key is set to a
value it can't use — every such key is a ✗ row and is named in the error; ⚠ rows, e.g. a key file
this user can't read, an admin key that is the shared one, or no `ARENA_START_DATE`, are advice). Copy `config.env.example` to get
started.

`config check | set | which` — `check` is the read-only doctor (keys + setup readiness); `config set
KEY VALUE` writes a key (e.g. an API key) into config.env — or give just `KEY` and **pipe the value
on stdin** (`printf %s "$TOK" | arena config set HF_TOKEN`, script-friendly, keeps secrets out of
`argv`/`ps`), or with no args prompt interactively (the picker shows which keys are already set);
`config which` shows the active config file (path, readable/**writable**), what parsed, and any keys
coming from the environment.

After the report, `config check` looks at **this box's disk** (local, read-only; advice, never a
failure): one row per filesystem arena writes to — `LOCAL_BACKUP_DIR` (and `LOCAL_BACKUP_BIG_DIR`),
`$TMPDIR` (replace/migrate's staging), the state dir — with what's free of what; ⚠ under twice
`CONTROL_MIN_FREE_GB` (default 5 GB: what every copy onto this box keeps free), ⚠ under it (every
copy onto it is then skipped), each with arena's biggest dirs there; then what arena left on the box,
with size, age, what made it and the commands to look at and remove it ([Room on this
box](BACKUPS.md#room-on-this-box-control_min_free_gb)). The report's own row shows the floor (`· copies
leave free here 5 GB (default)`; a bad value is ✗: pulls, backup copies and replace/migrate are
refused until it's fixed).

`config check` makes no request but one: with `HETZNER_IMAGE=arena` it looks up which snapshot that
resolves to for `HETZNER_SERVER_TYPE` (read-only, bounded at 30 s), with a ⚠ when it's over 14 days
old or the repo's requirements changed since it was built ([Hetzner: pre-built
images](#hetzner-pre-built-images)). Advice only: it never fails the check.

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

`gpus` — RunPod's GPU list as it always printed ([`offers runpod`](#offers) shows it per tier),
kept for habits and scripts (hidden from `--help`), unchanged on stdout: the same table, `gpus
--json` the same array of rows, and with another `--provider` the presets, as before.

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

`api get <runpod|runpod-v1|runpod-v2|vast|hetzner|shadeform> <path> [--raw | --limits]` — **read-only**
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

`--limits` prints the rate limits the API reported for that GET instead of the body: the
`RateLimit-Policy` and `RateLimit` headers as received (a partition key `pk` blanked), each window
as it stands (`minute: 47 of 60 left, resets in 12 s`), any hold, and the requests this process
sent. Counts only. Without the headers it says `no RateLimit headers in its answer: no limit
reported`. The GET goes through the same rate-limit path as every command.

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
| `backups [--json]` | [What gets backed up, where and when](BACKUPS.md#what-gets-backed-up-where-and-when) |

`archive up|status|sync|resize|access|ssh-config|ssh|forget|unforget|down` — the **backup archive**:
an always-on Hetzner VM with two volumes (dated snapshots, all files) holding copy #2 of
`LOCAL_BACKUP_DIR`, pushed by cron, readable by participants once `access on`; never part of the
fleet. See [Backup archive](BACKUPS.md#backup-archive-arena-archive).

`backups [--json]` — **what gets backed up, where and when**, read-only: each backup job (the git
backup, the home copy, the archive's two pushes) set up or not and how often, its last run and
result, when it's next due and whether it's running now; each cohort pod's last git save, home
copy and archive copy, failing first; where the copies live, with their free space. It reads this
box only (the crontab, the run record `backups.json`, `LOCAL_BACKUP_DIR`, the archive's state, the
locks), and says `?` with why for anything it can't read. Exits 0. The dashboard's `b` shows the
same. See [What gets backed up, where and when](BACKUPS.md#what-gets-backed-up-where-and-when).

## The dashboard (`arena-tui`)

`arena-tui` (TUI) — interactive dashboard (ratatui): pods from every configured provider plus, per
pod, GPU stats via `nvidia-smi`, git branch, a setup-health check, and an optional progress signal
(`PROGRESS_CMD`) — all over SSH in **one** probe per pod. Fetching runs in a **background task** so
the UI never freezes; it auto-refreshes (`ARENA_REFRESH_SECS`, default 5) and `f` cycles the cadence
live (2/5/10/20/60s). `ARENA_PROVIDER` (default `runpod`) is where the add-pod form starts; config via
`ARENA_CONFIG`.

### Columns

**Columns**: a provider badge (`R`/`V`/`H`/`S`), NAME (short `apple` by default; `s` toggles full
`arena8-apple` and the choice persists to `~/.config/arena-tui/prefs`), STATUS, a **SET** health
glyph (`✓/✗/·` for `~/.name`, the deploy key, git origin→GitHub, an API key), GPU (live from `nvidia-smi`, e.g.
`2×RTX A4000 16G` — the provider list API omits this; a MIG slice by its own name, `1×B300 MIG 34G`,
not the parent card nvidia-smi names), GPU%/MEM/TEMP, **BRANCH** (autocommit branches
shortened to their `w1d2` label; `main`/others shown as-is), and progress. **RTT** is the round trip
from this box — where the proxy runs — to the pod's SSH endpoint: one TCP connect, timed alongside
the SSH probes (never before the rows are shown), at most once a minute per endpoint (each is a
connect closed before any SSH, which OpenSSH ≥ 9.8 counts against the box; once a minute keeps even
several dashboards far from its limit); green under 60 ms, yellow under 120, red beyond, `-` when
not timed yet or no answer. The detail pane's `where:` line has it beside where the pod runs
(`EUR-IS-1`, `Czechia, CZ`, `nbg1 (Nuremberg, DE)`). Below 107 columns (or in the detail view's
split) TEMP, RTT, MEM, BRANCH and DISK drop out in that order, so NAME/STATUS/GPU/$/H/HLTH/PROXY
stay whole down to 69 columns; above it `💾` SAVED (from 112), AI$ (122, with an OpenRouter key), the
GPU's `RTX ` (124), full names (130) and CPU%/RAM% (140) come in, each whole, then the graphs
(`crates/tui/src/columns.rs` has the order).

### Fleet columns from the core snapshot

**Fleet columns from the core snapshot** — every refresh is one `snapshot::build` (`arena
snapshot`'s builder) over the per-provider listing, the local proxy file and the health cache, so
these read exactly as the CLI prints them: `$/H` (provider currency, `-` unless billing), an **M**
badge for a host maintenance window (detail pane: window + note) — else **L** for a locked pod
(detail pane: `status: … · locked`; restart/stop/terminate on it are refused with the unlock hint,
and the dashboard never unlocks), **HLTH** = the last `pods test --deep` verdict as a glyph + its age
(`✓12m` pass, `!5h` warn, `✗2h` fail, `·` skipped, `…` checking, `-` never; detail pane: `fail 2h
GPU error` + the failing check's text), **PROXY** = the stable port, green live, yellow with a `!`
when stale (`9501!`; detail pane: `:9501 stale (arena proxy apply re-points it)`), `-` no forward,
`?` remote proxy, **💾** = when the repo was last committed (`↑` unpushed, `↓` behind, `⇕`
diverged). The **top line** is the fleet in groups, a dim `│` between them:

```text
 51 pods, 48 GPUs 37% · 2 unreachable │ 42 RunPod $520.30 ≈4.6d · $4.62/h │ ⚠ Vast $40.00 ≈40h │ 9 Hetzner €1.23/h │ ⚠ OpenRouter key at cap: arena9-apple (a → raise cap) · $12.40 today · $422.09 left
```

first the fleet (pods, GPUs and their mean util, unreachable pods in yellow); then one group per
provider with pods — its count and name, the prepaid account's balance (`~$118.30 est.` for an
estimate) and runway, and the burn of its pods (each provider's own, in its currency: Hetzner's `€`,
postpaid, has no balance; `(2 unpriced)` when some have no price, so the figure is a floor); then
OpenRouter's (the cohort's spend today, the credit left). A group's name is green while all is well;
`⚠` in front and yellow when its account is low, red when critical (`⚠ 3 RunPod $5.00 ≈11h`) or when
the provider didn't answer (`⚠ Vast: list failed`: its pods and their cost are missing); OpenRouter's
the same for its own (credit under the keys' caps, keys at their cap, credit gone). A provider with no pods and nothing wrong — an unfunded opt-in
account with nothing billing there, `shadeform $0.00 unfunded`, included — has no group (the info
panel has it). The line is built to fit the terminal: the least needed words go first, each kind in
every group at once — the provider and config tag at the right end (shown only when not the
default), GPU util, the GPU count, a fine account's runway, today's AI spend, keys with no cap, a
warning's details (`key at cap`, the names left to the info panel and the `AI$` column), the burns,
a fine account's balance and the credit, the providers' pod counts, the fleet's — and never a
warning or a group's name: at 80 columns the same fleet reads ` 2 unreachable │ RunPod │ ⚠ Vast
$40.00 ≈40h │ Hetzner │ ⚠ OpenRouter key at cap`. With more warnings than the width holds, the red
ones go first, then the yellow, then the calm names, and the groups that still don't fit are counted
at the end (`+2 more`, in the worst colour among them). A click on it opens the info panel, which has `pods
list`'s footer word for word plus per day, and the prepaid balances (`arena balance`'s views,
observed in the details round — at most one live read per account per 5 min — and recorded in the
CLI's state file). API calls per refresh are at
most one list per provider; the details query (GPU/$/maintenance) runs every 60 s or on `r` (≥10 s
apart); one that fails part-way still shows what it filled (as `pods list` does) over the last good
answer, with a footer warning.

**Rate limits.** The footer's status shows the providers' API requests in the last minute (`ok · API
13/min`); the info panel shows each one's and what its quota has left this minute (`RunPod API
13/min · 47/60 left this minute`). A provider call waits at
most 2 s for a rate limit. A listing that comes back rate-limited keeps that provider's last rows,
and the dashboard stops listing for the API's own wait (else 5 s, doubling to 5 min). The SSH
probes keep the refresh cadence, and the footer says `warn: RunPod rate limit` (the info panel:
`RunPod rate limit: listing again in 12 s (the minute window is spent) · its rows are 40 s old`).
`r` doesn't list through that wait, and the details query waits too. When a window's quota is low
(a quarter or less left, or 10 when the quota isn't reported), every request on that key — the
dashboard's listings, `r` included — is spread to half of what's left over the time to its reset
(the info panel: `listing again in 10 s (the minute window is low: 15 of 60 left)`).

### Deep checks and marked sets

**`d`** deep-checks the cursor pod (or the marked set) **in the background** — no modal; the row's
HLTH says `…`, the footer reports the tally when done, and the verdicts go to the same health
cache `arena snapshot` reads. **`D`** deep-checks every running pod whose GPUs it has read and found no one using: the
first press says how many and names the busy ones it leaves out (any GPU with a compute process,
over 5% — the reaper's idle line — or holding over 1 GiB: a deep check loads the GPUs for about a
minute and on a busy one reads as a failure), and counts those it leaves out as not read yet (no
metrics so far), whose GPUs couldn't be read (the read failed, or `nvidia-smi` gave no GPU or no
utilization: a pod too loaded to answer must not look idle — `d` checks one on purpose) or
CPU-only (Hetzner, or 0 GPUs listed: always a fail). A second `D` within 5 s starts the pods the
first press counted that still qualify — never one a refresh added in between. In HLTH, a pass older than 26 hours is grey: it says little about today (the
daily `cron install --health` keeps it fresh; the two hours absorb its drift). **`space`** marks or unmarks the cursor pod (as does a click at a row's far left, or a shift-click),
**`x`** clears the marks, and **`/`** marks pods with the CLI's selector syntax (`alpha..delta`,
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

Every pod SSH call goes through `Remote` with a budget (metrics 20 s, test 90 s, run 30 min, backup
5 min, set-branch 2 min, setup per step, deep check 150 s), so a wedged pod ends an action with
`timed out after …`; only `c` (your interactive shell) is a plain `ssh`.

### Navigating

**Navigate** with `↑/↓`/`j/k`; `enter` opens a per-pod detail pane (per-GPU breakdown, full
branch/origin/health, util/temp **sparklines**; labels dim, each value in the colour the table gives
its state). The footer is `[i] info · <status> · ↻ 7s ago · every 5s` (refreshed 7 s ago, every 5 s;
`refreshing, last 7s ago` while it does, the `↻` in cyan; a notice, a run going on or a question out
comes before the status), in plain words but for that one mark, and the most-used keys; **`i`**, or a
click on `[i] info` or on the top line, opens the **info panel**: provider, config, the whole status,
deep checks running, the fleet's summary word for word, each prepaid account, the OpenRouter credit
and keys at their cap, every key, and what each column's glyphs mean (Esc or `i` closes it).
**`b`**, or a click on the `💾` column's header, opens the **backups screen**: each backup job
(how often, last run, next run, or the command that sets it up), what's running now, and every
pod's last git save, home copy and archive copy, failing first (`o` reorders), then where the
copies live. Enter opens a pod's history, from which `b` backs it up now and `h` copies its home
now (each with its confirm; the screen comes back after). The screen shows what `arena backups`
prints: [What gets backed up, where and when](BACKUPS.md#what-gets-backed-up-where-and-when).
`Ctrl-C`/`q` quit. A hangup or a TERM ends it at
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
stop and terminate only on a **marked** set, which `a` acts on while pods are marked — a stop there skips, and names, pods not running and
pods whose provider has no stop (Shadeform), and refuses a set of only those, as `pods stop` does),
or **add pods** with `n` ([Adding pods](#adding-pods)). The cursor pod's menu also runs four CLI
commands as they are — `start` (a stopped pod, after a confirm), `rename` (type the new name),
`claude` (type its new Claude Code account: a number, an email, `none` or `spread` —
[`arena claude assign`](#claude-code-accounts-claude)), `jobs` and `logs` (read-only: the newest
job's last 100 lines) — as `arena pods …` children, their
output in the runs panel (below); with a provisioning key, also the pod's AI key: `ai cap`, `ai
pause`/`ai resume`, `ai rotate` ([keys](#keys-openrouter): `arena keys …` children too, a marked set's
cap/pause/resume as one run). `run` and `set-branch` pop a
text-input modal to type the command / branch; `run` shows each pod's whole output (its last 40
lines, stderr too when it fails); `test` is read-only. Every mutation goes through a confirmation
modal — the *only* place the TUI mutates anything. Lifecycle actions (restart/stop/terminate)
require **typing the pod's exact name**; restart is destructive like terminate (red, and its modal
says what it does to the container disk on that pod's backend: WIPES it on RunPod v2, MAY WIPE it on
RunPod v1 and Vast; Hetzner: "the disk is kept"); whole-fleet mutations
require typing **ALL** (a marked set: its count, or `ALL` as above); every confirm says what will
happen in plain words (`alpha: commit + push the current branch`), never the generated `ssh …` line
— `v` shows that for backup/setup/test — and backup/setup/test take a single `y`. The dashboard's
reads stay reads.

### Adding pods

**Add-pod (`n`)** is a form that opens at once, from the rows already on screen: nothing is listed
or awaited, so stepping across providers with `←→` is instant (it used to list the whole fleet on
every provider change, about a second each with the dashboard frozen). `↑↓` moves between fields:
the provider (those with an API key), RunPod's cloud tier (`COMMUNITY`, `SECURE`, or `COMMUNITY →
SECURE` to fall back), the type, GPUs per pod, disk, volume, `--check`, **retry**, and how many pods
(the free names as of the last refresh — `up` lists the fleet again and picks its own). Retry is
what `up` does when nothing is free: `off` (once, the default), or keep trying every minute for up
to `10 min`, `30 min`, `2 h` or `24 h` (`--retry-mins N`; each round tries every option, near the
proxy first, for the confirmed names only, and none starts past the window). `--keep-trying` isn't
used: it gives up after an hour whatever it says, takes one GPU type and tier only, drops the
near-the-proxy preference, and its waits don't hear Ctrl+C. The type is a
GPU type, or on Hetzner the **server type** (`HETZNER_SERVER_TYPE`, default `cx33`, first; each with
its vCPUs, RAM and €/h) — there are no GPU, disk, volume or `--check` fields there. The choices come from the
provider's live catalogue, read once in the background by `arena offers` (RunPod: every type it
sells, B300s and MIG slices included; a type sold only on the other tier moves the tier there and
says so); the presets stand in until it lands (`…`) or if it can't be read. Under the fields, what
`up` would try for the choice, in order, with price, stock and near/anywhere: `arena offers
--json`, asked once the form has rested on a choice for 0.4 s and kept for 3 minutes, so arrowing
through a list costs one request (`o` turns it off).

`enter` shows the exact command and `y` runs it: `arena --config <the dashboard's> --provider <P>
pods up --add N … --yes` — so a pod from the dashboard gets everything `up` does (placement and its
fallbacks, near the proxy, the endpoint wait, proxy, setup, keys, `--check`). It's the `arena`
installed beside `arena-tui`, never one from `PATH`. It runs in the background:

- **The runs panel (`p`)** shows a run's command, each pod's state from its `[created]`, stage,
  `READY` and `FAILED` lines, and the whole output (scroll with `↑↓`/PgUp/PgDn, `End` follows,
  `←→` other runs, `Esc` hides it — the run goes on). The footer says `running: pods up 1m12s (1 under
  way)` while one runs, and the tally when it ends (`pods up: exit 1 — 1 ready, 1 failed`, in red). A
  created pod shows as a `pending` row at once.
- **`s`** in the panel sends the run Ctrl+C (SIGINT to its process group): `up` stops where it is
  and terminates nothing. A retrying run says so in the panel and the footer (`waiting for capacity
  until 14:35 UTC`); `s` between two rounds ends its retries at once (`interrupted — stopping
  retries`), keeping what was made.
- Its output is also in a file of its own under the state directory (`runs/`, the newest 30
  kept). It isn't a pipe, and the run is in a process group of its own, so it outlives the
  dashboard: **quitting while a run that changes pods is going asks first** — leave it running (the
  default: it finishes what was confirmed, as `nohup arena pods up …` would), send it Ctrl+C, or
  stay. A hangup or a TERM leaves it running too. Read-only runs (`jobs`, `logs`, `offers`) end
  with the dashboard.

### Keeping up with the keys

Each pass of the loop takes **every** input event already waiting (up to 256, or 40 ms of them)
before it draws again; it used to draw after each one, so a held key, a burst of keys or a slow
terminal over SSH queued events faster than frames went out, and the lag grew with the queue. A
frame is drawn when something changed — a key, a refresh or a result landing, a notice — or once a
second for the clock (every 0.2 s while a running command's output or a pending availability check is on screen); the
view is rendered under the shared lock and the terminal written after it's released, so a slow
terminal never holds up the refresh. No network or SSH call is awaited in the key loop: each runs
in a task of its own and lands through the shared state. The mouse reports clicks, drags and the
wheel, not every pointer motion. `ARENA_TUI_TRACE=<file>` records each frame (render and write
times, in µs) and each input event, to measure lag (see [Testing](#testing)).

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
public IP is replaced by `up` after `--endpoint-timeout` like any other; one still without an
endpoint on the last attempt fails as `FAILED endpoint`, and the guard still cleans it up.

### The live attestation check

The offline tests verify recorded attestations (`crates/core/src/fixtures/attestation_*.json`),
so they can't notice Sigstore rotating a key or GitHub changing its bundles. This opt-in test
does: it reads the latest release's attestation from GitHub (2 anonymous API requests, no
account, nothing spent), verifies it for the x86_64 `arena` and `arena-tui` as `arena update`
does, and checks it doesn't verify as another workflow's or another repository's.

```bash
ARENA_LIVE_ATTEST=1 cargo test --release -p arena-core --lib attest -- --ignored --nocapture
```

### Measuring the dashboard

`ARENA_TUI_TRACE=<file>` makes `arena-tui` write one tab-separated line per frame (`frame <µs since
start> <render µs> <write µs> <events since the last frame>`) and per input event (`event <µs>
<key>` — a key by its kind, `Char` for any character, never what was typed). It takes a new file,
made readable by you alone, and refuses a path that exists. `ARENA_TUI_DEMO=<n>` (1–500, a developer's aid, not in `--help`) shows a made-up fleet of
`n` pods instead of the real one — nothing is listed, probed or run — so a sandbox with no pods can
measure the dashboard at a cohort's size. The summary of a trace (frames, render/write times, and
per burst of keys the time from its first event to the frame that shows its last):

```bash
ARENA_TUI_TRACE_SUMMARY=/tmp/trace.tsv cargo test -p arena-tui -- --ignored --nocapture trace_summary
cargo test --release -p arena-tui -- --ignored --nocapture view_cost   # µs per frame over 60 pods
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

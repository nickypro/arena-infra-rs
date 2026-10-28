# Changelog

What changed, for the people who run arena. Newest first, one entry per change you'd notice.
`arena update` shows the entries your build doesn't have yet, with every **Action needed** and
**Config** line first.

How to write an entry:
- The heading is `## YYYY-MM-DD · what changed`, and it's the entry's identity: once released,
  never edit a heading (an old build would then see it as new). Fix the text under it instead.
- A few bullets in plain words: what an operator sees differently.
- `### Action needed` for anything someone must do after updating. `### Config` for keys that
  were added, renamed or removed, or whose default changed.

Before this file, see the merged pull requests on GitHub (master `a335316`, 2026-10-21, and
earlier).

## 2026-10-28 · Commands over ssh work whatever the remote login shell (fish too)
- Every command arena runs over ssh now reaches the remote shell as one simple command any
  shell reads the same way. On a box whose user's login shell is fish (or another shell that
  isn't POSIX), `pods run`, setup, deep checks, backups and the dashboard's probe now run under
  bash; they used to fail to parse. Bash, zsh and sh login shells still run them themselves.

## 2026-10-28 · OpenRouter spend in the dashboard, and keys kept to one workspace
- The dashboard shows each pod's OpenRouter key as `AI$` (spent/cap: yellow from 80%, red at the
  cap), the key's today/week/month in the detail pane, and the account's credit and the cohort's
  spend today in the summary bar. A credit lower than what the keys' caps allow, or a key at its
  cap, leads the bar. Nothing changes without `OPENROUTER_PROVISIONING_KEY`.
- `arena keys list` adds SPENT (in the cap's period), TODAY, MONTH and ALL columns and a total.
- `arena keys` writes the keys file owner-only (0600) and atomically. It used to be created readable
  by anyone who could reach the folder; an older file is tightened on its next write.

### Config
- New, optional: `OPENROUTER_WORKSPACE` (a workspace id or slug). OpenRouter's provisioning key
  reaches every workspace in an organization; with this set, `arena keys`, `terminate
  --revoke-key`, `pods rename` and the dashboard only see and change keys in that workspace, and new
  keys go there. Unset: the Default workspace, as before.
## 2026-10-28 · A daily health check on cron; `pods test` checks this cohort's pods
- `arena cron install --health 07:20` adds a daily `pods test --deep` at that time, in this
  box's time zone. Its verdicts show in the dashboard's HEALTH column, and its table in
  `~/.local/state/arena-cron/health.log`. Your other cron lines stay as they are.
- `cron show` says when it runs (`↳ daily at 07:20, this box's time zone (UTC +0000)`);
  `cron remove --health` takes it out. A time on the reaper's minutes (:11, :27, :41, :57) gets a
  warning: a check then looks like use to the reaper.
- `pods test` and `pods test --deep` with no pod names (or `--all`) now check only this cohort's
  pods (`<prefix>-…`), not staff boxes or another prefix's pods. One `⊘ skipped N non-cohort pods`
  line counts the rest; `--include-non-cohort` checks every pod, as before.

### Action needed
- A script that runs `pods test` to check every pod on the account needs `--include-non-cohort`
  now.
## 2026-10-28 · `ssh <pod>` keeps working: arena can keep your own SSH config current
- `arena ssh-config --install` writes `~/.ssh/arena.conf` (a `Host` per pod) and makes
  `Include ~/.ssh/arena.conf` the first line of `~/.ssh/config`. It shows the change and asks
  first, and keeps a copy of the old file. `--uninstall` takes that line out again.
- With `SSH_CONFIG_FILE` set, the file is kept current: after every pod change (`up`, start,
  restart, replace, migrate, rename, terminate, the reaper), on `pods list` and on each dashboard
  refresh. It's rewritten only when something changed.
- Only a file arena wrote is ever rewritten (its first line says so). A hand-made file, or one
  from an older `ssh-config --out`, is left alone with one warning line.
- `config check` says whether the file is written and whether `~/.ssh/config` includes it.

### Config
- New, optional: `SSH_CONFIG_FILE` (e.g. `~/.ssh/arena.conf`). Unset: nothing is written. Leave
  it unset on a box that runs arena's cron lines: their syncs would update that user's file
  whenever a pod changes.

## 2026-10-27 · `pods ask` and `?`: pick each AI's model and effort, shorter answers
- The dashboard's `?` names the AI with its model and effort, in a shorter box; **Tab** switches
  between claude and codex, and the next `?` opens with the one you last picked.
- Answers are brief (about 12 lines), so they come back sooner. In the dashboard they're
  rendered: bold, `arena` commands in cyan, a bullet's wrapped lines under its text, no raw `**`.
- `pods ask --effort low|medium|high|xhigh|max`, next to `--model`.
- A small model at low effort answers in seconds: on one pod, codex `gpt-6-luna` at `low` took
  about 5 s, claude's default 12–19 s.

### Config
- New, all optional (unset: the CLI's own): `ASK_CLAUDE_MODEL`, `ASK_CLAUDE_EFFORT`,
  `ASK_CODEX_MODEL`, `ASK_CODEX_EFFORT`. For quick answers from codex:
  `arena config set ASK_CODEX_MODEL gpt-6-luna` and `arena config set ASK_CODEX_EFFORT low`.

## 2026-10-27 · `arena update` installs the newest release and says what changed
- `arena update` reads the latest GitHub release, shows the entries of this file your build
  lacks, downloads `arena` and `arena-tui` for your platform, checks them against the release's
  `SHA256SUMS`, checks the new `arena` runs, asks, and swaps both in, keeping `arena.prev` and
  `arena-tui.prev`. It lists dashboards still running the old build.
- `arena update --check` only says (exit 10 when there's a newer release); `--dry-run`
  downloads and checks without installing.
- Every green master commit is a release: Linux x86_64 and arm64 builds.
- `arena --version` names the commit too: `arena 2.0.261027.1500 (5274b7a1c2)`.

## 2026-10-27 · RunPod pods show their SSH address again
- RunPod's v2 list stopped including each pod's IP and port (2026-10-26), so the dashboard
  showed `init` for running pods and backups couldn't reach them. Each pod's address now comes
  from its own details, remembered while the pod keeps running, so a refresh costs no extra
  request.
- After a restart the address is checked again every 30 s for 3 minutes (RunPod keeps showing
  the old port for about 30 s).
- One pod whose lookup fails no longer fails the whole listing: only that pod shows no address.
### Action needed
- Restart long-running `arena-tui` dashboards: one started before this still runs the old build.

## 2026-10-26 · Hetzner VMs go to the nearest location with room
- A create tries every location that sells the server type, nearest first (timed from the
  machine running arena), and moves to the next when one has no room. "No room anywhere" now
  waits under `--keep-trying` instead of failing at once.
### Config
- `HETZNER_LOCATION` unset (or `any`) means any location, nearest first. One location still
  pins creates there; a list (`nbg1,fsn1`) is tried in order; `nbg1,any` is nbg1, then the rest.

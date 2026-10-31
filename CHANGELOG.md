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

Versions are `major.minor.YYMMDD.HHMM` (the base is `BASE` in `crates/cli/build.rs`). Bump the
minor when a command, flag or `--json` shape changes in a way a script or habit notices; the major
for a config or on-pod layout break that needs action after updating; otherwise the date stamp
alone tells releases apart. Say the new version in the entry that bumps it.

Before this file, see the merged pull requests on GitHub (master `a335316`, 2026-10-21, and
earlier).

## 2026-10-31 · arena 2.5: move a Hetzner box to a bigger type, keeping its files
- `arena pods replace apple --server-type cx43` and `pods migrate copy apple --server-type cx43`
  now work on Hetzner: a new server of that type (a comma list falls back), wherever
  `HETZNER_LOCATION` allows — it may land in another location, and the run says so — the home
  copied straight from the old box, the names swapped by two renames (no delete-then-wait gap),
  the hostname and `~/.name` following. `--keep-trying` waits for room as `up` does.
- `arena pods spare up 4 --server-type cx43 --keep-trying` makes capacity spares ahead of need
  (`<prefix>-spare-N`, nobody's machine: no proxy, backup or keys). `--onto-spare` on `replace` and
  `migrate copy` takes one; `--onto <machine>` takes another machine arena has (a cohort machine
  only with `--force`, saying what's on it). `pods spare list` / `spare down`. `pods list`,
  `balance`, `spend` and `teardown --check` count the spares.
- `arena pods copy-home rarity scoot` moves a person onto another machine that keeps its own
  name: the same copy, nothing renamed; refused while someone is on either machine, and when
  `scoot` has files of its own in the way (named) unless `--overwrite` (moved aside, never deleted).
- `replace` and `migrate cutover` refuse a box someone is on (an SSH session, a Jupyter tab, a VS
  Code server) unless `--force`; `replace` points at `migrate` for those.
- The recipe that replaces a hand-made swapper script: [Hetzner: a bigger type, capacity spares,
  moving a person](docs/REFERENCE.md#hetzner-a-bigger-type-capacity-spares-moving-a-person).

### Action needed
- A script that runs `arena pods replace` on pods people may be on now stops there (nothing
  changed): add `--force`, or move those with `migrate copy` + `migrate cutover`.

## 2026-10-31 · Project week: the dashboard's setup on a Hetzner VM, staff names, renames say the new port
- The dashboard's **setup** on a Hetzner (or Shadeform) VM now runs what `arena pods setup` runs
  there: the VM setup script with its swap, the keys authorized and the hostname. It ran the
  RunPod/Vast steps on every pod before, so a Hetzner VM got no script and no swap.
- `pods up -n`/`-a` (and the dashboard's add form) never pick a staff `@` name from
  `MACHINE_NAME_LIST` for a new pod. A staff box is made by name only.
- `pods rename` says where the machine's proxy port goes (`devtest-echo :9504 → devtest-bravo
  :9501`): a port belongs to a name, so whoever works on the machine connects as the new name.
- Docs: [A project week, start to end](docs/REFERENCE.md#a-project-week-start-to-end), the
  organisers' flow in order: keys before machines, owners, a name for someone else, renames,
  backups and the end-of-week report.

## 2026-10-31 · arena 2.4: what works where — refused up front, greyed out in the dashboard
- One table says which commands and create flags work on which provider (RunPod, RunPod v1, Vast,
  Hetzner, Shadeform). Every command checks it for each selected pod before it asks anything, the
  dashboard reads it, and so does the new "What works where" matrix in docs/REFERENCE.md and
  FEATURE_MAP.md. A refusal names each pod that can't, why, and what to do instead.
- `pods reimage` on a Hetzner, Vast or Shadeform pod is refused before the wipe gate and the
  confirm. It used to ask both, then fail reading the pod's spec. A selection that mixes in such a
  pod is refused whole, with the `--exclude` that leaves it out. `replace` and `migrate` on Vast
  or Hetzner are also refused before anything is read. `migrate` on Shadeform is refused too: its
  cutover could never pass there and reverted itself after about 45 s.
- **A create flag the provider would ignore is refused now, not dropped.** That covers `--disk`,
  `--cloud`, `--bootstrap` and `--max-price` on Hetzner; `--volume` (above 0) and `--bootstrap` on
  Vast; `--cloud`, `--disk`, `--volume`, `--image` and `--bootstrap` on Shadeform. It applies to
  `create`, `up`, `replace` and `migrate copy`. `--volume 0` is still accepted everywhere. This is
  why the release is **arena 2.4** (a minor bump): a script that passed one of these flags now
  stops.
- The dashboard's action menu greys out what the pod's provider can't do, with the reason: stop
  and start on a Shadeform pod. Picking one says the whole sentence. Its add-pod form sends only
  what the provider's create takes: Vast, Shadeform and Hetzner get `--volume 0` (no volume)
  instead of the form's size, and Shadeform no longer gets `--disk`.
- Creating on Vast without a volume now warns that a restart or a stop may wipe the container
  disk. It used to say "set VOLUME_GB", which Vast doesn't take.

### Action needed
- Restart running dashboards (`arena-tui`) after updating. An old dashboard's add-pod form still
  sends `--volume` to Vast and `--disk` to Shadeform, and the new `arena` refuses those flags.
- Check scripts and cron lines that pass one of the flags above to a provider that ignores it.
  They now fail before anything is created; drop the flag.

## 2026-10-30 · The backups screen says when it can't read a copy; "backing up", not "backuping"
- `arena backups` and the dashboard's backups screen: a snapshot folder entry whose type can't be
  read now makes the home-copy cell unknown (with the path and the error), instead of quietly
  leaving that pod out.
- The dashboard's busy box while an action runs reads `backing up alpha…`, `stopping alpha…`,
  `terminating alpha…` (it added `ing` to the action's name: `backuping`, `stoping`).

## 2026-10-30 · Every backup in one place: `arena backups` and the dashboard's `b`
- `arena backups` shows which backups happen and when, and how they're doing. For the git
  backup, the home copy and the archive's two pushes: whether each is set up (if not, the command
  that sets it up), how often, its last run and result, when it's next due, and whether it's
  running now. Then every cohort pod's last git save, home copy and archive copy, failing pods
  first, then the ones longest since a good backup. Last, where the copies live and how much room
  is left. `--json` for scripts. It reads this box only, never a pod.
- In the dashboard, `b` (or a click on the `💾` header) opens the same as a screen. Enter on a pod
  shows its history and offers "back up now" and "copy home now", each with its confirm.
- `pods backup`, `pods pull` and the dashboard's backup now record their results in
  `backups.json` in the state dir. A cron still on an older arena shows "none recorded yet" until
  it's updated.
- Anything that can't be read shows as `?` with the reason, never as "ok" or "never".
- New page section: [What gets backed up, where and when](docs/BACKUPS.md#what-gets-backed-up-where-and-when).
## 2026-10-30 · The dashboard's top line, grouped by provider
- The top line reads by provider now: `51 pods, 48 GPUs 37% │ 42 RunPod $520.30 ≈4.6d · $4.62/h │ ⚠
  Vast $40.00 ≈40h │ 9 Hetzner €1.23/h │ OpenRouter $12.40 today · $422.09 left`. Each provider with
  pods has its pod count, its balance and runway, and what its pods burn (Hetzner in €); OpenRouter
  has the cohort's spend today and the credit left.
- A provider's name is green while all is well. `⚠` and yellow mark a low balance; red marks a
  critical one, or a provider that failed to list (`⚠ Vast: list failed`). OpenRouter's warnings
  (credit under the keys' caps, a key at its cap, credit gone) work the same way. An account with no
  pods and nothing wrong isn't on the line.
- The line fits your terminal: on a narrow one the least needed words go first (GPU util, then the
  GPU count, a runway that's fine, …) and a warning never does. With more warnings than fit, the red
  ones come first and the rest are counted (`+2 more`). A click on it opens the info panel, which
  still has every detail.
- The footer's refresh age has a `↻` in front, cyan while a refresh runs.

## 2026-10-30 · Raise an AI key's cap, or pause it, from the CLI and the dashboard
- `arena keys limit apple +5` raises a machine's OpenRouter cap by $5 where the key is: same
  secret, same spend, nothing to copy onto the pod, so the participant's next call works. `10` sets
  the cap, `--all` is every pod, `--reset daily|weekly|monthly|never` changes when it starts over,
  and `--no-cap` takes the cap away (never an amount). A cap set under what the key already spent
  (in the cap's period) is refused for all targets before anything changes, unless `--force`.
- `arena keys pause apple` stops a key (its calls fail, the `AI$` cell says `off`) and `arena keys
  resume apple` turns it back on. Aliases `disable` and `enable`.
- Each previews with `--dry-run`, asks first (`--yes`), prints one `before → after` line per key as
  OpenRouter lists it afterwards, and has `--json`.
- The dashboard runs these same commands from the action menu, when a provisioning key is set: `a`
  then `ai cap` (also `a`) opens a box prefilled with `+OPENROUTER_KEY_LIMIT` that shows what the
  key spent and the new cap. `o` pauses or resumes the key, `n` rotates it and copies the new one
  onto the pod. A marked set gets cap, pause and resume. The `AI$` column and the top line update
  within seconds, and the at-cap warning says `(a → raise cap)`.
- `gen` and `rotate` still mint with `OPENROUTER_KEY_LIMIT`. This stays arena 2.3: today's `arena
  spend` release opened the 2.3 line.

## 2026-10-30 · Tell us what hurts: pain points on GitHub
- When arena gets in your way, or you work around it, file a pain point:
  https://github.com/nickypro/arena-infra-rs/issues/new?template=pain-point.yml (label
  `pain-point`). The README's "Tell us what hurts" has a paragraph to paste into your agents'
  CLAUDE.md so they can file them with `gh issue create`. The repo is public, so leave out IPs,
  hostnames of private machines, keys and tokens, and participants' names and emails.
- The docs say what arena does today. Vast's create works (it has since 2026-10-08), production
  updates with `arena update` (not `cargo install`), and `pods rename` works on every provider.
  `config.env.example` now lists the keys it was missing: `ARENA_REPO_OWNER`, `GIT_SSH_KEY_LOCAL`,
  `LOCAL_BACKUP_DIR`, `DEFAULT_BRANCH`, `PROXY_LOCAL`, `EXTRA_SSH_KEYS`, `BACKUP_MAX_SIZE`,
  `BACKUP_REMOTE_PATH` and the three `OPENROUTER_*` keys. It no longer lists `BACKUP_BRANCH_PREFIX`,
  which nothing read.

## 2026-10-30 · Hetzner: a limit refusal says which limit and what frees room under it
- A Hetzner create refused over one of the account's limits (403 `resource_limit_exceeded`) now
  says what frees room under the limit Hetzner names: a primary IP for `primary_ip_limit` (live:
  the sandbox refused every create so, though its project held no server and no IP), a smaller type
  or a server for cores, a server for the number of servers, a floating IP, a volume, a snapshot
  (`arena image prune`) — and
  where to ask for more (Hetzner Console: Limits tab → Request change → Limit increase). It said
  "use a smaller type" whatever the limit, and called it the project's: Hetzner counts a limit over
  every project of the account.
- Hetzner's 403 `maintenance` says to try again later instead of "authentication failed"; a
  `pods rename` refused for a limit or for maintenance reads the same way. Only a 401 or a 403
  `forbidden` reads as "authentication failed".

## 2026-10-30 · Room on the control box: a bad floor refused before replace creates anything
- `pods replace`, `migrate copy` and a cutover's final sync refuse a `CONTROL_MIN_FREE_GB` that
  isn't a whole number before anything is created (their `--dry-run` says they would). Before, the
  copy found it only after the new pod was created and set up, and left that pod billing.
  `config check`'s ✗ row says what it refuses.
- A copy skipped or stopped for room says how much to free, and where: `only 3.1 GB free on /
  (floor 5 GB, …): this copy (~3.9 GB, measured) + the floor = 8.9 GB needed — nothing copied;
  free at least 5.8 GB on / (…)`. A replace/migrate staging refused so also names arena's biggest
  dirs on that disk, and another disk arena writes to with room, for the run's `TMPDIR`
  (`TMPDIR=/mnt/backups/arena-tmp arena …`); on a disk already under the floor it's refused
  before the pod's home is sized (an rsync dry run over all of it).
- The daily health check says when a copy was stopped (not skipped) for space and, for the
  backup's and the reaper's copies, that each run tries it again until there's room.

## 2026-10-30 · arena 2.3: `arena spend` — what each machine cost and who it was for
- This is **arena 2.3** (a minor bump: a new command, `arena spend`, and `--owner`/`--keep-key`
  on `pods create` and `pods up`).
- `pods up/create --owner alice …` records who the new machines are for. `arena spend` shows one
  row per owner — machines, hours, compute per currency, AI keys, total — then `(no owner)` (a
  forgotten `--owner` shows there) and the totals; `--by machine` one row per instance, `--since
  DATE`, `--csv`, `--json`. `arena spend assign <machine|id|key> <owner>` sets or fixes an owner.
- Rows are machine instances, never names: a name created again for someone else is a new row,
  and the old one keeps its owner and cost. Currencies are never added together (Hetzner's is
  read from its pricing: the sandbox project bills USD).
- RunPod rows are corrected from RunPod's own billing per pod (`GET /v2/billing/pods`), and the
  report says how much of the account's billing it accounts for. Other estimates are marked `~`,
  with which way they err.
- An AI key's spend stays with the person it was minted for: `--owner bob` for a name whose key
  was alice's rotates it first (`--keep-key` to skip; if the rotation fails, nothing is created),
  and every key arena deletes has its final usage recorded with the delete.
- `pods list`, the dashboard, `teardown --check`, `arena balance`, the snapshot cron line and
  every create, stop, start and terminate now record what they see in a local ledger,
  `~/.local/state/arena/<prefix>/spend.json` (0600) — quietly: nothing else they print changes.
- A project week runs on the fleet you already have: `--owner` on `pods up` for new machines,
  `arena spend assign` for ones that exist, then `arena spend --since <its first day>`. The
  snapshot cron line keeps the ledger current; tracking starts when this version is installed,
  so earlier history isn't there.
- Docs: "Tracking spend" in the README, and, optionally, how another program can run a second
  fleet from the same proxy box (docs/REFERENCE.md): its own config, prefix, workspace, proxy
  file and port range, and cron lines written by hand — `arena cron install` for a second config
  would replace the first fleet's lines.

## 2026-10-30 · The dashboard's bottom line is plain words
- The footer no longer uses `☰`, `⟳`, `✓`, `⚠` or a spinner, which some terminals draw as
  pictures. It reads `[i] info · ok · API 6/min · 7s ago · every 5s` (`warn: …` in yellow, `list
  error` in red, `refreshing, last 7s ago` while it does, `running: pods up …` while a run goes on). Click
  `[i] info`, or press `i`, for the info panel as before.

## 2026-10-30 · Copies onto the control box no longer fill its disk
- Every copy arena makes onto the box it runs on — `pods pull`, `pods backup`'s pull and its copies
  of pods git didn't back up (a failed push, a wrong origin, a BUSY pod), `set-branch --hard`'s
  copies, `pods baseline`, replace/migrate's local staging and the reaper's — now checks that disk
  first: it goes ahead only if `CONTROL_MIN_FREE_GB` (default 5 GB) stays free after it, the run's
  other copies and other arena runs' copies under way (a cron backup and an operator's pull at
  once, any cohort: each run notes what its copies may take in
  `<ARENA_STATE_DIR>/disk-reservations.json` until they end; notes that can't be read or written
  skip the run's copies). A pod's first copy counts as 3 GB per tier, a later one as 0.1 GB — later
  only once the pod's whole home was copied into that backup (`<LOCAL_BACKUP_DIR>/.arena-copied/<pod>`;
  the pods already backed up there are marked once, at the first such run), so an interrupted
  first copy's retry is still a first one —, the staging as measured. A pod that doesn't fit is
  skipped whole, before anything of it is written, with one line (`✗ arena9-bloom: skipped: only
  3.1 GB free on / (floor 5 GB, CONTROL_MIN_FREE_GB) — nothing copied; …`), and the
  run fails. The git backup itself isn't affected. (Prod's box was at 96 %, 3.1 GB free of 75 GB,
  with the */15 cron copying pods onto it unchecked.)
- The floor holds while copies write too: a run looks at the disks its copies write to every 2 s
  and stops them once one has less than the floor free (`stopped: only 4.9 GB free on / while
  copying …` — what was copied stays, the run fails, the health check lists it). An estimate is no
  bound: a later copy, counted 0.1 GB a tier, can bring a new 12 GB file.
- The daily health check (`cron install --health`) now ends with this box: how full its disk is,
  what arena left on it, and every pod whose copy was skipped for space in the last day, which
  fails the check.
- `config check` shows how full each disk arena writes to is (⚠ under twice the floor, ⚠ under it,
  with arena's biggest dirs there), and what arena left on the box: a replace/migrate staging copy
  (`/tmp/arena-replace-<pod id>`), an interrupted `pods baseline`, a hard-stopped pull's rsync temp
  files — with size, age and the commands to look at and remove each. `teardown --check` lists them
  too (◷ kept, never against its verdict). None of these removes anything: a staging copy can be the
  only copy of a pod's work.
- replace/migrate's staging copy is now locked while a run copies through it: a second
  replace/migrate of the same pod is refused instead of writing into it, and `migrate finish` / a
  finished `replace` never remove one a run is using. One an earlier attempt left stays when a later
  attempt copies directly (it may hold what the source has lost since): listed, as above.
- `pods setup` / `up` on Hetzner or Shadeform no longer leave their setup script
  (`/tmp/arena-hetzner-setup-<pid>.sh`) behind.
- The reaper keeps its own `REAP_MIN_FREE_GB` (default 20) free, never less than
  `CONTROL_MIN_FREE_GB`.

### Action needed
- On a filesystem with less than the floor free, every copy onto it is skipped (and its run fails)
  until there's room. On prod today the backups (`LOCAL_BACKUP_DIR` on the attached volume, ~330 GB
  free) are fine, but `/` (3.1 GB free: `/tmp`, where replace/migrate stage a pod's home, and the
  state dir) is under the 5 GB floor — a via-local replace/migrate would be refused, and `config
  check` / the health check show `/` with ⚠. Make room first, or lower the floor for a while
  (`arena config set CONTROL_MIN_FREE_GB 2`, or `CONTROL_MIN_FREE_GB=2` for one command).
- The first judged run on a backup marks the pods already backed up there as copied whole (one line,
  `note: N pod(s) already backed up … marked`); nothing to do.
- Suggestions for prod's control box (yours to decide; check before deleting anything):
  - `/tmp/arena-replace-leh3rc2hdcf31i` (3.5 GB, written 2026-10-14 09:42 UTC): replace/migrate's
    staging copy of pod `leh3rc2hdcf31i`'s home (`ARENA_3.0`, the chapter dirs, its dotfiles, the
    copy's `.arena_replace_marker`). Left by the build prod ran then — master from 2026-10-07
    (`bae0112`), which never removed its staging, even after a copy that worked; removal reached
    master with #3 on 2026-10-19. Check that the pod's work is on the pod that replaced it or in
    `LOCAL_BACKUP_DIR` (`arena pods list --no-probe | grep -F leh3rc2hdcf31i`, `ls -la
    /tmp/arena-replace-leh3rc2hdcf31i`), then `rm -rf /tmp/arena-replace-leh3rc2hdcf31i`.
  - The rest of `/tmp/arena-*` is small (a few MB): `arena-hetzner-setup-<pid>.sh` setup scripts
    (fixed above) and dev-owned test dirs from when the tests ran on that box (before 2026-10-21) —
    safe to remove.
  - journald (3.6 GB): `journalctl --vacuum-size=500M`, and `SystemMaxUse=500M` in
    `/etc/systemd/journald.conf` to keep it there.
  - `/root/archive/old-pod-backups` (14 GB): move it to the attached volume.
  - Docker (~6 GB reclaimable): `docker system df`, then `docker image prune` / `docker builder
    prune` if nothing needs what they remove.
  - Then `arena config check` shows what's free, and what arena left on the box.

### Config
- `CONTROL_MIN_FREE_GB` (new, default 5): whole GB every copy onto this box leaves free on its
  filesystem, 0..100000 (0: only the copy itself must fit). A bad value fails every copy up front.
  Settable from the environment for one run (`CONTROL_MIN_FREE_GB=2 arena pods pull alpha`).
- replace/migrate's local staging now keeps `CONTROL_MIN_FREE_GB` free (before: a tenth of the
  disk, at least 5 GB).

## 2026-10-30 · Ctrl+C stops a capacity retry at once, even mid-listing
- `pods up`/`create` with `--retry-mins` (and the dashboard's retry, stopped with `[s]`) now stop
  the moment Ctrl+C is pressed. Before, a press while arena was re-listing the fleet between rounds
  (which can take a while when a provider is slow or rate-limiting) still let one more round
  create pods.

## 2026-10-30 · `up` replaces a pod that never gets an SSH endpoint; Hetzner's project limit says so
- `pods up` no longer leaves a pod that never gets an SSH endpoint running and billing (seen on
  prod: a RUNNING 4090 with no public IP or ports, `FAILED endpoint: no SSH endpoint after 600s —
  left running`, then terminated and recreated by hand). With or without `--check` it is now a
  failed placement: `up` says which machine and data centre it was on, terminates it, and recreates
  the same name with the same options (others first), up to `--check-attempts` placements (default
  2; the flag now works without `--check` too). The last one is terminated too; the name then FAILs
  saying nothing of it is left billing. Nothing is terminated on an old look: its provider is asked
  once more first.
- When: a pod up without an endpoint for `--endpoint-timeout` minutes (new, default 5) — on RunPod,
  its container running behind RunPod's SSH proxy only (a host with no public IP; before, only
  `--check` replaced these); it always gets the whole `--endpoint-timeout`, past `--timeout` if
  need be — even one first seen so by `--timeout`'s last look. Shadeform's 60 s boot grace (its
  IP held back after it goes active) isn't counted, and there the deadline stays final: a pod
  not up by it is deleted, as before. And a RunPod pod still listed RUNNING with no container when a `--timeout` of 10
  minutes or more (the default) ends — its image never started (prod's case above). RunPod lists a
  pod RUNNING from the create while its image pulls, so the pull doesn't count: healthy pulls took
  1½–7 minutes live, and a `--timeout` under 10 minutes still leaves such a pod running.
- A pod still coming up when `--timeout` ends otherwise (Vast `loading`, Hetzner `initializing`, a
  RunPod pod under a short `--timeout`) is left running as before, but the reason now says it's
  billing and how to stop it.
- A Hetzner create refused over the project's limit (403 `resource_limit_exceeded`, e.g. "shared
  core limit exceeded") said "authentication failed creating …". It now says `Hetzner refused: the
  project's resource limit is reached (…) — use a smaller type, free a server, or ask Hetzner to
  raise the limit`, keeps what was made and stops there; it's never waited out, and with several
  options only that one is skipped.

### Config
- `ENDPOINT_TIMEOUT_MINS` (new, optional, default 5): the minutes `pods up --endpoint-timeout`
  takes when the flag isn't given; whole minutes, 1 to 120. `config check` shows it.

## 2026-10-30 · A more compact dashboard, an info panel, retry in the add-pod form; `B300 MIG`
- The dashboard's top is one short line: pods, GPUs and their util, unreachable pods, the burn
  (`$14.80/h + €0.048/h`), balances with their runway, AI spent today and the credit left.
  Warnings still lead it in their colours (a provider that failed to list, a low balance, AI keys
  at their cap). The provider and config show at its right only when they aren't the default.
- The footer is `☰ ✓ API 13/min ⟳7s · 5s` (status, refreshed 7 s ago, every 5 s; a spinner
  while refreshing) and the most-used keys, pairs merged (`[a/A] act/all`, `[d/D] deep`). `i`, or
  a click on the `☰`, opens an **info panel** with everything else: provider, config, the whole
  status, deep checks running, the fleet's full summary, each balance, the AI credit and keys at
  their cap, every key, and what each column's glyphs mean. Notices, a run going and a `?`
  question still lead the footer.
- Narrower columns: HEALTH is `HLTH` (`✓12m` pass, `!5h` warn, `✗2h` fail, `…` checking); PROXY
  is the port, `9501!` when stale; SAVED's header is `💾`. The width they free brings columns in
  sooner: every mid column from 107 columns (was 111), `💾` from 112 (was 114), CPU%/RAM% from 140
  (was 152), all of them whole.
- MIG slices are named `<model> MIG <memory>` everywhere: the dashboard's GPU column said
  `1×B300 SXM6 AC` (nvidia-smi's name for the parent card) and now says `1×B300 MIG 34G`; a deep
  check's GPU (`pods test`, HEALTH's detail), `arena snapshot` and a listing read without RunPod's
  details say `1×B300 MIG 34GB` too, as `pods list` and `up` already did (was `1×B300 SXM6 AC MIG
  1g.34gb`). Any `… MIG <n>g.<m>gb` slice reads the same way (A100, H100, H200, B200 …).
- The detail view (Enter) is in colour: labels dim, each value in the tone the table gives it
  (status, health, proxy, backup/sync, disk, CPU/RAM, maintenance, lock, AI spend, errors).
- The add-pod form (`n`) has a **retry** field: when nothing is free, `up` tries again every
  minute for up to 10 min, 30 min, 2 h or 24 h (`--retry-mins`; off by default). The confirm says
  until when; the runs panel and the footer say it's waiting for capacity; `s` in the panel stops
  the retries between rounds, keeping what was made.

## 2026-10-30 · VS Code's memory-saving settings on small machines, and a git identity from setup
- Setup writes VS Code's memory-saving machine settings (`~/.vscode-server/data/Machine/settings.json`)
  on every Hetzner VM, and on any other machine with under 20 GiB of RAM: Pylance's indexing off,
  diagnostics for open files only, and the venv, caches, `__pycache__`, `data` and `infrastructure`
  kept out of Pylance's analysis, the file watcher and search. A cx33 (8 GB) ran out of memory with
  Pylance and the kernels; these are the settings operators added by hand. GPU pods with more RAM
  keep Pylance's defaults.
- Only what isn't set is added (to an exclude list already there, only the patterns it lacks), so
  a participant's or an operator's own values stay. The file is edited as the JSONC VS Code writes:
  comments and trailing commas stay; one it can't read the way VS Code does is left alone, with a
  warning on the pod's line. When setup changes the file, the old one is kept beside it
  (`settings.json.arena-<random>`), and a save VS Code makes at that moment is never lost.
- Setup gives the course checkout a git identity (the backup's, `Arena Autocommit`) when none is
  configured, on every provider: a participant's first `git commit` or `git merge` on a fresh
  machine no longer stops at "Please tell me who you are". A participant's own is never overridden,
  and one they set globally later takes over at the next setup.

### Action needed
- Existing machines get both from a setup re-run, e.g. every Hetzner VM:
  `arena pods setup --all --on hetzner` (`--dry-run` first). The settings.json made by hand on a
  VM is kept; setup adds only what it lacks. A participant's open VS Code window picks the change
  up after **Developer: Reload Window**. Images need no rebuild: a VM from one gets them at its
  first setup.

## 2026-10-30 · `pods rename` works on Hetzner and Vast
- Fix: `pods rename` on a Hetzner VM failed with "rename not supported on provider `hetzner`".
  It renames the server in place now (no restart, its labels untouched), rewrites `~/.name` as
  before, and also sets the VM's hostname to the new name (what `hostname`, bash's prompt and
  VS Code show). The hostname stays after a reboot. A name Hetzner refuses (one another server
  in the project has, or one that isn't a valid hostname) is explained.
- Vast pods can be renamed too: their label is the name.
- The note that the MACHINE_NAME env var keeps the old name is printed only for pods that have
  that variable (RunPod and Vast), not for Hetzner or Shadeform VMs.
- `pods setup` on a Hetzner VM sets its hostname to the machine name when it isn't already. This
  fixes a VM that a rename couldn't reach (the rename says to run it).
- A pod whose provider can't rename it is refused before anything is asked, and the dashboard
  refuses it when you pick rename (instead of running the command and failing).

## 2026-10-29 · An admin key only the proxy holds, which reaches every machine
- `ADMIN_SSH_KEY_PATH` (optional) is now a key of its own role: its private half stays on the
  proxy / control box, and every machine — cohort pods and `@` staff boxes, on every provider —
  lets its public half in. Staff log in to the proxy box and reach any machine from there; arena
  there offers it first, so it gets in even where the cohort key was revoked. Nothing copies it
  to a pod.
- `arena ssh-config --admin` prints staff's config: every machine at its live endpoint with the
  admin key alone. `--admin --install` keeps it in `~/.ssh/arena-admin.conf` (its own file, next
  to the participants' one) and includes it from `~/.ssh/config`; with `SSH_ADMIN_CONFIG_FILE`
  set it is rewritten after every pod change, `pods list` and dashboard refresh.
- `config check` shows the admin key beside the shared and deploy keys, and warns when it can't
  be read, has no public half, is the shared or the deploy key under another name, or other
  users can read it.
- Hetzner VMs now let in the deploy and admin keys from their create (cloud-init), not only the
  cohort key; Hetzner and Shadeform VMs' setup authorizes them too, before its long script.
- `pods copy-keys` now writes the fleet's ssh (every machine's keys and the host map) to every
  pod, even one with no API key of its own and with no key file or token at all — before, it
  stopped with "no keys to copy".

### Action needed
- To use it, on the proxy box: `ssh-keygen -t ed25519 -N '' -f /root/.ssh/arena_admin_key`, then
  `arena config set ADMIN_SSH_KEY_PATH /root/.ssh/arena_admin_key`. Machines that already exist
  let it in after `arena pods copy-keys --all` (or `arena pods setup`); new ones from their create.
  Then `arena ssh-config --admin --install`.

### Config
- `ADMIN_SSH_KEY_PATH`: now in `config check`, the example and the docs (it was read before, but
  undocumented). Optional.
- `SSH_ADMIN_CONFIG_FILE` (new, optional): staff's ssh config file to keep current, e.g.
  `~/.ssh/arena-admin.conf`, on the proxy box. One of the two per user: not kept while
  `SSH_CONFIG_FILE` is set too, or while `~/.ssh/config` includes the participants' file (both name
  the same hosts).

## 2026-10-29 · arena 2.2: Hetzner VMs from a pre-built image, set up in about a minute
- This is **arena 2.2** (a minor bump: a new command, `arena image`, and `--image` on a Hetzner
  create; `update` offers it to every 2.1 build).
- `arena image build` bakes a Hetzner image: a temporary cx23 set up as `pods setup` does (no
  tokens), scrubbed of everything that belongs to that machine (deploy key, authorized keys,
  tokens, history, name, swap, machine-id, SSH host keys, cloud-init state, logs), scanned for
  secrets — it refuses to snapshot on any hit, naming the file — its free space zeroed, then
  snapshotted. The VM is deleted whatever happens, Ctrl+C included. `arena image list` shows the images, their cost and
  whether the repo's requirements still match; `arena image prune` deletes old ones.
- `HETZNER_IMAGE=arena` (or `--image arena` on `pods create` / `pods up`) creates VMs from the
  newest image that fits the server type, else from Ubuntu with a note. Setup then takes about a
  minute instead of ~10 (sandbox: 1m22s against 4m48s): it fast-forwards the checkout, keeps the
  env when the requirements are the same (rebuilds it when not), and says which image the VM came
  from and how old it is.
- On every Hetzner VM, `pods setup` now updates an existing checkout before the setup script, as
  on RunPod/Vast pods (a clean default branch fast-forwards; `--force` resets it).
- `config check` says which image `arena` resolves to, with a note when it's over 14 days old or
  the requirements changed. `teardown --check` lists the images as kept (◷, with their monthly
  cost; never against the check), a build VM still there as remaining, and any other snapshot in
  the project as a note.
- Images belong to one Hetzner project: build one in each (`arena image build` on the control box).

### Config
- `HETZNER_IMAGE` takes `arena` (new): the newest arena-built image in the project that fits.

## 2026-10-29 · The dashboard keeps up, and its `n` makes pods with `arena pods up`
- The dashboard handles every key already waiting before it draws again, so holding a key or
  typing ahead no longer queues up (over SSH it could lag for seconds). Nothing in it waits on the
  network any more: switching provider in the add-pod form is instant (it used to list the whole
  fleet each time, about a second with the screen frozen).
- `n`'s form now runs `arena pods up --add N … --yes` — the `arena` installed beside `arena-tui` —
  so a pod made from the dashboard gets what `up` does: fallbacks, near the proxy, the endpoint
  wait, proxy, setup, keys, and `--check` if you tick it. It runs in the background; `p` shows its
  progress (each pod's READY/FAILED and the whole output), `s` there stops it (Ctrl+C: nothing is
  terminated), and quitting while it runs asks whether to leave it running (the default) or stop it.
- The form's type list comes from the provider's live catalogue (RunPod: B300s, MIG slices…; on
  Hetzner, the server types with their size and price), and a line under it shows what `up` would
  try for your choice, with prices and stock (`o` hides it).
- On Hetzner the form picks the server type, `cx33` first.
- The pod menu (`a`) also has start, rename, jobs and logs, run as the CLI's own commands.
- The summary bar names the AI keys at their cap (`⚠ AI key at cap: arena9-foo (no pod)`: no pod
  in the table has that key), and the detail pane lists them all and points at `arena keys list`.
  A key with a $0 cap that hasn't spent anything is `paused`, not at its cap.

### Config
- `HETZNER_SERVER_TYPE`'s default is now `cx33` (4 vCPU, 8 GB), was `cx23` (2 vCPU, 4 GB): new
  Hetzner VMs cost about 1.5× as much. Set `HETZNER_SERVER_TYPE=cx23` to keep the old size.

## 2026-10-29 · arena 2.1: `offers` lists everything on sale, compact help, Vast host classes
- This is **arena 2.1** (a minor bump: `offers` and its `--json` changed shape; `update` offers it
  to every 2.0 build).
- `arena offers` is the same table, without the paragraph under it: one line says what `-v` adds
  (the notes, what `--max-price` dropped, who was skipped). Its `--json` is unchanged.
- `arena offers runpod` lists every GPU type on every tier — B200, B300 and the MIG slices included,
  secure-only ones as `SECURE` — with `#` on what `up` would try. `arena offers vast` shows the cards
  on Vast's market now, the cheapest offer per host class; `arena offers hetzner` the server types
  grouped by family (cx, cpx, cax, ccx) and by size; `arena offers all` every provider. `--gpu`,
  `--cloud` and `--max-price` filter them. `--all` still works; `gpus` prints as before.
- `--gpu` takes RunPod types its public catalog doesn't list, when arena has seen them: `NVIDIA B300
  SXM6 AC MIG 1g.34gb` (also `--gpu "B300 MIG"`), types on the account's pods, and
  `RUNPOD_EXTRA_GPU_TYPES`. It warns, and RunPod's create decides. `offers runpod` lists them as
  `unlisted`.
- Vast's hosts have tiers, as RunPod's cloud has: `--cloud datacenter`, `verified` or `unverified`
  (`up --provider vast --cloud datacenter` rents datacenter hosts only). The default is unchanged:
  verified hosts, unless `VAST_ALLOW_UNVERIFIED=1`. The option tables show the tier.
- Every command's `--help` lists its options one per line, without a blank line between each, and
  keeps all its text.
- The dashboard's GPU picker offers B200, B300 and the MIG slices too.
- A create refused for capacity says the provider's own words (`[no capacity] devtest-apple on 1×RTX
  A4000 SECURE: … There are no instances currently available`), so "full" reads apart from a type
  the provider doesn't know.

### Config
- New, optional: `RUNPOD_EXTRA_GPU_TYPES`, exact RunPod GPU type ids its catalog lacks
  (comma-separated), for `--gpu` to accept and `offers runpod` to list. Works from the environment
  for one run.

## 2026-10-29 · No more "command not found: conda" on Hetzner VMs
- A Hetzner VM's `~/.zshrc` no longer has conda's init block or `conda activate arena-env` (it came
  from the GPU image's shared rc; a VM has no conda, so every login printed an error). The uv env
  is activated as before, now even with a `conda` on PATH (the old block skipped it then). A
  conda a participant installed themselves (its own `conda init` block) is left alone.
- Re-running `arena pods setup` on a VM no longer overwrites `~/.zshrc`: it takes the conda lines
  out of the VM's own, keeping the API-key exports `copy-keys` put there and anything the
  participant added. Until now a re-run dropped those keys from zsh until the next `copy-keys`.
  Only oh-my-zsh's untouched template is replaced outright; a `~/.zshrc` that isn't arena's is
  saved as `~/.zshrc.pre-arena-<time>` before it's replaced.
- VMs set up before this get it at their next `arena pods setup`.

## 2026-10-29 · RunPod's rate limits are respected, and REST v1 ends on 2026-11-16
- RunPod limits every API since 2026-10-27, per key, shared by every dashboard and cron on it.
  arena now waits out a `429` (what RunPod asks, else 1, 2, 4… s) and retries it up to 3 times,
  and holds back its own requests while a window has 1 request or none left; when a window runs
  low (a quarter of it or less), it spaces its requests so half of what's left lasts to the reset.
  Concurrent requests each count against what's left. A command that can't wait that long fails
  at once with the wait: `list pods: not sent — RunPod REST v2 rate limit (the minute window is
  spent): next request in 12 s`.
- RunPod reads (listing, pod details, catalog, billing) are sent again after a 5xx or a dropped
  connection. A create, stop or terminate never is. The reaper's terminate waits out a rate limit
  for 10 s at most; past that it's left to the next run, which decides again.
- The dashboard's footer shows RunPod's request rate and what's left this minute (`RunPod API
  13/min · 47/60 left this minute`). When RunPod refuses a listing or its quota is held, the
  dashboard keeps RunPod's last rows, says how old they are (`⚠ RunPod rate limit: listing again
  in 12 s (the minute window is spent) · its rows are 40 s old`), keeps probing the pods over SSH,
  and lists the other providers as usual. A held quota is not asked until it frees.
- `arena api get runpod pods --limits` prints RunPod's quota (each window's limit, what's left,
  when it resets) instead of the body.
- Vast's throttled listing is waited out the same way (it used to be retried only when Vast asked
  for 2 s or less).
- REST v1 (`RUNPOD_API=v1`) ends on **2026-11-16**, a day later than we said. After that a v1
  request gets an error saying v1 is gone and to remove `RUNPOD_API=v1`.

### Action needed
- If your config still has `RUNPOD_API=v1`, remove it before 2026-11-16 (v2 is the default).

## 2026-10-29 · Pods near the proxy first, and an RTT column in the dashboard
- `pods create`/`up` (and `replace`, `migrate`) on RunPod and Vast now try Europe first — where
  the proxy every participant's SSH goes through runs — unless that costs more than 25% over the
  cheapest option. A far pod adds its round trip to every keystroke: a US pod about 100 ms.
- RunPod: SECURE pods only, pinned to the `EU-…`/`EUR-…` data centres. COMMUNITY hosts are in no
  data centre, so a community create is placed as before; to get a near A4000, add SECURE
  (`--cloud community,secure`) and raise the margin past 47% (it's $0.25/h there, $0.17 community).
- Vast: offers in European countries first, priced at the cheapest one there, never rented dearer
  than the margin allows.
- `arena offers` and the dry-runs show the order with a WHERE column (`Europe`/`any`) and say why
  an option isn't tried near. A single SECURE or Vast create now shows that option table before
  its prompt, as a create with several options does; with `--keep-trying` it stays one pool,
  anywhere, and says so. If Europe has no room, the same option is tried anywhere, and on RunPod
  Europe isn't asked again for the rest of the run: a 10-pod `up` with Europe full makes 11
  creates, not 20.
- The dashboard has an RTT column: the round trip from this box to each pod's SSH port, green under
  60 ms, yellow under 120, red beyond. It's timed once a minute, after the rows are shown. The
  detail pane's new `where:` line shows it beside where the pod runs (`EUR-IS-1`, `Czechia, CZ`,
  `nbg1 (Nuremberg, DE)`), which `pods list --json` now includes as `location`.

### Config
- New, optional: `NEAR` (`europe` when unset; `north-america`; `any` or empty turns it off) and
  `NEAR_MAX_EXTRA_PCT` (default `25`). Both work from the environment for one run, e.g. `NEAR=any
  arena pods up -n 3` for the old cheapest-anywhere order. `config check` shows both.

## 2026-10-29 · Hetzner VMs get 8 GB of swap
- `arena pods setup` (and `up`) on a Hetzner VM now makes an 8 GB swap file first, so a notebook
  that goes past the VM's RAM slows down instead of having its kernel killed. Shadeform's GPU VMs
  get it too; RunPod and Vast pods can't swap.
- It's skipped, with a `WARN` line, when the disk would have under 10 GB left, and a swap that's
  already on is kept. Setup never fails over it.
- A VM set up before this gets it at its next `arena pods setup`.

### Config
- `VM_SWAP_GB` (new, default 8): GB of swap setup makes on a bare VM; `0` = none, at most 64.

## 2026-10-28 · `arena update` checks who built the release
- `arena update` (and `--dry-run`) now verifies the new binaries' build-provenance attestation
  before installing or running them: that both were built together by this repository's
  `release.yml` from master, on GitHub's runners, as Sigstore's signature, certificate and
  transparency log prove. It checks this here, with no `gh` and no GitHub login, and says which
  workflow run built them. Until now it only checked `SHA256SUMS`, which comes from the same
  release as the binaries.
- Binaries without an attestation, or with one that doesn't verify, are not installed, and the
  message says why. `arena update --skip-attestation <sha256>,<sha256>` installs without this
  check, only binaries with those two SHA-256s, with a loud warning: for a genuine release this
  build can't verify (after Sigstore changes its keys), whose binaries you verified with
  `gh attestation verify` elsewhere (docs/REFERENCE.md says how). `--check` is unchanged.
- What changed is shown after the binaries are verified, and says whether the attestation also
  covers the release's changelog (from this release on it does; a changelog other than the
  attested one, or a missing one, stops the update). `--check` labels it unverified.
- An older build served under a newer release's name is refused before it runs.
- An update makes 2 GitHub API requests instead of 1 (of the 60 an hour allowed without a login).
- Dependencies with security advisories updated (rustls, rustls-webpki, webpki-roots, tokio,
  anyhow).

## 2026-10-28 · `offers` shows every provider, Hetzner too; `--help` lists commands in groups
- `arena offers` is one table over every provider with a key in the config (RunPod, Vast,
  Hetzner): each row is an option `pods up` would try there, `#` the order it would try them in,
  with the size (VRAM, or vCPUs and RAM), the price per pod, where the price came from and the
  stock hint. `--provider X` shows that one provider, as before; Shadeform shows only that way.
- Hetzner's row is the VM `up --provider hetzner` would make (`--server-type`, else
  `HETZNER_SERVER_TYPE`), priced in € as Hetzner bills it.
- `arena offers --all` lists everything each provider sells: RunPod's GPU types (what `arena gpus`
  showed) and Hetzner's server types.
- A provider that can't be planned as `pods up` would (a `--gpu` typo, a server type Hetzner
  doesn't sell) is listed as `failed:` under the others, and `offers` exits non-zero.
- `arena gpus` still works and prints what it did, `--json` too, but is no longer listed in
  `--help`.
- `arena --help` and `arena pods --help` list the commands in groups (`pods`: Look, Make, Change,
  Move, Save, Stop) instead of one long list.

### Action needed
- A script that reads `arena offers --json`: the plan is now under `providers`, one per provider —
  read `.providers[] | select(.provider == "runpod")` (with `--provider X`, it's the only one).
  `arena gpus --json` is unchanged.

## 2026-10-28 · Commands over ssh work on a box whose login shell is fish
- On a box whose user's login shell is fish, arena's commands over ssh (`pods run`, setup, deep
  checks, backups, the dashboard's probe) now run under bash. Before, the dashboard's probe failed
  there (the row read `err`), and a command with a backslash in quotes could run changed.
- Boxes with bash, zsh or sh as the login shell still run every command themselves, with their
  startup files read once, as before.

## 2026-10-28 · Dashboard: rows at once, `D` checks all, graphs before PROGRESS
- The pods show as soon as they're listed; GPU stats, details and balances fill in after. The
  screen used to say `loading…` until every pod had been read over SSH.
- **`D`** deep-checks every running pod whose GPUs it has read and found no one using. A second
  `D` confirms, and checks only the pods the first one counted. Busy pods (any GPU with a
  process, over 5% utilization or over 1 GiB of memory) are named and left out; so are pods not
  read yet, pods whose GPUs couldn't be read, and CPU-only boxes.
- A question to `?` shows a spinner and its seconds in the footer until it's answered (after
  a notice, if one is up).
- The graphs come before PROGRESS, which only shows when `PROGRESS_CMD` is set. A CPU-only box
  (Hetzner) graphs its CPU and RAM, in magenta and blue.
- In HEALTH, a pass older than 26 hours is grey (a daily cron's drift kept).

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

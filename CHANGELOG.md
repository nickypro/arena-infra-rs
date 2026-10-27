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

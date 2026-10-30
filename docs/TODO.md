# Known issues

_Last checked against the code: 2026-10-30._ Something else got in your way? File it as a
[pain point](../README.md#tell-us-what-hurts); the open ones are the repo's
[`pain-point` issues](https://github.com/nickypro/arena-infra-rs/issues?q=is%3Aissue+is%3Aopen+label%3Apain-point).

## Copying a pod needs rsync on both pods
`replace` and `migrate copy` (direct pod-to-pod, and via this box) run `rsync` on the source and
destination pods. The ARENA image and Ubuntu VMs ship it; a bare image (e.g. `nvidia/cuda:*` via
`--image`) doesn't, and the copy dies with `rsync: command not found`. Nothing checks first. To do:
check `command -v rsync` on both ends before copying and install it (`apt-get install -y rsync`) when
missing.

## Migrate cutover hardening (review findings #6/#7)
- #6: a write-only proxy (`SSH_PROXY_RELOAD_CMD=""`) is said up front, and a cutover whose new pod
  can't be reached through the proxy is reverted. Still missing: a preflight that nginx actually
  reloads (`nginx -t` passes, as the user arena runs as) **before** any rename, so a cutover can't
  rename pods and only then find it can't drive the proxy.
- #7: `cutover_revert` makes one attempt at each rename and prints `REVERT WARNING: … fix names
  manually!` when one fails. It should retry, return a status, name the exact pod ids and names to
  fix, and check the original is reachable again before saying the revert worked.

## Migrate state machine (lower priority)
- No lock between runs: two concurrent `migrate`/`replace` runs on the same machine can create
  duplicate `-new` pods or clobber a rename (`migrate copy` does reuse an existing `-new`).
- A cutover interrupted between its two renames leaves no pod named `<name>`, so `migrate
  revert/status <name>` can't find it. Make recovery name-tolerant, or keep a small journal.

## Resolved since the last update (2026-06-25)
- **RunPod COMMUNITY pods with no public IP**: a v1 create asks for one (`supportPublicIp`; v2's
  create has no such field), `pods list` shows a proxy-only pod as `no public endpoint (RunPod ssh
  proxy only)`, and `pods up` replaces a pod still without an SSH endpoint after
  `--endpoint-timeout` (`ENDPOINT_TIMEOUT_MINS`, default 5).
- **The proxy's write-before-reload**: a failed reload puts the previous config back, locally and
  on a remote proxy (installed from a temp file in one SSH session), so the next sync retries.

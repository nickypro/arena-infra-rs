# TODO / known issues — pod migration & proxy

_Last updated: 2026-06-25._

## Pods with no public IP (COMMUNITY)
Some RunPod **COMMUNITY** pods come up with **no public IP** — they're only reachable
via RunPod's SSH proxy: `ssh <podid>-<hash>@ssh.runpod.io -i ~/.ssh/id_ed25519`. The tool's
`SshTarget::from_pod` builds `user@publicIp:port`, so for these pods it never gets an endpoint
("waiting for SSH endpoint" hangs forever) and can't SSH/rsync at all.

**SECURE cloud reliably gets a public IP** (directly SSH-able). COMMUNITY is a gamble.

TODO options:
- Detect a pod that's RUNNING but never gets a `publicIp`/`portMappings["22"]` within N seconds
  and either (a) fall back to the `ssh.runpod.io` proxy transport (`<podid>@ssh.runpod.io`), or
  (b) warn clearly and suggest SECURE, or (c) default `migrate`/`replace`-created pods to SECURE.
- For now: pass `--cloud SECURE` when creating pods you need to reach directly.

## Copy requires rsync on BOTH pods — check + install if missing
The copy (direct pod-to-pod and via-local) runs `rsync` on the source and dest pods. The arena
image ships it, but a bare base image (e.g. `nvidia/cuda:*`) does NOT, so the copy dies with
`rsync: command not found`. TODO: before copying, check `command -v rsync` on both ends and
`apt-get install -y rsync` (or equivalent) if missing, rather than assuming it's present.

## Proxy `deploy_proxy` write-before-reload landmine (review finding #5)
`deploy_proxy` writes the rendered nginx config to disk **before** `nginx -t && nginx -s reload`.
If the reload fails (e.g. run as non-root: can't read the letsencrypt cert, `nginx -t` fails),
the new, unvalidated config is left on disk — a landmine for the next root reload (logrotate,
certbot, reboot), which can then route a stable port to a wrong/dead endpoint.
TODO: write to a temp file, validate, and only `mv` into place + reload on success; restore the
previous file contents on failure (write-validate-swap).

## Migrate cutover hardening (review findings #6/#7) — partially pending
- #6: preflight that nginx is actually reloadable (root / `nginx -t` passes) **before** any
  rename, so a cutover can't rename pods and only then discover it can't drive the proxy.
- #7: `cutover_revert` should retry its renames and return a status; if the revert itself fails
  it must surface a loud "MANUAL FIX NEEDED" with the exact pod ids/names, and re-verify the
  original is reachable post-revert before claiming success.

## Migrate state-machine (review findings, lower priority)
- No locking: two concurrent `migrate`/`replace` runs on the same machine can create duplicate
  `-new` pods or clobber a rename. (`migrate copy` does reuse an existing `-new` before creating.)
- A cutover interrupted between the two renames leaves no pod named `<name>`; `resolve_migration`
  can't then find it to recover — make recovery name-tolerant / add a small journal.

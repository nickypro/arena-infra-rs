# RunPod API reference (as used by arena-infra-rs)

**Last updated: 2026-10-30**

> **REST v1 only.** Since 2026-10-19 the default backend is REST **v2** (`api.runpod.io/v2`,
> `provider/runpod_v2.rs` — its module doc is the v2 reference, GraphQL inventory included).
> The v2 list can omit `ssh.direct` and `runtime` even for running pods. `list_pods`
> fills missing endpoints with bounded detail requests (8 concurrent, 5 s each), using the
> TCP 22 runtime mapping as a fallback. An endpoint found is remembered while the pod keeps
> its `startedAt`, so a dashboard refresh costs no detail GET: rechecked every 30 s while the
> lookup was within 3 min of the start (a restart's detail keeps the old port for ~30 s), else
> every 10 min, at most 30 rechecks a minute per process. arena's own stop/start/restart forget
> it. A failed lookup leaves only that pod without an endpoint, and its `status_detail` says why.
> Running pods still missing SSH after 10 min show `no-ssh` and count as failed backup/run/pull work;
> the age is from `startedAt` or runtime uptime, never creation time.
> What follows describes REST v1, used only with an explicit `RUNPOD_API=v1` until RunPod
> retires it on 2026-11-16; after that every v1 call answers `410 Gone`, which arena reports as
> "remove RUNPOD_API=v1 (v2 is the default)".
>
> **Rate limits** (since 2026-10-27, every RunPod API: REST v2, REST v1 — 180/min, 3,600/h,
> 43,200/day, stepping down each Monday until its sunset — and GraphQL). Each API's quota is per
> key and shared by every process using it (the dashboards and the cron jobs alike). Every
> request goes through `crates/core/src/ratelimit.rs`: a `429`'s `Retry-After` is waited out (3
> retries, one wait at most 65 s, else the call fails at once saying how long), a spent or low
> window holds or spaces requests, and reads are resent twice after a 5xx or a network error
> (writes never). `arena api get runpod … --limits` shows what RunPod reports.

> Hand-maintained reference — it may drift from RunPod's live API. When in doubt, re-verify
> against the live OpenAPI spec at `https://rest.runpod.io/v1/openapi.json` and the RunPod web
> dashboard (watch its network tab for the GraphQL calls). See also the project memory note
> `runpod-rename-without-restart` for the rename detail.

This project talks to RunPod through **two** surfaces:

- **REST v1** — base URL `https://rest.runpod.io/v1` — the primary API for lifecycle.
- **GraphQL** — `https://api.runpod.io/graphql` — used only for what REST can't do
  safely/at all: the restart-free rename (`podEditName`), the GPU catalog with live prices
  (`gpuTypes`), and per-pod host details for `pods list` (`myself.pods`).

Auth: both use `Authorization: Bearer <API_KEY>` (verified for GraphQL 2026-10-07). Never
use GraphQL's `?api_key=<KEY>` query param: reqwest's error messages include the URL, so a
network error would print the key (`graphql_request_keeps_key_out_of_url` guards this).

All RunPod code lives in **`crates/core/src/provider/runpod.rs`** unless noted. Responses are
parsed defensively from `serde_json::Value`, so a RunPod schema tweak degrades a field to
`None` instead of crashing the tool.

---

## ⚠️ Critical gotchas (learned the hard way — do not forget)

1. **NEVER rename (or otherwise mutate) a pod via REST `PATCH /pods/{id}` or
   `POST /pods/{id}/update`.** RunPod documents these as *"Update a Pod, potentially
   triggering a reset"*, and **empirically they RESTART the container.** A container restart
   **resets the container disk back to the image** — total data loss for anything not on a
   network volume (`/workspace`). This caused real participant data loss when an earlier
   `replace`/`migrate` cutover renamed live pods this way. The tool never calls them to rename,
   lock or otherwise edit a pod; the one caller is `reimage_pod` (`pods reimage`), where the
   reset is the point (behind the same `--wipe-ok` gate as restart).

2. **The safe rename is the GraphQL `podEditName` mutation** — the exact mutation the RunPod
   web dashboard uses. It is **NOT** in RunPod's public graphql-spec, and it is **not**
   `podEditJob`. It is a pure metadata rename: **no restart, no data loss**, verified by
   comparing the container's `/proc/1` start time before and after (PATCH changes it;
   `podEditName` leaves it unchanged). Input is `PodEditNameInput = { podId, name }`.
   Used by `runpod.rs::rename_pod`.

3. **`GET /pods/{id}` returns an EMPTY `machine` object.** The **GPU type and cloud tier are
   NOT recoverable from the API** — `machine.gpuTypeId`, `machine.gpuDisplayName`, and
   `machine.secureCloud` all come back absent. `parse_spec` therefore returns blanks for GPU
   type / cloud tier and the caller falls back to the configured `GPU_TYPE` / `CLOUD_TYPE`
   (correct, since the fleet is provisioned uniformly from config).

4. **Pod SSH endpoints (ip:port) churn frequently** and the API's reported endpoint can lag.
   A recycled `ip:port` can belong to a **different** pod. Pod identity must therefore be
   verified out-of-band by reading **`RUNPOD_POD_ID` from `/proc/1/environ`** over SSH (it is
   **not** present as a plain env var in a non-interactive shell — only in PID 1's environ).
   See `crates/cli/src/pods/migrate.rs::proxy_reaches_pod` (and the push/cutover paths around it).
   **`lastStartedAt` from the REST API is NOT a reliable restart signal** — use the `/proc/1`
   start time via SSH instead.

5. **Some COMMUNITY hosts have no public IP.** A pod there is reachable only through RunPod's
   SSH proxy (`ssh <podid>-<hash>@ssh.runpod.io -i <key>`: no scp, rsync or port-forwards), which
   arena doesn't use. So every v1 create sends `supportPublicIp: true`; `pods list` shows such a
   pod as `no public endpoint (RunPod ssh proxy only)`, and `pods up` replaces one that stays so
   past `--endpoint-timeout` (v2's create has no such field).

6. **The container-disk-loss class of bug is only fully fixed with a network volume.** The
   fleet currently runs `volumeInGb=0`, so all data sits on ephemeral container disk — any
   container reset (rename-via-PATCH, restart, or RunPod migrating a contended pod) wipes it.

---

## REST v1 endpoints used

Base: `https://rest.runpod.io/v1`

| Method & path              | Purpose                                | Rust (`file:function`)                          |
|----------------------------|----------------------------------------|-------------------------------------------------|
| `GET /pods`                | List all pods                          | `runpod.rs::RunpodProvider::list_pods`          |
| `POST /pods`               | Create a pod                           | `runpod.rs::RunpodProvider::create_pod`         |
| `GET /pods/{id}`           | Get one pod (for spec recovery)        | `runpod.rs::RunpodProvider::pod_spec`           |
| `POST /pods/{id}/stop`     | Stop (deallocate) a pod                | `runpod.rs::RunpodProvider::stop_pod`           |
| `POST /pods/{id}/restart`  | Restart container in place             | `runpod.rs::RunpodProvider::restart_pod`        |
| `POST /pods/{id}/start`    | Start a stopped pod (`/restart` left an EXITED pod stopped) | `runpod.rs::RunpodProvider::start_pod` |
| `PATCH /pods/{id}`         | Reimage: new image + env, container reset | `runpod.rs::RunpodProvider::reimage_pod`     |
| `DELETE /pods/{id}`        | Terminate (destroy) a pod              | `runpod.rs::RunpodProvider::terminate_pod`      |
| `GET /openapi.json`        | Fetch OpenAPI spec (creatable GPU ids) | `runpod.rs::fetch_creatable_gpu_ids`            |

> **Deliberately NOT used** to rename or lock: `PATCH /pods/{id}` and `POST /pods/{id}/update` —
> see gotcha #1 (`PATCH` serves only `pods reimage`).

### `GET /pods` — list pods
Returns either a top-level JSON array or `{ "pods": [...] }` (both handled). Each entry is
mapped by `parse_pod`. The **list view's `machine` object is empty**, so GPU type is usually
absent here (see `parse_pod`'s `gpu_type` fallback chain).

### `POST /pods` — create pod
Body built by the pure, unit-tested `create_payload(spec)`:
```jsonc
{
  "name": "...",
  "imageName": "...",
  "gpuTypeIds": ["<gpu type id>"],   // array, even for a single type
  "gpuCount": 1,
  "cloudType": "COMMUNITY" | "SECURE",
  "containerDiskInGb": 200,
  "volumeInGb": 0,
  "ports": ["8888/http", "22/tcp"],  // comma-split from spec, array of strings
  "env": { "KEY": "VALUE", ... },     // object, not a list
  "dockerStartCmd": ["bash","-c","..."], // OPTIONAL — only when --bootstrap is set
  "allowedCudaVersions": ["12.8", ...], // the CUDA floor (or EXACT_CUDA_VERSIONS) in v1's enum, ≤ 13.0
  "supportPublicIp": true,             // always (gotcha #5)
  "dataCenterIds": ["EU-RO-1", ...]    // OPTIONAL — a NEAR option's data centres
}
```
`--api-json` / `CREATE_EXTRA_JSON` are deep-merged in last; the fields arena sets are refused.
**Gotchas:**
- `dockerStartCmd` is an **argv array** (overrides the image's CMD, keeps its ENTRYPOINT).
  Omit it entirely to let the image's own CMD run (the prebuilt arena image starts sshd
  itself). Do **not** send the old GraphQL field `dockerArgs` — REST rejects it with a 400.
- A stray/unknown key is a 400. Field names must match the REST schema exactly.
- The response is parsed by `parse_pod`; on create the `machine` fields may populate GPU type
  (fallback chain `machine.gpuDisplayName` → `machine.gpuType` → top-level `gpuTypeId`).

### `GET /pods/{id}` — get pod (spec recovery)
Parsed by the pure `parse_spec` into a recreate-able `PodSpec` for `replace`/`migrate`.
Reliably returns `imageName`, `containerDiskInGb`, `volumeInGb`, `gpuCount`, `ports`, `env`.
**`machine` is empty** → GPU type + cloud tier come back blank (gotcha #3). `name` is returned
blank for the caller to set; identity env vars `PUBLIC_KEY` and `MACHINE_NAME` are dropped
(the new pod re-seeds them). SSH endpoint fields: top-level `publicIp` + `portMappings` object
mapping container port → public port, e.g. `{"22": 1118}` (see `parse_pod`).

### `POST /pods/{id}/stop` / `POST /pods/{id}/restart` / `DELETE /pods/{id}`
- **stop** deallocates the pod (releases the GPU).
- **restart** restarts the container *in place* — keeps the pod (unlike stop/start, which
  deallocates). Live it kept the container disk (2026-10-09), but nothing promises that, so it is
  gated like a wipe (`--wipe-ok`).
- **DELETE** terminates/destroys the pod.

### `GET /openapi.json` — creatable GPU id enum
`fetch_creatable_gpu_ids` downloads the OpenAPI doc and `extract_gpu_enum` walks it for the
`gpuTypeIds` items-`enum` (under `PodCreateInput`). **The create-validation enum can LAG the
live `gpuTypes` catalog** — a GPU can be in stock yet rejected at create with a 400
("value must be one of …"). On v1 the catalog's rows are flagged against this enum: `arena offers
runpod` marks the rest `v1: not creatable`, and the hidden `gpus` table leaves them out (`--json`
keeps them, `creatable: false`).

---

## GraphQL operations used

Endpoint: `POST https://api.runpod.io/graphql` with `Authorization: Bearer <KEY>`
GraphQL returns **HTTP 200 even on logical errors** — the code must inspect the `errors` array
(`rename_pod` does exactly this).

### `podEditName` mutation (UNDOCUMENTED) — the safe rename
```graphql
mutation editPodName($input: PodEditNameInput!) {
  podEditName(input: $input) { id name }
}
```
Variables: `{ "input": { "podId": "<id>", "name": "<newname>" } }`
Used by `runpod.rs::rename_pod`. **This is the only correct way to rename** — see gotchas
#1/#2. Not in RunPod's public graphql-spec; mirror of what the dashboard sends.

### `gpuTypes` query — the GPU catalog
```graphql
{ gpuTypes { id displayName memoryInGb securePrice communityPrice
             lowestPrice(input: {gpuCount: 1}) { stockStatus } } }
```
Used by `runpod.rs::fetch_gpu_types`. The REST v1 API has no gpu-types route, so this GraphQL
query is the authoritative live list of valid `--gpu` names (including ones the local preset
table doesn't alias). `id` is the exact string to pass as `gpuTypeIds` on create. Prices are
$/h per GPU (0/null is treated as "tier not offered"); e.g. A4000 → community 0.17, secure 0.25, stock
"Low" (2026-10-07). If the priced query errors with no data, it retries the plain
`{ gpuTypes { id displayName memoryInGb } }`.

### `myself.pods` query — host details for `pods list`
```graphql
{ myself { pods { id gpuCount costPerHr
                  machine { gpuDisplayName maintenanceStart maintenanceEnd maintenanceNote } } } }
```
Used by `RunpodProvider::enrich` (best-effort and bounded: `pods list`, `up`, and the dashboard's
details round, once a minute). REST's
`machine` is empty, so this is the only source of the GPU name and the host maintenance
window. The maintenance fields' types aren't documented: strings are kept as-is, epoch
numbers (s or ms) become ISO UTC; null/empty = no window.

---

## RunPod GraphQL mutations — used vs. not used

RunPod's GraphQL surface exposes these pod mutations:

| Mutation                    | Documented? | Used by this tool? |
|-----------------------------|-------------|--------------------|
| `podBidResume`              | yes         | no                 |
| `podEditJob`                | yes         | no                 |
| `podFindAndDeployOnDemand`  | yes         | no (uses REST `POST /pods`) |
| `podRentInterruptable`      | yes         | no                 |
| `podResume`                 | yes         | no                 |
| `podStop`                   | yes         | no (uses REST `POST /pods/{id}/stop`) |
| `podTerminate`              | yes         | no (uses REST `DELETE /pods/{id}`) |
| **`podEditName`**           | **NO (undocumented)** | **YES** — `rename_pod` |

The tool prefers REST v1 for all lifecycle (create/stop/start/restart/terminate/list/get, and
reimage) and only drops to GraphQL for `podEditName` (restart-free rename) and read-only queries:
`gpuTypes`, `myself.pods`, the balance (`myself { clientBalance … }`) and network volumes
(`myself { networkVolumes … }`, for `teardown --check`).

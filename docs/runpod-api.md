# RunPod API reference (as used by arena-infra-rs)

**Last updated: 2026-06-25**

> Hand-maintained reference — it may drift from RunPod's live API. When in doubt, re-verify
> against the live OpenAPI spec at `https://rest.runpod.io/v1/openapi.json` and the RunPod web
> dashboard (watch its network tab for the GraphQL calls). See also the project memory note
> `runpod-rename-without-restart` for the rename detail.

This project talks to RunPod through **two** surfaces:

- **REST v1** — base URL `https://rest.runpod.io/v1` — the primary API for lifecycle.
- **GraphQL** — `https://api.runpod.io/graphql?api_key=<KEY>` — used only for the two things
  REST can't do safely/at all: the restart-free rename (`podEditName`) and the GPU catalog
  (`gpuTypes`).

Auth: REST uses `Authorization: Bearer <API_KEY>`. GraphQL passes the key as the
`?api_key=<KEY>` query param (no bearer header).

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
   `replace`/`migrate` cutover renamed live pods this way. The tool does **not** call these
   endpoints anywhere.

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
   See `crates/cli/src/main.rs::proxy_reaches_pod` (and the push/cutover paths around it).
   **`lastStartedAt` from the REST API is NOT a reliable restart signal** — use the `/proc/1`
   start time via SSH instead.

5. **Public IP availability depends on cloud tier.** SECURE-cloud pods reliably get a public
   IP and ARE directly SSH-reachable (`publicIp:portMappings["22"]`). COMMUNITY pods sometimes
   get **no public IP** — only reachable via RunPod's SSH proxy (`ssh <podid>-<hash>@ssh.runpod.io
   -i <key>`), which the tool's `SshTarget::from_pod` (raw `publicIp:port`) can't use, so such a
   pod looks like it "never gets an SSH endpoint." Use `--cloud SECURE` for pods you must reach
   directly. (See docs/TODO.md.)

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
| `DELETE /pods/{id}`        | Terminate (destroy) a pod              | `runpod.rs::RunpodProvider::terminate_pod`      |
| `GET /openapi.json`        | Fetch OpenAPI spec (creatable GPU ids) | `runpod.rs::fetch_creatable_gpu_ids`            |

> **Deliberately NOT used:** `PATCH /pods/{id}` and `POST /pods/{id}/update` — see gotcha #1.

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
  "dockerStartCmd": ["bash","-c","..."] // OPTIONAL — only when --bootstrap is set
}
```
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
- **restart** restarts the container *in place* — keeps the pod and its disk (unlike
  stop/start which deallocates). Note this still restarts the container.
- **DELETE** terminates/destroys the pod.

### `GET /openapi.json` — creatable GPU id enum
`fetch_creatable_gpu_ids` downloads the OpenAPI doc and `extract_gpu_enum` walks it for the
`gpuTypeIds` items-`enum` (under `PodCreateInput`). **The create-validation enum can LAG the
live `gpuTypes` catalog** — a GPU can be in stock yet rejected at create with a 400
("value must be one of …"). `arena gpus` intersects the live catalog with this enum so it only
advertises *creatable* types.

---

## GraphQL operations used

Endpoint: `POST https://api.runpod.io/graphql?api_key=<KEY>`
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
{ gpuTypes { id displayName memoryInGb } }
```
Used by `runpod.rs::fetch_gpu_types`. The REST v1 API has no gpu-types route, so this GraphQL
query is the authoritative live list of valid `--gpu` names (including ones the local preset
table doesn't alias). `id` is the exact string to pass as `gpuTypeIds` on create.

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

The tool prefers REST v1 for all lifecycle (create/stop/restart/terminate/list/get) and only
drops to GraphQL for `podEditName` (restart-free rename) and the `gpuTypes` catalog query.

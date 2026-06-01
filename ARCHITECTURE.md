# arena-infra-rs — architecture

A Cargo workspace with one library crate (`arena-core`) holding all logic, and two
thin presentation crates (`arena-cli`, `arena-tui`) that depend on it. Concrete
compute backends hide behind a single `Provider` trait, so the CLI/TUI — and any
future web layer — never mention a specific provider.

```mermaid
flowchart TD
    user([operator])

    subgraph present[presentation layer · thin, swappable]
        CLI["arena (CLI)<br/>clap · pods list/create/stop/terminate · proxy plan"]
        TUI["arena-tui<br/>ratatui · read-only pod dashboard"]
    end

    user --> CLI
    user --> TUI

    subgraph core[arena-core · library · all logic + safety]
        Config["Config<br/>parses config.env<br/>(KEY=val + MACHINE_NAME_LIST array)"]
        Provider{{"Provider (trait)<br/>list / create / stop / terminate"}}
        Runpod["RunpodProvider"]
        Vast["VastProvider<br/>(offer-search rent model)"]
        Naming["naming<br/>next_free_names()"]
        Proxy["proxy<br/>plan_forwards / render_nginx / render_tunnels"]
        Model["model: Pod · PodSpec<br/>errors: Error / Result"]

        Config --> Provider
        Provider --> Runpod
        Provider --> Vast
        Config --> Naming
        Config --> Proxy
        Provider -.->|"Pod list feeds"| Naming
        Provider -.->|"Pod list feeds"| Proxy
    end

    CLI --> Config
    CLI --> Provider
    CLI --> Naming
    CLI --> Proxy
    TUI --> Config
    TUI --> Provider

    cfg[("/home/dev/prod-ro/config.env<br/>read-only prod copy")]
    cfg -->|read| Config

    Runpod -->|HTTPS| RP[("rest.runpod.io/v1")]
    Vast -->|HTTPS| VA[("console.vast.ai/api/v0")]
```

## Crates

| crate | binary | role |
|-------|--------|------|
| `arena-core` | — | config parsing, `Provider` trait + RunPod/Vast backends, `Pod`/`PodSpec` model, `naming`, `proxy`, errors |
| `arena-cli` | `arena` | clap CLI over the library (`pods …`, `proxy plan`) |
| `arena-tui` | `arena-tui` | ratatui read-only dashboard |

## Request flow (example: `arena --provider vast pods create -n 3`)

1. `Config::load` reads `config.env` (the read-only prod copy by default).
2. `build_provider("vast", cfg)` constructs a `VastProvider` from `VAST_API_KEY`.
3. `provider.list_pods()` (read-only GET) gets the current pods.
4. `naming::next_free_names` picks the next free `arena8-*` names.
5. For each name a `PodSpec` is built; **dry-run prints what it would do** —
   `--apply` is required before `provider.create_pod()` actually mutates anything.

## Safety model (cross-cutting)

- **Read-only by default**: only `list_pods()` issues GETs; the TUI is GET-only.
- **Mutations are dry-run** unless `--apply` is passed.
- **`proxy plan` never connects** to the proxy host — it renders nginx config +
  SSH-tunnel commands for manual application (`--out` writes the config *locally*).
- **Secrets stay out of git**: `config.env` and `*.key` are git-ignored.

## Extending

- **New compute provider** → one new `impl Provider` in `crates/core/src/provider/`;
  no CLI/TUI changes.
- **New surface (e.g. web UI)** → a new crate depending on `arena-core`; all logic
  already lives in the library, so the core is untouched.

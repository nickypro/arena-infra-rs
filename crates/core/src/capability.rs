//! What works where: one table of which operations — and which create flags — each backend can
//! do, read by every command before it asks anything, by the dashboard's menus and by the docs.
//!
//! Before it (the human, 2026-10-30: "i'm slightly concerned now that there is not consistency
//! in all the commands working with all providers in general"), an optional operation was a
//! `Provider` method whose default returned `NotImplemented`, and each command found out by
//! trying: the prod dashboard offered `rename` on a Hetzner VM and failed after 3 s with "not
//! implemented"; `reimage` on a Hetzner VM went through the wipe gate and the confirm before it
//! failed on the spec read; `--disk` on a Hetzner create was dropped without a word.
//!
//! Now every such question is one cell of [`row`]: [`Support::Yes`], [`Support::Note`] (it works,
//! with what to know first) or [`Support::No`] with why and what to do instead. A provider
//! answers per pod through [`crate::Provider::supports`] — the fleet provider routes a pod to the
//! backend that listed it, a create to the create target — and nothing else decides it: no
//! command, menu or doc keeps its own list.
//!
//! It can't drift from the implementations unnoticed:
//! - `tests::every_cell_matches_what_the_backend_does`: for every backend and every operation,
//!   each provider call the operation needs ([`Op::needs`]) is made on the real backend, its HTTP
//!   pointed at a closed local port. A cell that says no must get `NotImplemented` from at least
//!   one of them (before any request); one that says yes must get none.
//! - `tests::every_flag_cell_matches_the_create_request`: a flag a backend is said to ignore
//!   leaves its create request as it was; one it is said to honour changes the request or the
//!   dry run's description.
//! - `tests::restart_and_stop_notes_match_the_backend`: the restart and stop cells say what the
//!   backend's `restart_resets_container` / `restart_wipes_container_disk` answer.
//! - `tests::every_instead_names_something_that_works_there`: an alternative that names a command
//!   names one that works on that backend.
//! - `tests::the_docs_carry_the_matrix`: docs/REFERENCE.md and FEATURE_MAP.md carry
//!   [`matrix_markdown`] between [`DOCS_BEGIN`] and [`DOCS_END`].
//!
//! **Adding a capability** — say Hetzner learns to replace a VM: implement the provider call(s)
//! the operation needs ([`Op::needs`]), flip the backend's line in [`row`] to `YES`, and paste the
//! new matrix into the two docs (the docs test prints it; `ARENA_BLESS_DOCS=1` writes it). The
//! drift test fails until the cell and the implementation agree.

use crate::config::Config;

/// One of arena's backends: a column of the table. RunPod's two REST generations are two
/// columns — both report as provider `runpod`, but they can't do the same things (only v2 locks)
/// — chosen by `RUNPOD_API` ([`crate::provider::RunpodApi`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// RunPod REST v2, the default.
    Runpod,
    /// RunPod REST v1 (`RUNPOD_API=v1`), retiring on [`crate::provider::RUNPOD_V1_RETIREMENT`].
    RunpodV1,
    Vast,
    Hetzner,
    Shadeform,
}

impl Backend {
    /// Every backend, in the table's column order.
    pub const ALL: [Backend; 5] = [Backend::Runpod, Backend::RunpodV1, Backend::Vast, Backend::Hetzner, Backend::Shadeform];

    /// The provider name its pods carry (`Pod::provider`, `--provider`).
    pub fn provider(self) -> &'static str {
        match self {
            Backend::Runpod | Backend::RunpodV1 => "runpod",
            Backend::Vast => "vast",
            Backend::Hetzner => "hetzner",
            Backend::Shadeform => "shadeform",
        }
    }

    /// Its name in the matrix and in refusals.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Runpod => "RunPod",
            Backend::RunpodV1 => "RunPod v1",
            Backend::Vast => "Vast",
            Backend::Hetzner => "Hetzner",
            Backend::Shadeform => "Shadeform",
        }
    }

    /// The backend `provider` (a provider name) means under `cfg`: RunPod's REST generation is
    /// `RUNPOD_API`'s (v2 when unset — and when it's malformed, which building the provider
    /// refuses anyway). `None`: not one of arena's providers.
    pub fn of(provider: &str, cfg: &Config) -> Option<Backend> {
        match Backend::named(provider) {
            Some(Backend::Runpod) if crate::provider::RunpodApi::from_config(cfg).ok() == Some(crate::provider::RunpodApi::V1) => {
                Some(Backend::RunpodV1)
            }
            b => b,
        }
    }

    /// The backend a provider name means, RunPod's as REST v2 (the default); `None`: not one of
    /// arena's providers.
    pub fn named(provider: &str) -> Option<Backend> {
        Backend::ALL.into_iter().find(|b| b.provider() == provider)
    }

    fn column(self) -> usize {
        Backend::ALL.iter().position(|b| *b == self).expect("every backend is in ALL")
    }
}

/// An operation whose support differs between backends: a command on a pod, or a create flag
/// (`pods create`/`up`, and `replace` / `migrate copy` where they take it). Commands that only
/// reach the pod over SSH (setup, backup, run, test, pull, …) work wherever SSH does and aren't
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// `pods start`.
    Start,
    /// `pods stop`.
    Stop,
    /// `pods restart` — and what it does to the disk ([`Support::Note`]).
    Restart,
    /// `pods terminate`.
    Terminate,
    /// `pods rename`.
    Rename,
    /// `pods reimage`.
    Reimage,
    /// `pods lock` / `unlock`, `up --lock`, `terminate --unlock`.
    Lock,
    /// `pods replace`.
    Replace,
    /// `pods migrate` (`copy` → `cutover` → `finish`).
    Migrate,
    /// Setup's repair of a pod that refuses the cohort key: its keys authorized again through
    /// the provider's API.
    KeyRepair,
    /// The auto-reaper (`pods reap`, `reap set`/`pin`, `up --expire-in/--expire-at/--idle-after`):
    /// whether it may ever terminate such a pod. Its judge asks this cell
    /// ([`crate::reap::judge::provider_supported`]); a yes also needs the reaper's identity and
    /// lock checks to know the backend (`reap::identity_condition`, `reap::lock_state`).
    Reap,
    /// `--gpu` / `--gpus`.
    Gpu,
    /// `--server-type`.
    ServerType,
    /// `--cloud`.
    Cloud,
    /// `--disk`.
    Disk,
    /// `--volume`.
    Volume,
    /// `--image`.
    Image,
    /// `--bootstrap`.
    Bootstrap,
    /// `--max-price`.
    MaxPrice,
    /// `--api-json` / `CREATE_EXTRA_JSON`.
    ApiJson,
}

/// A provider call an operation is made of, for the drift test ([`Op::needs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    StartPod,
    StopPod,
    RestartPod,
    TerminatePod,
    RenamePod,
    ReimagePod,
    SetLocked,
    PodSpec,
    AuthorizeSshKeys,
}

impl Op {
    /// Every operation, in the matrix's order: the commands on a pod, then the create flags.
    pub const ALL: [Op; 20] = [
        Op::Start,
        Op::Stop,
        Op::Restart,
        Op::Terminate,
        Op::Rename,
        Op::Reimage,
        Op::Lock,
        Op::Replace,
        Op::Migrate,
        Op::KeyRepair,
        Op::Reap,
        Op::Gpu,
        Op::ServerType,
        Op::Cloud,
        Op::Disk,
        Op::Volume,
        Op::Image,
        Op::Bootstrap,
        Op::MaxPrice,
        Op::ApiJson,
    ];

    /// What the operator types: `pods start`, `--disk`.
    pub fn what(self) -> &'static str {
        match self {
            Op::Start => "pods start",
            Op::Stop => "pods stop",
            Op::Restart => "pods restart",
            Op::Terminate => "pods terminate",
            Op::Rename => "pods rename",
            Op::Reimage => "pods reimage",
            Op::Lock => "pods lock / unlock",
            Op::Replace => "pods replace",
            Op::Migrate => "pods migrate",
            Op::KeyRepair => "setup's SSH key repair (via the API)",
            Op::Reap => "the auto-reaper",
            Op::Gpu => "--gpu / --gpus",
            Op::ServerType => "--server-type",
            Op::Cloud => "--cloud",
            Op::Disk => "--disk",
            Op::Volume => "--volume",
            Op::Image => "--image",
            Op::Bootstrap => "--bootstrap",
            Op::MaxPrice => "--max-price",
            Op::ApiJson => "--api-json",
        }
    }

    /// [`Self::what`] as the docs show it: a command or flag as code.
    fn shown(self) -> String {
        match self {
            Op::KeyRepair | Op::Reap => self.what().to_string(),
            _ => format!("`{}`", self.what()),
        }
    }

    /// A create/up (replace) flag rather than a command on a pod.
    pub fn is_flag(self) -> bool {
        matches!(
            self,
            Op::Gpu | Op::ServerType | Op::Cloud | Op::Disk | Op::Volume | Op::Image | Op::Bootstrap | Op::MaxPrice | Op::ApiJson
        )
    }

    /// The provider calls the operation is made of — what the drift test makes of each cell. A
    /// flag has none: its cells are checked against the create request instead.
    pub fn needs(self) -> &'static [Call] {
        match self {
            Op::Start => &[Call::StartPod],
            Op::Stop => &[Call::StopPod],
            Op::Restart => &[Call::RestartPod],
            Op::Terminate => &[Call::TerminatePod],
            Op::Rename => &[Call::RenamePod],
            // The pod's env is read first (`pods reimage` keeps it), then swapped.
            Op::Reimage => &[Call::PodSpec, Call::ReimagePod],
            Op::Lock => &[Call::SetLocked],
            // The source's spec is read back for the new pod, and the two are renamed past each
            // other (`<name>-new` → `<name>`, `<name>` → `<name>-old`).
            Op::Replace | Op::Migrate => &[Call::PodSpec, Call::RenamePod],
            Op::KeyRepair => &[Call::AuthorizeSshKeys],
            _ => &[],
        }
    }
}

/// One cell: whether a backend can do an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Yes,
    /// It works, with this to know first (the matrix's footnote).
    Note(&'static str),
    /// It doesn't: why, and what to do instead (`nothing to use instead` when there's nothing).
    No { why: &'static str, instead: &'static str },
}

impl Support {
    /// It works (with or without a note).
    pub fn works(self) -> bool {
        !matches!(self, Support::No { .. })
    }

    /// `why — instead`, for a refusal; `None` when it works.
    pub fn refusal(self) -> Option<String> {
        match self {
            Support::No { why, instead } => Some(format!("{why} — {instead}")),
            _ => None,
        }
    }

    /// As a `Result`, for `?`: `Err(why — instead)` when it doesn't work.
    pub fn check(self) -> Result<(), String> {
        self.refusal().map_or(Ok(()), Err)
    }
}

/// What backend `b` can do about `op` — the table's cell.
pub fn support(b: Backend, op: Op) -> Support {
    row(op)[b.column()]
}

/// The answer for a backend arena doesn't know (a provider name that isn't one of its own): no,
/// nothing to use instead. Production never asks it — every pod is listed by one of arena's
/// backends — but the fleet provider says it for a pod no configured backend claims.
pub const UNKNOWN_BACKEND: Support = Support::No {
    why: "no configured provider owns this pod (its provider's key may be missing from the config)",
    instead: "check `arena config check` and `arena pods list`",
};

const YES: Support = Support::Yes;

const fn note(text: &'static str) -> Support {
    Support::Note(text)
}

const fn no(why: &'static str, instead: &'static str) -> Support {
    Support::No { why, instead }
}

/// What to say instead when there's nothing to use instead.
pub const NOTHING_INSTEAD: &str = "nothing to use instead";

/// The way off a backend for an operation it lacks: a new pod by the same name.
const REMAKE: &str = "back it up (`arena pods backup <name>`), `arena pods terminate <name>`, then `arena pods up <name>` \
     and `arena pods restore <name>`";

const SHADEFORM_NO_STOP: &str = "Shadeform has no stop: an instance bills until it's deleted, idle or not";

/// THE TABLE: one row per operation, one line per backend in [`Backend::ALL`]'s order —
/// RunPod (v2), RunPod v1, Vast, Hetzner, Shadeform. Flipping a capability is a one-line change
/// here (then the docs: see the module doc).
const fn row(op: Op) -> [Support; 5] {
    match op {
        Op::Start => [
            YES, // RunPod
            YES, // RunPod v1
            YES, // Vast
            YES, // Hetzner
            no(SHADEFORM_NO_STOP, "nothing is ever stopped there, so there's nothing to start"), // Shadeform
        ],
        Op::Stop => [
            note("a stopped pod keeps no container disk: only a /workspace volume survives (`--wipe-ok` gate)"), // RunPod
            note("a stopped pod keeps no container disk: only a /workspace volume survives (`--wipe-ok` gate)"), // RunPod v1
            note("may lose the container disk (unverified): gated like a wipe"), // Vast
            note("keeps the disk: a graceful shutdown, powered off after 90 s if it doesn't"), // Hetzner
            no(SHADEFORM_NO_STOP, "back it up (`arena pods backup <name>`), then `arena pods terminate <name>`"), // Shadeform
        ],
        Op::Restart => [
            note("resets the container to its image: only a /workspace volume survives (`--wipe-ok` gate)"), // RunPod
            note("may reset the container (seen keeping it): gated like a wipe"), // RunPod v1
            note("may reset the container (a stop + start, unverified): gated like a wipe"), // Vast
            note("keeps the disk: a hard reset of the VM"), // Hetzner
            note("may reset the instance (unverified): gated like a wipe"), // Shadeform
        ],
        Op::Terminate => [YES, YES, YES, YES, YES],
        Op::Rename => [
            YES, // RunPod
            YES, // RunPod v1
            YES, // Vast: the instance label
            note("also sets the VM's hostname"), // Hetzner
            note("the new name must keep the `<prefix>-` prefix"), // Shadeform
        ],
        Op::Reimage => [
            YES, // RunPod
            YES, // RunPod v1
            no("Vast can't swap an instance's image in place", REMAKE), // Vast
            no("arena can't reimage a Hetzner VM yet (Hetzner's rebuild action could)", REMAKE), // Hetzner
            no("Shadeform can't swap an instance's image in place", REMAKE), // Shadeform
        ],
        Op::Lock => [
            YES, // RunPod
            no("only RunPod REST v2 can lock a pod", crate::provider::RUNPOD_V1_REMEDY), // RunPod v1
            no("Vast can't lock an instance", NOTHING_INSTEAD), // Vast
            no("arena can't lock a Hetzner VM yet (Hetzner's delete/rebuild protection could)", NOTHING_INSTEAD), // Hetzner
            no("Shadeform can't lock an instance", NOTHING_INSTEAD), // Shadeform
        ],
        Op::Replace => [
            YES, // RunPod
            YES, // RunPod v1
            no("arena can't read a Vast instance's spec back yet", REMAKE), // Vast
            no("arena can't read a Hetzner VM's spec back yet", REMAKE), // Hetzner
            note("not live-tested"), // Shadeform
        ],
        Op::Migrate => [
            YES, // RunPod
            YES, // RunPod v1
            no("arena can't read a Vast instance's spec back yet", REMAKE), // Vast
            no("arena can't read a Hetzner VM's spec back yet", REMAKE), // Hetzner
            no(
                "the cutover's proxy check reads RunPod's pod id, so it can't pass on Shadeform (it reverts itself)",
                "`arena pods replace <name>`",
            ), // Shadeform
        ],
        Op::KeyRepair => [
            no("RunPod authorizes keys only at create", "`arena pods replace <name>` (a new pod gets today's keys)"), // RunPod
            no("RunPod authorizes keys only at create", "`arena pods replace <name>` (a new pod gets today's keys)"), // RunPod v1
            YES, // Vast: the per-instance key attach, as at create
            no("Hetzner authorizes keys only at create (cloud-init)", REMAKE), // Hetzner
            no("Shadeform authorizes keys only at create (its bootstrap script)", "`arena pods replace <name>` (a new pod gets today's keys)"), // Shadeform
        ],
        Op::Reap => [
            YES, // RunPod
            no("the reaper acts on RunPod pods only through REST v2 (their lock state)", crate::provider::RUNPOD_V1_REMEDY), // RunPod v1
            no("the reaper never terminates a Vast instance: no lock state, no verified identity", NOTHING_INSTEAD), // Vast
            YES, // Hetzner
            no("the reaper doesn't read a Shadeform instance's identity (its create nonce) yet", NOTHING_INSTEAD), // Shadeform
        ],

        // The create flags (`pods create`/`up`; `replace` and `migrate copy` take --gpu, --gpus,
        // --cloud, --disk, --volume, --image and --max-price): a flag a backend ignores is refused
        // before anything is listed, priced or created — never dropped without a word.
        Op::Gpu => [
            YES, // RunPod
            YES, // RunPod v1
            YES, // Vast
            no(
                "Hetzner creates CPU VMs sized by server type",
                "pass --server-type (e.g. cx23, cpx42; `arena offers hetzner` lists them) or set HETZNER_SERVER_TYPE",
            ), // Hetzner
            note("each GPU type must be one Shadeform sells"), // Shadeform
        ],
        Op::ServerType => [
            no(SERVER_TYPE_IS_HETZNERS, SIZE_A_GPU_POD), // RunPod
            no(SERVER_TYPE_IS_HETZNERS, SIZE_A_GPU_POD), // RunPod v1
            no(SERVER_TYPE_IS_HETZNERS, SIZE_A_GPU_POD), // Vast
            YES, // Hetzner
            no(SERVER_TYPE_IS_HETZNERS, SIZE_A_GPU_POD), // Shadeform
        ],
        Op::Cloud => [
            note("COMMUNITY or SECURE"), // RunPod
            note("COMMUNITY or SECURE"), // RunPod v1
            note("host classes: datacenter, verified, unverified"), // Vast
            no("Hetzner has no tiers", "HETZNER_LOCATION picks where it runs"), // Hetzner
            no("Shadeform picks the cheapest cloud in stock", "set SHADEFORM_CLOUDS"), // Shadeform
        ],
        Op::Disk => [
            YES, // RunPod
            YES, // RunPod v1
            note("the offer's disk is at least this"), // Vast
            no("a Hetzner VM's disk comes with its server type", "pick a bigger --server-type (`arena offers hetzner`)"), // Hetzner
            no("a Shadeform instance's disk comes with its type", NOTHING_INSTEAD), // Shadeform
        ],
        Op::Volume => [
            note("a /workspace volume: what survives a stop, restart or reimage"), // RunPod
            note("a /workspace volume: what survives a stop, restart or reimage"), // RunPod v1
            no("arena's Vast create takes no volume", KEEP_WORK), // Vast
            no("arena can't attach a volume to a Hetzner VM yet (a VM keeps its disk across a stop and a restart)", KEEP_WORK), // Hetzner
            no("Shadeform's CUDA-13 cloud has no volumes", KEEP_WORK), // Shadeform
        ],
        Op::Image => [
            YES, // RunPod
            YES, // RunPod v1
            YES, // Vast
            note("the VM image: `arena` (the newest arena-built snapshot), a snapshot id or a name like ubuntu-24.04"), // Hetzner
            no("Shadeform picks the OS image by the CUDA floor", NOTHING_INSTEAD), // Shadeform
        ],
        Op::Bootstrap => [
            YES, // RunPod
            YES, // RunPod v1
            no("not needed on Vast: its launcher brings up SSH on any image", "leave it out"), // Vast
            no("a Hetzner VM runs no container start script: setup installs what it needs", "leave it out"), // Hetzner
            no("Shadeform instances come up through arena's own bootstrap script", "leave it out"), // Shadeform
        ],
        Op::MaxPrice => [
            YES, // RunPod
            YES, // RunPod v1
            YES, // Vast
            no("Hetzner quotes no price before a create", "`arena offers hetzner` lists each server type's price: pick the --server-type"), // Hetzner
            note("capped at SHADEFORM_MAX_PRICE too"), // Shadeform
        ],
        Op::ApiJson => [YES, YES, YES, YES, YES],
    }
}

const SERVER_TYPE_IS_HETZNERS: &str = "only Hetzner VMs are sized by server type";
const SIZE_A_GPU_POD: &str = "pass --provider hetzner, or size a GPU pod with --gpu / --gpus";
const KEEP_WORK: &str = "work survives through `arena pods backup` (and the archive)";

/// The line the generated matrix starts with in the docs ([`matrix_markdown`]).
pub const DOCS_BEGIN: &str =
    "<!-- capability matrix: generated from crates/core/src/capability.rs (its module doc says how) — don't edit by hand -->";
/// The line it ends with.
pub const DOCS_END: &str = "<!-- end of the capability matrix -->";

/// The docs' "What works where" matrix, from the table: one row per operation, one column per
/// backend; `✓`, or `✓ [n]` / `✗ [n]` with note `n` below saying what to know, or why not and
/// what to do instead. Between [`DOCS_BEGIN`] and [`DOCS_END`], as docs/REFERENCE.md and
/// FEATURE_MAP.md carry it (`tests::the_docs_carry_the_matrix`). Pure.
pub fn matrix_markdown() -> String {
    let mut out = vec![DOCS_BEGIN.to_string()];
    let head: Vec<&str> = Backend::ALL.iter().map(|b| b.label()).collect();
    out.push(format!("| | {} |", head.join(" | ")));
    out.push(format!("|---|{}", ":-:|".repeat(Backend::ALL.len())));
    // A note per distinct (operation, cell): backends with the same words share a number.
    let mut notes: Vec<(Op, Support, Vec<Backend>)> = Vec::new();
    let mut flags_started = false;
    for op in Op::ALL {
        if op.is_flag() && !flags_started {
            flags_started = true;
            out.push(format!("| **create flags** ({}) |{}", FLAGS_ON, " |".repeat(Backend::ALL.len())));
        }
        let cells: Vec<String> = Backend::ALL
            .iter()
            .map(|&b| {
                let s = support(b, op);
                if s == Support::Yes {
                    return "✓".to_string();
                }
                let n = match notes.iter().position(|(o, t, _)| *o == op && *t == s) {
                    Some(i) => {
                        notes[i].2.push(b);
                        i + 1
                    }
                    None => {
                        notes.push((op, s, vec![b]));
                        notes.len()
                    }
                };
                format!("{} [{n}]", if s.works() { "✓" } else { "✗" })
            })
            .collect();
        out.push(format!("| {} | {} |", op.shown(), cells.join(" | ")));
    }
    out.push(String::new());
    for (i, (op, s, on)) in notes.iter().enumerate() {
        let on: Vec<&str> = on.iter().map(|b| b.label()).collect();
        let text = match s {
            Support::No { why, instead: NOTHING_INSTEAD } => format!("no — {why}. Nothing to use instead."),
            Support::No { why, instead } => format!("no — {why}. Instead: {instead}."),
            Support::Note(t) => format!("{t}."),
            Support::Yes => unreachable!("a plain yes has no note"),
        };
        out.push(format!("{}. {} on {}: {text}", i + 1, op.shown(), on.join(", ")));
    }
    out.push(DOCS_END.to_string());
    out.join("\n")
}

/// Which commands take the create flags, for the matrix's heading row.
const FLAGS_ON: &str = "`pods create`/`up`; `replace` and `migrate copy` take --gpu, --gpus, --cloud, --disk, --volume, --image, --max-price";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::pod::{Pod, PodSpec};
    use crate::provider::{self, ContainerReset, Provider};

    const CFG: &str = "RUNPOD_API_KEY=k\nVAST_API_KEY=v\nHETZNER_API_KEY=h\nSHADEFORM_API_KEY=s\nMACHINE_NAME_PREFIX=devtest\n";

    /// Each backend as arena builds it, but every request it sends goes to a closed local port.
    fn offline(b: Backend) -> Box<dyn Provider> {
        let cfg = Config::parse(CFG);
        let client = crate::http::offline_client();
        match b {
            Backend::Runpod => Box::new(provider::runpod_v2::RunpodV2Provider::new("k").with_client(client)),
            Backend::RunpodV1 => Box::new(provider::runpod::RunpodProvider::new("k").with_client(client)),
            Backend::Vast => Box::new(provider::vast::VastProvider::from_config(&cfg).unwrap().with_client(client)),
            Backend::Hetzner => {
                Box::new(provider::hetzner::HetznerProvider::new("h", provider::hetzner::HetznerOpts::from_config(&cfg)).with_client(client))
            }
            Backend::Shadeform => Box::new(provider::shadeform::ShadeformProvider::from_config(&cfg).unwrap().with_client(client)),
        }
    }

    /// Each backend through `provider::build`, as the CLI gets it (no request is made).
    fn built(b: Backend) -> Box<dyn Provider> {
        let api = if b == Backend::RunpodV1 { "v1" } else { "v2" };
        provider::build(b.provider(), &Config::parse(&format!("{CFG}RUNPOD_API={api}\n"))).unwrap()
    }

    async fn call(p: &dyn Provider, c: Call) -> crate::Result<()> {
        let pod = Pod { id: "123".into(), name: "devtest-apple".into(), provider: p.name().into(), ..Default::default() };
        match c {
            Call::StartPod => p.start_pod("123").await,
            Call::StopPod => p.stop_pod("123").await,
            Call::RestartPod => p.restart_pod("123").await,
            Call::TerminatePod => p.terminate_pod("123").await,
            Call::RenamePod => p.rename_pod("123", "devtest-bravo").await,
            Call::ReimagePod => p.reimage_pod("123", "img:2", &[]).await,
            Call::SetLocked => p.set_locked("123", true).await,
            Call::PodSpec => p.pod_spec("123").await.map(|_| ()),
            Call::AuthorizeSshKeys => p.authorize_ssh_keys(&pod, &["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbc devtest".into()]).await,
        }
    }

    /// The cells that say no for a reason in arena's own commands, not a provider call the backend
    /// lacks: each with what stops it. (The test checks each is still a no.)
    const NO_BY_ARENA: &[(Backend, Op, &str)] = &[(
        Backend::Shadeform,
        Op::Migrate,
        "migrate cutover's proxy check reads RUNPOD_POD_ID (`proxy_reaches_pod`)",
    )];

    /// The operations made of no provider call and no create field: decided in arena's own code,
    /// which asks the table itself (the reaper's judge: `reap::judge::provider_supported`).
    const DECIDED_BY_ARENA: &[Op] = &[Op::Reap];

    /// The drift test: for every backend and every operation, the provider calls the operation is
    /// made of ([`Op::needs`]) are made on the real backend, offline. A cell that says no gets
    /// `NotImplemented` from at least one of them — before any request, so offline or not makes
    /// no difference — and one that says yes from none (each fails some other way: the request
    /// can't go anywhere). So a backend that learns an operation without its cell flipped, or a
    /// cell flipped without the implementation, fails here.
    #[tokio::test]
    async fn every_cell_matches_what_the_backend_does() {
        let mut bad = Vec::new();
        for b in Backend::ALL {
            let p = offline(b);
            assert_eq!(p.backend(), Some(b), "{b:?} says it's another column");
            for op in Op::ALL {
                if op.needs().is_empty() {
                    assert!(op.is_flag() || DECIDED_BY_ARENA.contains(&op), "{op:?}: no calls to check — say what decides it");
                    continue;
                }
                let mut missing = Vec::new();
                for &c in op.needs() {
                    if let Err(Error::NotImplemented(_)) = call(p.as_ref(), c).await {
                        missing.push(c);
                    }
                }
                let cell = support(b, op);
                let by_arena = NO_BY_ARENA.iter().any(|(nb, no, _)| *nb == b && *no == op);
                match (cell.works(), missing.is_empty(), by_arena) {
                    (true, true, false) | (false, false, false) | (false, true, true) => {}
                    (true, false, _) => bad.push(format!("{b:?} {op:?}: the table says yes, but {missing:?} aren't implemented")),
                    (false, true, false) => bad.push(format!("{b:?} {op:?}: the table says no, but every call it needs is implemented")),
                    (_, _, true) => bad.push(format!("{b:?} {op:?}: listed in NO_BY_ARENA, but the cell or the calls changed")),
                }
            }
        }
        assert!(bad.is_empty(), "the capability table and the backends disagree:\n{}", bad.join("\n"));
    }

    /// A spec every backend can preview a create for.
    fn base_spec() -> PodSpec {
        PodSpec {
            name: "devtest-apple".into(),
            image: "img:1".into(),
            gpu_type: "NVIDIA RTX A4000".into(),
            gpu_count: 1,
            cloud_type: "COMMUNITY".into(),
            disk_gb: 50,
            volume_gb: 0,
            ports: "22/tcp".into(),
            env: vec![("PUBLIC_KEY".into(), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAbc devtest".into())],
            docker_args: None,
            allowed_cuda: Vec::new(),
            min_cuda: None,
            max_price: None,
            api_extra: None,
            avoid_machines: Vec::new(),
            server_type: None,
            vm_image: None,
            region: None,
        }
    }

    /// `spec` with flag `op` given, as `create`/`up` set it for backend `b` (`None`: a flag
    /// checked elsewhere — see the test).
    fn with_flag(b: Backend, op: Op, mut spec: PodSpec) -> Option<PodSpec> {
        match op {
            Op::Gpu => {
                spec.gpu_type = "NVIDIA RTX A6000".into();
                spec.gpu_count = 2;
            }
            Op::ServerType => spec.server_type = Some("cx53".into()),
            // A value the backend's own tiers have, where it has tiers (Vast reads RunPod's
            // words as its default).
            Op::Cloud => spec.cloud_type = if b == Backend::Vast { "datacenter".into() } else { "SECURE".into() },
            Op::Disk => spec.disk_gb = 200,
            Op::Volume => spec.volume_gb = 100,
            // `--image` is the VM image on Hetzner (`pods::create::image_flag`).
            Op::Image if b == Backend::Hetzner => spec.vm_image = Some("ubuntu-22.04".into()),
            Op::Image => spec.image = "img:2".into(),
            Op::Bootstrap => spec.docker_args = Some(vec!["bash".into(), "-c".into(), "sshd".into()]),
            _ => return None,
        }
        Some(spec)
    }

    /// The flags' drift test: a flag a backend is said to ignore leaves its create request as it
    /// was (or is refused by the backend itself); one it is said to honour changes the request
    /// or what the dry run says it would create (`describe`: Vast's GPU and tier pick the offer,
    /// they aren't in the rent request). `--max-price` (placement's: prices are checked before any
    /// create) and `--api-json` (`provider::tests::every_backend_takes_api_extra…`) are checked
    /// where they act.
    #[test]
    fn every_flag_cell_matches_the_create_request() {
        let mut bad = Vec::new();
        for b in Backend::ALL {
            let p = built(b);
            let base = base_spec();
            let preview = |spec: &PodSpec| p.preview_create_body(spec);
            // What differs between two previews of one spec (Shadeform's per-create nonce) is
            // noise, left out of every comparison.
            let (one, two) = (preview(&base).unwrap_or_else(|e| panic!("{b:?}: {e}")), preview(&base).unwrap());
            let quiet = |mut v: serde_json::Value| {
                mask(&mut v, &one, &two);
                v
            };
            let body = quiet(one.clone());
            let said = p.describe(&base);
            for op in Op::ALL.into_iter().filter(|o| o.is_flag()) {
                let Some(spec) = with_flag(b, op, base_spec()) else { continue };
                let (body2, said2) = (preview(&spec).map(quiet), p.describe(&spec));
                let honoured = matches!(&body2, Ok(v) if *v != body) || said2 != said;
                let ignored = matches!(&body2, Ok(v) if *v == body);
                let refused = matches!(&body2, Err(Error::Config(_)));
                match support(b, op).works() {
                    true if !honoured || body2.is_err() => bad.push(format!("{b:?} {op:?}: said to work, but the request and description don't change ({body2:?})")),
                    false if !(ignored || refused) => bad.push(format!("{b:?} {op:?}: said to be ignored, but the create request changes")),
                    _ => {}
                }
            }
        }
        assert!(bad.is_empty(), "the capability table and the create requests disagree:\n{}", bad.join("\n"));
    }

    /// `v` with every leaf where `a` and `b` differ set to null.
    fn mask(v: &mut serde_json::Value, a: &serde_json::Value, b: &serde_json::Value) {
        use serde_json::Value;
        match (v, a, b) {
            (Value::Object(m), Value::Object(ma), Value::Object(mb)) => {
                for (k, x) in m.iter_mut() {
                    if let (Some(xa), Some(xb)) = (ma.get(k), mb.get(k)) {
                        mask(x, xa, xb);
                    }
                }
            }
            (Value::Array(v), Value::Array(va), Value::Array(vb)) => {
                for ((x, xa), xb) in v.iter_mut().zip(va).zip(vb) {
                    mask(x, xa, xb);
                }
            }
            (v, a, b) if a != b => *v = Value::Null,
            _ => {}
        }
    }

    /// The restart and stop cells say what the backend's own answers are — the ones the
    /// `--wipe-ok` gate and the confirm go by: a restart that resets says "resets", one that may
    /// says "may", one that keeps the disk says "keeps"; a stop that wipes never says it keeps.
    #[test]
    fn restart_and_stop_notes_match_the_backend() {
        for b in Backend::ALL {
            let p = built(b);
            let pod = Pod { provider: b.provider().into(), ..Default::default() };
            let Support::Note(restart) = support(b, Op::Restart) else { panic!("{b:?}: restart says nothing about the disk") };
            let want = match p.restart_resets_container(&pod) {
                ContainerReset::Yes => "resets",
                ContainerReset::Unknown => "may",
                ContainerReset::No => "keeps",
            };
            assert!(restart.starts_with(want), "{b:?}: restart's note {restart:?} should start with {want:?}");
            match support(b, Op::Stop) {
                Support::Note(stop) => {
                    assert_eq!(stop.starts_with("keeps"), !p.restart_wipes_container_disk(&pod), "{b:?}: stop's note {stop:?}")
                }
                s => assert!(!s.works(), "{b:?}: a stop says what it does to the disk"),
            }
        }
    }

    /// An alternative that names an arena command names one that works on that backend: "use
    /// `pods replace`" on a backend that can't replace would send the operator in a circle.
    #[test]
    fn every_instead_names_something_that_works_there() {
        let commands: Vec<(String, Op)> = [Op::Start, Op::Stop, Op::Restart, Op::Terminate, Op::Rename, Op::Reimage, Op::Replace, Op::Migrate]
            .into_iter()
            .map(|op| (format!("`arena {}", op.what()), op))
            .chain([("`arena pods lock".to_string(), Op::Lock), ("`arena pods unlock".to_string(), Op::Lock)])
            .collect();
        let flags = [("--server-type", Op::ServerType), ("--gpu", Op::Gpu), ("--volume", Op::Volume), ("--image", Op::Image)];
        for b in Backend::ALL {
            for op in Op::ALL {
                let Support::No { why, instead } = support(b, op) else { continue };
                for (cmd, needs) in &commands {
                    if instead.contains(cmd.as_str()) {
                        assert!(support(b, *needs).works(), "{b:?} {op:?}: instead names {cmd}`, which {b:?} can't do: {instead}");
                    }
                }
                // A flag in the alternative is for a create on this backend (not `--provider X`'s).
                if !instead.contains("--provider") {
                    for (flag, needs) in flags {
                        if instead.contains(flag) {
                            assert!(support(b, needs).works(), "{b:?} {op:?}: instead names {flag}, which {b:?} ignores: {instead}");
                        }
                    }
                }
                for text in [why, instead] {
                    assert!(!text.is_empty() && !text.ends_with('.') && !text.contains('|'), "{b:?} {op:?}: {text:?}");
                }
            }
        }
        for (b, op, _) in NO_BY_ARENA {
            assert!(!support(*b, *op).works(), "NO_BY_ARENA lists {b:?} {op:?}, which now works");
        }
    }

    /// The table's columns are `Backend::ALL`'s, and a provider name maps back to its column
    /// (RunPod's by `RUNPOD_API`).
    #[test]
    fn a_provider_name_maps_to_its_column() {
        let cfg = Config::parse("");
        for b in Backend::ALL.into_iter().filter(|b| *b != Backend::RunpodV1) {
            assert_eq!(Backend::of(b.provider(), &cfg), Some(b));
        }
        assert_eq!(Backend::of("runpod", &Config::parse("RUNPOD_API=v1")), Some(Backend::RunpodV1));
        assert_eq!(Backend::of("runpod", &Config::parse("RUNPOD_API=v3")), Some(Backend::Runpod), "malformed: build refuses it anyway");
        assert_eq!(Backend::of("lambda", &cfg), None);
        for b in Backend::ALL {
            assert_eq!(built(b).backend(), Some(b));
        }
    }

    /// docs/REFERENCE.md and FEATURE_MAP.md carry the matrix [`matrix_markdown`] makes from the
    /// table, between [`DOCS_BEGIN`] and [`DOCS_END`]. When the table changes, this fails with
    /// the new matrix to paste; `ARENA_BLESS_DOCS=1` writes it into both files instead.
    #[test]
    fn the_docs_carry_the_matrix() {
        let want = matrix_markdown();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let bless = std::env::var("ARENA_BLESS_DOCS").is_ok_and(|v| v == "1");
        let mut stale = Vec::new();
        for doc in ["docs/REFERENCE.md", "FEATURE_MAP.md"] {
            let path = root.join(doc);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{doc}: {e}"));
            let (Some(start), Some(end)) = (text.find(DOCS_BEGIN), text.find(DOCS_END)) else {
                panic!("{doc} has no capability matrix (the lines {DOCS_BEGIN:?} … {DOCS_END:?})");
            };
            let have = &text[start..end + DOCS_END.len()];
            if have != want {
                if bless {
                    std::fs::write(&path, format!("{}{want}{}", &text[..start], &text[end + DOCS_END.len()..])).unwrap();
                } else {
                    stale.push(doc);
                }
            }
        }
        assert!(
            stale.is_empty(),
            "{} out of date with crates/core/src/capability.rs — replace the matrix there with this (or run the test \
             with ARENA_BLESS_DOCS=1):\n{want}\n",
            stale.join(" and ")
        );
    }
}

//! Asking the capability table ([`arena_core::capability`]) about what a command was given —
//! its pods, its create flags — before anything is asked or done, and the words its refusals
//! use. Every command that does something only some providers can goes through here, so a pod
//! its provider can't do it for is refused (or, in a batch that skips such pods, named) with why
//! and what to do instead — never found out by trying after the confirm.

use anyhow::Result;
use arena_core::capability::{Op, Support};
use arena_core::{Pod, Provider};

/// `pods` split by whether their backend can do `op` ([`Provider::supports`]): the ones that
/// can't, each with why and what to do instead — then the rest, in order. Pure, no request.
pub(crate) fn split_by<'a>(provider: &dyn Provider, op: Op, pods: impl IntoIterator<Item = &'a Pod>) -> (Vec<(&'a Pod, String)>, Vec<&'a Pod>) {
    let mut cant = Vec::new();
    let mut can = Vec::new();
    for p in pods {
        match provider.supports(op, Some(p)).refusal() {
            Some(why) => cant.push((p, why)),
            None => can.push(p),
        }
    }
    (cant, can)
}

/// The pods that can't, one line per reason (pods with the same reason together): `apple, bloom
/// (hetzner): arena can't reimage a Hetzner VM yet (…) — back it up …`. Pure.
pub(crate) fn cant_lines(cant: &[(&Pod, String)]) -> Vec<String> {
    let mut groups: Vec<(&str, Vec<&Pod>)> = Vec::new();
    for (p, why) in cant {
        match groups.iter_mut().find(|(w, _)| *w == why.as_str()) {
            Some((_, pods)) => pods.push(p),
            None => groups.push((why, vec![p])),
        }
    }
    groups
        .into_iter()
        .map(|(why, pods)| {
            let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
            let mut providers: Vec<&str> = pods.iter().map(|p| p.provider.as_str()).collect();
            providers.dedup();
            format!("{} ({}): {why}", names.join(", "), providers.join(", "))
        })
        .collect()
}

/// The refusal of a command that acts on all of its pods or on none (`reimage`, `rename`,
/// `terminate`, `replace`): every pod whose backend can't do `op`, grouped by why, and how to
/// leave them out when others can; `None` when every pod can. `verb` as in "refusing to
/// reimage anything". Pure.
pub(crate) fn refusal(provider: &dyn Provider, op: Op, verb: &str, pods: &[&Pod]) -> Option<String> {
    let (cant, can) = split_by(provider, op, pods.iter().copied());
    if cant.is_empty() {
        return None;
    }
    let lines = cant_lines(&cant);
    let leave_out = if can.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = cant.iter().map(|(p, _)| p.name.as_str()).collect();
        format!("\n  (to {verb} the others, leave these out: --exclude {})", names.join(","))
    };
    Some(match (lines.as_slice(), pods.len()) {
        ([one], 1) => format!("refusing to {verb} {one}"),
        _ => format!("refusing to {verb} anything — {} of the {} pod(s) can't be:\n  {}{leave_out}", cant.len(), pods.len(), lines.join("\n  ")),
    })
}

/// The create flags given — `(flag, its operation)`, as typed — that `support` says the create
/// target ignores, refused: one line per operation, `--disk doesn't apply to hetzner: a Hetzner
/// VM's disk comes with its server type — pick a bigger --server-type …`. `provider`: where the
/// pods would be created. Before anything is listed, priced or created. Pure.
pub(crate) fn ignored_flags(provider: &str, given: &[(&str, Op)], support: impl Fn(Op) -> Support) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    let mut done: Vec<Op> = Vec::new();
    for (_, op) in given {
        if done.contains(op) {
            continue;
        }
        done.push(*op);
        if let Some(why) = support(*op).refusal() {
            let flags: Vec<&str> = given.iter().filter(|(_, o)| o == op).map(|(f, _)| *f).collect();
            lines.push(format!("{} doesn't apply to {provider}: {why}", flags.join(" / ")));
        }
    }
    if lines.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", lines.join("\n"))
    }
}

/// The flags of a create, replace or `migrate copy` that only some providers honour, as given
/// (`None`/`false`: not given). `--volume 0` is "no volume", which every provider does, so it
/// isn't one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CreateFlags<'a> {
    pub cloud: Option<&'a str>,
    pub disk: Option<u32>,
    pub volume: Option<u32>,
    pub image: Option<&'a str>,
    pub bootstrap: bool,
    pub max_price: Option<f64>,
}

impl CreateFlags<'_> {
    /// `(flag, its operation)` for each one given.
    pub(crate) fn given(&self) -> Vec<(&'static str, Op)> {
        [
            (self.cloud.is_some(), "--cloud", Op::Cloud),
            (self.disk.is_some(), "--disk", Op::Disk),
            (self.volume.is_some_and(|v| v > 0), "--volume", Op::Volume),
            (self.image.is_some(), "--image", Op::Image),
            (self.bootstrap, "--bootstrap", Op::Bootstrap),
            (self.max_price.is_some(), "--max-price", Op::MaxPrice),
        ]
        .into_iter()
        .filter(|(given, _, _)| *given)
        .map(|(_, flag, op)| (flag, op))
        .collect()
    }

    /// Refused where the create target — `provider`'s create (`pod: None`), or a new pod beside
    /// `pod` on its backend (replace, migrate) — would ignore one ([`ignored_flags`]).
    pub(crate) fn check(&self, provider: &dyn Provider, pod: Option<&Pod>) -> Result<()> {
        let name = pod.map_or(provider.name(), |p| p.provider.as_str());
        ignored_flags(name, &self.given(), |op| provider.supports(op, pod))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::capability::{self, Backend};

    /// A fleet's answers by provider name, as the table gives them (RunPod: v2).
    struct Fleet;

    #[async_trait::async_trait]
    impl Provider for Fleet {
        fn name(&self) -> &'static str {
            "hetzner"
        }
        fn backend(&self) -> Option<Backend> {
            Some(Backend::Hetzner)
        }
        fn supports(&self, op: Op, pod: Option<&Pod>) -> Support {
            let b = pod.map_or(Some(Backend::Hetzner), |p| Backend::named(&p.provider));
            b.map_or(capability::UNKNOWN_BACKEND, |b| capability::support(b, op))
        }
        fn describe(&self, _spec: &arena_core::PodSpec) -> String {
            String::new()
        }
        async fn list_pods(&self) -> arena_core::Result<Vec<Pod>> {
            Ok(Vec::new())
        }
        async fn create_pod(&self, _spec: &arena_core::PodSpec) -> arena_core::Result<Pod> {
            unreachable!()
        }
        async fn stop_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn restart_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
        async fn terminate_pod(&self, _id: &str) -> arena_core::Result<()> {
            unreachable!()
        }
    }

    fn pod(name: &str, provider: &str) -> Pod {
        Pod { id: format!("id-{name}"), name: format!("devtest-{name}"), provider: provider.into(), ..Default::default() }
    }

    /// A mixed selection: the pods that can't are named, grouped by why, with what to do
    /// instead and how to leave them out; one pod alone reads as one line; all able: nothing.
    #[test]
    fn a_refusal_names_each_pod_that_cant_and_why() {
        let (a, b, c) = (pod("apple", "runpod"), pod("bloom", "hetzner"), pod("cloud", "hetzner"));
        let r = refusal(&Fleet, Op::Reimage, "reimage", &[&a, &b, &c]).unwrap();
        let why = capability::support(Backend::Hetzner, Op::Reimage).refusal().unwrap();
        assert_eq!(
            r,
            format!(
                "refusing to reimage anything — 2 of the 3 pod(s) can't be:\n  devtest-bloom, devtest-cloud (hetzner): {why}\n  \
                 (to reimage the others, leave these out: --exclude devtest-bloom,devtest-cloud)"
            )
        );
        assert_eq!(refusal(&Fleet, Op::Reimage, "reimage", &[&b]).unwrap(), format!("refusing to reimage devtest-bloom (hetzner): {why}"));
        assert_eq!(refusal(&Fleet, Op::Reimage, "reimage", &[&a]), None);
        // Two reasons, two lines (no "leave out" when none could).
        let v = pod("delta", "vast");
        let r = refusal(&Fleet, Op::Reimage, "reimage", &[&b, &v]).unwrap();
        assert_eq!(r.lines().count(), 3, "{r}");
        assert!(r.contains("devtest-delta (vast): Vast can't swap") && !r.contains("--exclude"), "{r}");
        let e = pod("echo", "shadeform");
        let (cant, can) = split_by(&Fleet, Op::Stop, [&a, &e]);
        assert_eq!((cant.len(), can.len()), (1, 1));
        assert!(cant[0].1.starts_with("Shadeform has no stop"), "{}", cant[0].1);
    }

    /// A flag the create target ignores is refused, naming the flag(s), the provider, why and
    /// what instead; `--volume 0` (no volume) and flags it honours pass.
    #[test]
    fn a_flag_the_target_ignores_is_refused() {
        let flags = CreateFlags { disk: Some(100), volume: Some(0), image: Some("arena"), ..Default::default() };
        let e = flags.check(&Fleet, None).unwrap_err().to_string();
        assert!(e.starts_with("--disk doesn't apply to hetzner: a Hetzner VM's disk comes with its server type — pick a bigger --server-type"), "{e}");
        assert_eq!(e.lines().count(), 1, "--volume 0 and --image are fine there: {e}");
        let all = CreateFlags { cloud: Some("SECURE"), bootstrap: true, max_price: Some(0.5), ..Default::default() };
        let e = all.check(&Fleet, None).unwrap_err().to_string();
        assert_eq!(e.lines().count(), 3, "{e}");
        // The same flags beside a RunPod pod (a replace on RunPod): all honoured.
        all.check(&Fleet, Some(&pod("apple", "runpod"))).unwrap();
        CreateFlags::default().check(&Fleet, None).unwrap();
        let e = ignored_flags("hetzner", &[("--gpu", Op::Gpu), ("--gpus", Op::Gpu)], |op| capability::support(Backend::Hetzner, op)).unwrap_err().to_string();
        assert!(e.starts_with("--gpu / --gpus doesn't apply to hetzner: Hetzner creates CPU VMs sized by server type — pass --server-type"), "{e}");
    }
}

//! One target syntax for every fleet command (PLAN 1.C).
//!
//! Commands that act on "some pods" — `run`, `test`, `setup`, `copy-keys`, `keys`,
//! `reimage`, `stop`, `cp`, `set-branch`, `backup`, … — each used to have their own
//! targeting (a positional name, `--all`, `--include`/`--exclude`), with subtly different
//! matching. This module is the one parser + resolver they share, pure so it's table-tested:
//!
//! - **names**: bare `apple` (→ `{prefix}-apple`), full `arena8-apple`, absolute
//!   `@james-gpu` / `james-gpu` (an `@` list entry, see [`crate::naming`]), or a provider id;
//! - **ranges** `apple..mayor`: every name from `apple` to `mayor` inclusive, in
//!   `MACHINE_NAME_LIST` order (the order that also fixes the proxy ports). Both ends must be
//!   list entries, and a reversed range is an error rather than an empty one;
//! - **`all`** (or `--all`): every pod;
//! - **filters**: `--exclude <token>` (same syntax, ranges too), `--gpus N` (exactly N GPUs
//!   by the provider's count — an unknown count never matches, so `--gpus 1` can't sweep in
//!   a pod whose size we don't know), `--on <provider>`.
//!
//! A comma also separates tokens (`apple,bloom`), so one `--exclude` can name several.
//!
//! **Typos fail loudly.** A token — to include *or* to exclude — that matches no pod is an
//! error naming it plus the closest existing names: a misspelt target would quietly act on
//! fewer pods than asked, and a misspelt `--exclude` would act on the very pod the operator
//! meant to protect. A selection the operator narrowed (names or filters) that ends up empty
//! is an error too, saying how each step narrowed it. What *no targets at all* means is the
//! caller's decision ([`Selector::is_unscoped`]): read-only commands default to the whole
//! fleet, mutating ones demand names or `--all`.

use std::collections::HashSet;

use crate::naming::{canonical_name, qualify, ABSOLUTE_MARKER};
use crate::pod::Pod;

/// The providers `--on` accepts (the `provider` tag each backend puts on its pods).
pub const PROVIDERS: [&str; 3] = ["runpod", "vast", "hetzner"];

/// The reserved token meaning "every pod" (same as `--all`).
pub const ALL: &str = "all";

/// A selection the operator got wrong: a typo, a malformed range, a contradictory mix.
/// The message is written for the operator (it names the token and suggests fixes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SelectError(pub String);

/// The raw selection as typed — what the CLI's flags/positionals collect. Validated and
/// made sense of by [`Selector::parse`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectArgs {
    /// Names, ids, ranges or `all` (comma-separated pieces count as separate tokens).
    pub targets: Vec<String>,
    /// `--all`: every pod.
    pub all: bool,
    /// `--exclude`: tokens to leave out (same syntax as targets).
    pub exclude: Vec<String>,
    /// `--gpus N`: only pods with exactly N GPUs.
    pub gpus: Option<u32>,
    /// `--on <provider>`: only pods on that provider.
    pub on: Option<String>,
}

impl SelectArgs {
    /// Exactly these pods (full names or ids) — for a set the program chose itself (the
    /// pods `up` just created, the ones setup just provisioned), not operator input.
    pub fn exact<I, S>(targets: I) -> SelectArgs
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        SelectArgs { targets: targets.into_iter().map(Into::into).collect(), ..Default::default() }
    }

    /// Did the operator name pods (not just `all`/`--all`, not just filters)? A named pod
    /// that can't be reached is then a failure to report, not something to skip quietly.
    pub fn names_pods(&self) -> bool {
        self.targets.iter().flat_map(|t| pieces(t)).any(|t| t != ALL)
    }
}

/// How names are spelt for this fleet: the prefix and the ordered `MACHINE_NAME_LIST`.
#[derive(Debug, Clone, Copy)]
pub struct Naming<'a> {
    pub prefix: &'a str,
    pub list: &'a [String],
}

impl<'a> Naming<'a> {
    /// From config: `MACHINE_NAME_PREFIX` (default `arena`, as everywhere in the CLI) and
    /// the parsed `MACHINE_NAME_LIST`.
    pub fn from_config(cfg: &'a crate::Config) -> Naming<'a> {
        Naming { prefix: cfg.get("MACHINE_NAME_PREFIX").unwrap_or("arena"), list: &cfg.machine_names }
    }

    /// Does `token` name `pod`? Its provider id, its exact name, an `@name` absolute name,
    /// or a bare/full machine name (via [`canonical_name`], so an absolute list entry typed
    /// without its `@` works too). The single matching rule for every command.
    pub fn matches(&self, pod: &Pod, token: &str) -> bool {
        if (!pod.id.is_empty() && pod.id == token) || pod.name == token {
            return true;
        }
        match token.strip_prefix(ABSOLUTE_MARKER) {
            Some(bare) => pod.name == bare,
            None => pod.name == canonical_name(self.prefix, self.list, token),
        }
    }

    /// The `MACHINE_NAME_LIST` slot of a token that names a list entry (bare, full or
    /// absolute), if it does.
    fn slot_of_token(&self, token: &str) -> Option<usize> {
        let want = canonical_name(self.prefix, self.list, token);
        self.list.iter().position(|e| qualify(self.prefix, e) == want)
    }

    /// The `MACHINE_NAME_LIST` slot of a pod name, if it holds one.
    fn slot_of_name(&self, name: &str) -> Option<usize> {
        self.list.iter().position(|e| qualify(self.prefix, e) == name)
    }

    /// A pod name as the operator would type it: without the fleet prefix.
    fn short<'n>(&self, name: &'n str) -> &'n str {
        name.strip_prefix(self.prefix).and_then(|r| r.strip_prefix('-')).unwrap_or(name)
    }
}

/// One parsed token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A machine name in any accepted form, or a provider id (as typed).
    Name(String),
    /// `from..to` (as typed) and the full pod names it spans, in list order.
    Range { from: String, to: String, names: Vec<String> },
    /// `all`.
    All,
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Name(n) => write!(f, "{n}"),
            Token::Range { from, to, .. } => write!(f, "{from}..{to}"),
            Token::All => write!(f, "{ALL}"),
        }
    }
}

impl Token {
    /// Which pods (indices into `pods`) this token matches, in listing order.
    fn matching(&self, naming: &Naming, pods: &[Pod]) -> Vec<usize> {
        let hit = |p: &Pod| match self {
            Token::Name(t) => naming.matches(p, t),
            Token::Range { names, .. } => names.iter().any(|n| *n == p.name),
            Token::All => true,
        };
        (0..pods.len()).filter(|&i| hit(&pods[i])).collect()
    }
}

/// The non-empty, trimmed comma-separated pieces of one raw argument.
fn pieces(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Parse one piece (`all`, `a..b`, or a name/id). Ranges are checked against the list
/// here — before any listing — so a malformed one fails without touching a provider.
fn parse_token(piece: &str, naming: &Naming) -> Result<Token, String> {
    if piece == ALL {
        return Ok(Token::All);
    }
    let Some((from, to)) = piece.split_once("..") else {
        return Ok(Token::Name(piece.to_string()));
    };
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() || to.is_empty() || to.contains("..") {
        return Err(format!("`{piece}` is not a range: write `<first>..<last>`, e.g. apple..mayor"));
    }
    if naming.list.is_empty() {
        return Err(format!("`{piece}`: ranges follow MACHINE_NAME_LIST, which isn't set in config"));
    }
    let slot = |end: &str| {
        naming.slot_of_token(end).ok_or_else(|| {
            let near = closest(end, naming.list.iter().map(|e| e.trim_start_matches(ABSOLUTE_MARKER)), naming);
            format!("`{piece}`: `{end}` is not a MACHINE_NAME_LIST name (ranges follow the list order){}", did_you_mean(&near))
        })
    };
    let (a, b) = (slot(from)?, slot(to)?);
    if a > b {
        return Err(format!(
            "`{piece}` is reversed: {to} comes before {from} in MACHINE_NAME_LIST — did you mean {to}..{from}?"
        ));
    }
    let names = naming.list[a..=b].iter().map(|e| qualify(naming.prefix, e)).collect();
    Ok(Token::Range { from: from.to_string(), to: to.to_string(), names })
}

/// A validated selection, ready to [`resolve`](Selector::resolve) against a pod listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    /// What to act on (empty + `!all` = nothing named: see [`Selector::is_unscoped`]).
    pub include: Vec<Token>,
    /// `--all` or an `all` token.
    pub all: bool,
    pub exclude: Vec<Token>,
    pub gpus: Option<u32>,
    pub on: Option<String>,
}

impl Selector {
    /// Validate what was typed: token syntax, ranges against the list, `--on`, and the
    /// combinations that can't mean anything sensible. Every problem is reported at once.
    pub fn parse(args: &SelectArgs, naming: &Naming) -> Result<Selector, SelectError> {
        fn tokens(raw: &[String], naming: &Naming, problems: &mut Vec<String>) -> Vec<Token> {
            let mut out = Vec::new();
            for piece in raw.iter().flat_map(|r| pieces(r)) {
                match parse_token(piece, naming) {
                    Ok(t) if !out.contains(&t) => out.push(t),
                    Ok(_) => {} // a repeated token adds nothing
                    Err(e) => problems.push(e),
                }
            }
            out
        }
        let mut problems = Vec::new();
        let mut include = tokens(&args.targets, naming, &mut problems);
        let exclude = tokens(&args.exclude, naming, &mut problems);

        let all_token = include.contains(&Token::All);
        include.retain(|t| *t != Token::All);
        if !include.is_empty() && args.all {
            problems.push("pass target names or --all, not both (use --exclude to leave pods out of --all)".into());
        } else if !include.is_empty() && all_token {
            problems.push("`all` already selects every pod — drop the names (use --exclude to leave pods out)".into());
        }
        if exclude.contains(&Token::All) {
            problems.push("--exclude all would leave nothing to act on".into());
        }
        let on = match args.on.as_deref().map(str::trim) {
            None => None,
            Some(p) => match PROVIDERS.iter().find(|k| k.eq_ignore_ascii_case(p)) {
                Some(k) => Some(k.to_string()),
                None => {
                    problems.push(format!("--on `{p}`: not a provider (one of {})", PROVIDERS.join(", ")));
                    None
                }
            },
        };
        if !problems.is_empty() {
            return Err(SelectError(problems.join("\n")));
        }
        Ok(Selector { include, all: args.all || all_token, exclude, gpus: args.gpus, on })
    }

    /// Nothing named and no `--all`: the caller decides what that means (read-only commands
    /// take the whole fleet; mutating ones refuse). Filters alone don't scope a selection.
    pub fn is_unscoped(&self) -> bool {
        self.include.is_empty() && !self.all
    }

    /// Did the operator narrow the fleet (names, `--exclude`, `--gpus`, `--on`)? Then an
    /// empty result means they got something wrong, so it's an error. `--all` (or no
    /// targets) on an empty fleet is just an empty fleet.
    fn narrows(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty() || self.gpus.is_some() || self.on.is_some()
    }

    /// The selection in words, for prompts and errors: `apple..mayor bloom, excluding
    /// cloud, with 2 GPUs, on runpod`.
    pub fn describe(&self) -> String {
        let mut s = if self.include.is_empty() {
            "all pods".to_string()
        } else {
            self.include.iter().map(Token::to_string).collect::<Vec<_>>().join(" ")
        };
        if !self.exclude.is_empty() {
            s += &format!(", excluding {}", self.exclude.iter().map(Token::to_string).collect::<Vec<_>>().join(" "));
        }
        if let Some(n) = self.gpus {
            s += &format!(", with {n} GPU{}", if n == 1 { "" } else { "s" });
        }
        if let Some(p) = &self.on {
            s += &format!(", on {p}");
        }
        s
    }

    /// Resolve against a listing: the selected pods as indices into `pods`, de-duplicated
    /// (by position — two pods sharing a name are two pods, each selected) and ordered by
    /// `MACHINE_NAME_LIST` slot (the proxy-port order), then name for pods off the list.
    ///
    /// Every include and exclude token must match at least one listed pod, else it's an
    /// error listing each such token with the closest names. Then `--exclude`, `--on` and
    /// `--gpus` narrow; a narrowed selection that ends up empty is an error saying how
    /// each step narrowed it. Unscoped (see [`is_unscoped`](Self::is_unscoped)) = every pod.
    pub fn resolve(&self, naming: &Naming, pods: &[Pod]) -> Result<Vec<usize>, SelectError> {
        let mut misses = Vec::new();
        let mut chosen: Vec<usize> = Vec::new();
        if self.include.is_empty() {
            chosen = (0..pods.len()).collect();
        } else {
            let mut seen = HashSet::new();
            for tok in &self.include {
                let hits = tok.matching(naming, pods);
                if hits.is_empty() {
                    misses.push(miss_line("", tok, naming, pods));
                }
                chosen.extend(hits.into_iter().filter(|i| seen.insert(*i)));
            }
        }
        let mut excluded = HashSet::new();
        for tok in &self.exclude {
            let hits = tok.matching(naming, pods);
            if hits.is_empty() {
                misses.push(miss_line("--exclude ", tok, naming, pods));
            }
            excluded.extend(hits);
        }
        if !misses.is_empty() {
            return Err(SelectError(format!(
                "{} matched no pod — nothing was done:\n  {}\n(see `arena pods list`)",
                if misses.len() == 1 { "a target" } else { "some targets" },
                misses.join("\n  ")
            )));
        }

        // Narrow, keeping a trace of the counts for the "ended up empty" error.
        let mut trace = vec![format!("{} → {}", self.include_words(), chosen.len())];
        if !self.exclude.is_empty() {
            chosen.retain(|i| !excluded.contains(i));
            let ex: Vec<String> = self.exclude.iter().map(Token::to_string).collect();
            trace.push(format!("--exclude {} → {}", ex.join(" "), chosen.len()));
        }
        if let Some(p) = &self.on {
            chosen.retain(|&i| pods[i].provider == *p);
            trace.push(format!("--on {p} → {}", chosen.len()));
        }
        if let Some(n) = self.gpus {
            let before = chosen.clone();
            chosen.retain(|&i| pods[i].gpu_count == Some(n));
            let unknown = before.iter().filter(|&&i| pods[i].gpu_count.is_none()).count();
            let note = if unknown > 0 { format!(" ({unknown} with an unknown GPU count)") } else { String::new() };
            trace.push(format!("--gpus {n} → {}{note}", chosen.len()));
        }
        if chosen.is_empty() && self.narrows() {
            return Err(SelectError(format!(
                "nothing selected: {} — nothing was done (see `arena pods list`)",
                trace.join(", ")
            )));
        }

        chosen.sort_by(|&a, &b| {
            let key = |i: usize| (naming.slot_of_name(&pods[i].name).unwrap_or(usize::MAX), &pods[i].name, i);
            key(a).cmp(&key(b))
        });
        Ok(chosen)
    }

    /// The include side in words for the trace (`all pods (3)` / `apple bloom`).
    fn include_words(&self) -> String {
        if self.include.is_empty() {
            "all pods".to_string()
        } else {
            self.include.iter().map(Token::to_string).collect::<Vec<_>>().join(" ")
        }
    }
}

/// Resolve exactly one pod for a single-target command (`terminate`, `restart`, `rename`,
/// `replace`, …) with the same matcher: a range or `all` is refused (those commands take
/// one pod on purpose), no match is the typo error, and a token matching two pods (a double
/// create left two with one name) is refused — acting on whichever came first could be the
/// wrong one; the ids disambiguate.
pub fn resolve_one(naming: &Naming, pods: &[Pod], token: &str) -> Result<usize, SelectError> {
    let token = token.trim();
    match parse_token(token, naming).map_err(SelectError)? {
        Token::Name(_) => {}
        Token::All => {
            return Err(SelectError("this command takes one pod — `all` isn't accepted here (see its --all flag, if any)".into()))
        }
        Token::Range { .. } => {
            return Err(SelectError(format!("this command takes one pod — `{token}` is a range")))
        }
    }
    let tok = Token::Name(token.to_string());
    match tok.matching(naming, pods).as_slice() {
        [i] => Ok(*i),
        [] => Err(SelectError(format!("{} (see `arena pods list`)", miss_line("", &tok, naming, pods)))),
        many => {
            let ids: Vec<String> = many.iter().map(|&i| format!("{} (id {})", pods[i].name, pods[i].id)).collect();
            Err(SelectError(format!(
                "`{token}` matches {} pods: {} — pass the id of the one you mean",
                many.len(),
                ids.join(", ")
            )))
        }
    }
}

/// One line of the "matched no pod" error for `tok`: what it is and the nearest names.
fn miss_line(flag: &str, tok: &Token, naming: &Naming, pods: &[Pod]) -> String {
    match tok {
        Token::Name(t) => {
            let near = closest(t, pods.iter().map(|p| p.name.as_str()), naming);
            if naming.slot_of_token(t).is_some() {
                // A real list name with no pod right now isn't a typo — say that instead.
                format!(
                    "{flag}`{t}`: no pod named {} right now (it is a MACHINE_NAME_LIST name){}",
                    canonical_name(naming.prefix, naming.list, t),
                    did_you_mean(&near)
                )
            } else {
                format!("{flag}`{t}`: no pod has that name or id{}", did_you_mean(&near))
            }
        }
        Token::Range { names, .. } => format!(
            "{flag}`{tok}`: no pod in that range ({} name{}: {} … {})",
            names.len(),
            if names.len() == 1 { "" } else { "s" },
            names.first().map(String::as_str).unwrap_or(""),
            names.last().map(String::as_str).unwrap_or("")
        ),
        Token::All => format!("{flag}`all`: the fleet is empty"),
    }
}

fn did_you_mean(near: &[String]) -> String {
    if near.is_empty() {
        String::new()
    } else {
        format!(" — did you mean {}?", near.join(" or "))
    }
}

/// Up to three `candidates` within a small edit distance of `token`, closest first. Both
/// sides are compared with and without the fleet prefix (and `@`), so `aple` finds
/// `arena8-apple` and `arena8-aple` finds it too. The allowance grows with the token's
/// length (1 edit for short names, up to 3) so short typos don't suggest everything.
fn closest<'c>(token: &str, candidates: impl Iterator<Item = &'c str>, naming: &Naming) -> Vec<String> {
    let t = token.trim_start_matches(ABSOLUTE_MARKER);
    let t_short = naming.short(t);
    let limit = (t_short.chars().count() / 3).clamp(1, 3);
    let mut scored: Vec<(usize, String)> = Vec::new();
    for c in candidates {
        let d = edit_distance(t, c).min(edit_distance(t_short, naming.short(c)));
        if d > 0 && d <= limit && !scored.iter().any(|(_, n)| n == c) {
            scored.push((d, c.to_string()));
        }
    }
    scored.sort();
    scored.into_iter().take(3).map(|(_, n)| n).collect()
}

/// Levenshtein distance (insert/delete/substitute = 1), over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list: apple … mayor, with an absolute `@james-gpu` in the middle.
    fn list() -> Vec<String> {
        ["apple", "autumn", "bloom", "@james-gpu", "cloud", "mayor", "zebra"].iter().map(|s| s.to_string()).collect()
    }

    fn pod(name: &str, provider: &str, gpus: Option<u32>) -> Pod {
        Pod {
            id: format!("id-{name}"),
            name: name.into(),
            provider: provider.into(),
            status: "RUNNING".into(),
            gpu_count: gpus,
            ..Default::default()
        }
    }

    /// A fleet listed out of order: apple/bloom/cloud/mayor on runpod (1, 2, ?, 2 GPUs),
    /// james-gpu (absolute) on vast, a hetzner CPU VM, and a pod off the list.
    fn fleet() -> Vec<Pod> {
        vec![
            pod("arena8-mayor", "runpod", Some(2)),
            pod("arena8-bloom", "runpod", Some(2)),
            pod("arena8-apple", "runpod", Some(1)),
            pod("james-gpu", "vast", Some(1)),
            pod("arena8-cloud", "runpod", None),
            pod("arena8-zebra", "hetzner", None),
            pod("arena8-apple-old", "runpod", Some(1)),
        ]
    }

    fn args(targets: &[&str]) -> SelectArgs {
        SelectArgs { targets: targets.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    /// Parse + resolve `a` against [`fleet`]; the selected names, or the error text.
    fn run(a: &SelectArgs) -> Result<Vec<String>, String> {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let pods = fleet();
        let sel = Selector::parse(a, &naming).map_err(|e| e.0)?;
        let got = sel.resolve(&naming, &pods).map_err(|e| e.0)?;
        Ok(got.into_iter().map(|i| pods[i].name.clone()).collect())
    }

    fn ok(a: &SelectArgs) -> Vec<String> {
        run(a).unwrap_or_else(|e| panic!("{a:?}: {e}"))
    }

    fn err(a: &SelectArgs) -> String {
        match run(a) {
            Ok(got) => panic!("{a:?} should fail, got {got:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn names_in_every_form_resolve_in_list_order() {
        let cases: &[(&[&str], &[&str])] = &[
            // bare, full, id, absolute with and without `@`
            (&["bloom"], &["arena8-bloom"]),
            (&["arena8-bloom"], &["arena8-bloom"]),
            (&["id-arena8-bloom"], &["arena8-bloom"]),
            (&["james-gpu"], &["james-gpu"]),
            (&["@james-gpu"], &["james-gpu"]),
            // an off-list pod by its exact name (a replace leftover)
            (&["arena8-apple-old"], &["arena8-apple-old"]),
            // list order, not typed order; duplicates (any spelling) once
            (&["mayor", "apple", "arena8-apple", "id-arena8-apple"], &["arena8-apple", "arena8-mayor"]),
            // commas split tokens
            (&["mayor,bloom", " apple , "], &["arena8-apple", "arena8-bloom", "arena8-mayor"]),
        ];
        for (targets, want) in cases {
            assert_eq!(ok(&args(targets)), *want, "{targets:?}");
        }
    }

    #[test]
    fn everything_unscoped_or_all_is_list_order_then_name() {
        let everyone =
            ["arena8-apple", "arena8-bloom", "james-gpu", "arena8-cloud", "arena8-mayor", "arena8-zebra", "arena8-apple-old"];
        assert_eq!(ok(&SelectArgs::default()), everyone);
        assert_eq!(ok(&SelectArgs { all: true, ..Default::default() }), everyone);
        assert_eq!(ok(&args(&["all"])), everyone);
        // `all` twice / `all` with --all is still just all.
        assert_eq!(ok(&SelectArgs { targets: vec!["all".into(), "all".into()], all: true, ..Default::default() }), everyone);
    }

    #[test]
    fn ranges_follow_the_list_inclusive() {
        let cases: &[(&[&str], &[&str])] = &[
            // autumn has no pod: a range may span spare names
            (&["apple..bloom"], &["arena8-apple", "arena8-bloom"]),
            // an absolute name inside the range, and as an endpoint (with or without `@`)
            (&["bloom..cloud"], &["arena8-bloom", "james-gpu", "arena8-cloud"]),
            (&["james-gpu..mayor"], &["james-gpu", "arena8-cloud", "arena8-mayor"]),
            (&["@james-gpu..cloud"], &["james-gpu", "arena8-cloud"]),
            // full-name endpoints; a one-name range
            (&["arena8-cloud..arena8-mayor"], &["arena8-cloud", "arena8-mayor"]),
            (&["mayor..mayor"], &["arena8-mayor"]),
            // overlapping ranges + a name: each pod once, list order
            (&["cloud..zebra", "apple..bloom", "mayor"], &["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-mayor", "arena8-zebra"]),
        ];
        for (targets, want) in cases {
            assert_eq!(ok(&args(targets)), *want, "{targets:?}");
        }
    }

    #[test]
    fn bad_ranges_fail_before_any_listing() {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let parse = |t: &str| Selector::parse(&args(&[t]), &naming).unwrap_err().0;
        let cases: &[(&str, &str)] = &[
            ("mayor..apple", "`mayor..apple` is reversed: apple comes before mayor in MACHINE_NAME_LIST — did you mean apple..mayor?"),
            ("aple..mayor", "`aple..mayor`: `aple` is not a MACHINE_NAME_LIST name (ranges follow the list order) — did you mean apple?"),
            ("apple..ghost", "`apple..ghost`: `ghost` is not a MACHINE_NAME_LIST name (ranges follow the list order)"),
            // an id is not a list position
            ("id-arena8-apple..mayor", "`id-arena8-apple` is not a MACHINE_NAME_LIST name"),
            // `@` means absolute: `@apple` is not the prefixed apple
            ("@apple..mayor", "`@apple` is not a MACHINE_NAME_LIST name"),
            ("apple..", "`apple..` is not a range"),
            ("..mayor", "`..mayor` is not a range"),
            ("apple..bloom..cloud", "`apple..bloom..cloud` is not a range"),
        ];
        for (tok, want) in cases {
            let e = parse(tok);
            assert!(e.contains(want), "{tok}: {e}");
        }
        // No list configured: ranges can't mean anything.
        let none = Naming { prefix: "arena8", list: &[] };
        let e = Selector::parse(&args(&["apple..mayor"]), &none).unwrap_err().0;
        assert!(e.contains("MACHINE_NAME_LIST, which isn't set"), "{e}");
        // Every problem at once.
        let e = Selector::parse(&SelectArgs { exclude: vec!["mayor..apple".into()], ..args(&["apple.."]) }, &naming)
            .unwrap_err()
            .0;
        assert_eq!(e.lines().count(), 2, "{e}");
    }

    #[test]
    fn exclude_takes_the_same_tokens_including_ranges() {
        let cases: &[(SelectArgs, &[&str])] = &[
            (SelectArgs { all: true, exclude: vec!["apple..cloud".into()], ..Default::default() }, &["arena8-mayor", "arena8-zebra", "arena8-apple-old"]),
            (SelectArgs { exclude: vec!["james-gpu,id-arena8-zebra".into(), "arena8-apple-old".into()], ..args(&["all"]) }, &["arena8-apple", "arena8-bloom", "arena8-cloud", "arena8-mayor"]),
            // exclude wins over include
            (SelectArgs { exclude: vec!["bloom".into()], ..args(&["apple..cloud"]) }, &["arena8-apple", "james-gpu", "arena8-cloud"]),
        ];
        for (a, want) in cases {
            assert_eq!(ok(a), *want, "{a:?}");
        }
    }

    #[test]
    fn gpus_and_on_filter_and_an_unknown_gpu_count_never_matches() {
        let cases: &[(SelectArgs, &[&str])] = &[
            (SelectArgs { gpus: Some(2), ..Default::default() }, &["arena8-bloom", "arena8-mayor"]),
            // cloud (runpod, count unknown) and zebra (hetzner) are never "1 GPU"
            (SelectArgs { gpus: Some(1), ..Default::default() }, &["arena8-apple", "james-gpu", "arena8-apple-old"]),
            (SelectArgs { on: Some("vast".into()), ..Default::default() }, &["james-gpu"]),
            (SelectArgs { on: Some("Hetzner".into()), all: true, ..Default::default() }, &["arena8-zebra"]),
            (SelectArgs { on: Some("runpod".into()), gpus: Some(1), ..args(&["apple..zebra"]) }, &["arena8-apple"]),
        ];
        for (a, want) in cases {
            assert_eq!(ok(a), *want, "{a:?}");
        }
        let e = err(&SelectArgs { on: Some("aws".into()), ..Default::default() });
        assert_eq!(e, "--on `aws`: not a provider (one of runpod, vast, hetzner)");
    }

    #[test]
    fn typos_fail_loudly_with_the_closest_names() {
        // A target typo: named, with the nearest pod name.
        let e = err(&args(&["apple", "bloon"]));
        assert!(e.starts_with("a target matched no pod — nothing was done:"), "{e}");
        assert!(e.contains("`bloon`: no pod has that name or id — did you mean arena8-bloom?"), "{e}");
        // An exclude typo is just as fatal (it would act on the pod meant to be spared),
        // and every miss is listed together.
        let e = err(&SelectArgs { exclude: vec!["arena8-zebrra".into()], ..args(&["apple", "nope"]) });
        assert!(e.contains("some targets matched no pod"), "{e}");
        assert!(e.contains("`nope`: no pod has that name or id\n"), "{e}");
        assert!(e.contains("--exclude `arena8-zebrra`: no pod has that name or id — did you mean arena8-zebra?"), "{e}");
        // Several near names, closest first; nothing suggested for something far off.
        let e = err(&args(&["aple"]));
        assert!(e.contains("did you mean arena8-apple?"), "{e}");
        assert!(!err(&args(&["xyzzy"])).contains("did you mean"));
        // A list name with no pod right now is reported as such (not a typo).
        let e = err(&args(&["autumn"]));
        assert!(e.contains("`autumn`: no pod named arena8-autumn right now (it is a MACHINE_NAME_LIST name)"), "{e}");
        // A range with no pod in it.
        let e = err(&SelectArgs { exclude: vec!["autumn..autumn".into()], all: true, ..Default::default() });
        assert!(e.contains("--exclude `autumn..autumn`: no pod in that range (1 name: arena8-autumn … arena8-autumn)"), "{e}");
    }

    #[test]
    fn a_narrowed_selection_that_ends_up_empty_is_an_error() {
        let cases: &[(SelectArgs, &str)] = &[
            (SelectArgs { gpus: Some(4), ..Default::default() }, "nothing selected: all pods → 7, --gpus 4 → 0 (2 with an unknown GPU count)"),
            (SelectArgs { on: Some("vast".into()), ..args(&["apple..bloom"]) }, "nothing selected: apple..bloom → 2, --on vast → 0"),
            (SelectArgs { exclude: vec!["bloom".into()], ..args(&["bloom"]) }, "nothing selected: bloom → 1, --exclude bloom → 0"),
        ];
        for (a, want) in cases {
            let e = err(a);
            assert!(e.starts_with(want), "{a:?}: {e}");
        }
        // An empty fleet with nothing narrowed is just empty — not an error.
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        for a in [SelectArgs::default(), SelectArgs { all: true, ..Default::default() }] {
            assert_eq!(Selector::parse(&a, &naming).unwrap().resolve(&naming, &[]), Ok(vec![]));
        }
        // ...but naming a pod in an empty fleet is the typo error.
        let e = Selector::parse(&args(&["apple"]), &naming).unwrap().resolve(&naming, &[]).unwrap_err().0;
        assert!(e.contains("`apple`: no pod named arena8-apple right now"), "{e}");
    }

    #[test]
    fn contradictory_mixes_are_refused() {
        let e = err(&SelectArgs { all: true, ..args(&["apple"]) });
        assert_eq!(e, "pass target names or --all, not both (use --exclude to leave pods out of --all)");
        let e = err(&args(&["all", "apple"]));
        assert!(e.starts_with("`all` already selects every pod"), "{e}");
        let e = err(&SelectArgs { exclude: vec!["all".into()], ..Default::default() });
        assert_eq!(e, "--exclude all would leave nothing to act on");
    }

    #[test]
    fn two_pods_sharing_a_name_are_both_selected() {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let mut pods = fleet();
        pods.push(Pod { id: "id-second-apple".into(), ..pods[2].clone() });
        let sel = Selector::parse(&args(&["apple"]), &naming).unwrap();
        let got = sel.resolve(&naming, &pods).unwrap();
        let ids: Vec<&str> = got.iter().map(|&i| pods[i].id.as_str()).collect();
        assert_eq!(ids, ["id-arena8-apple", "id-second-apple"]);
    }

    #[test]
    fn scope_and_naming_queries() {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let parse = |a: &SelectArgs| Selector::parse(a, &naming).unwrap();
        assert!(parse(&SelectArgs::default()).is_unscoped());
        // filters alone don't scope a mutating command
        assert!(parse(&SelectArgs { gpus: Some(1), on: Some("runpod".into()), ..Default::default() }).is_unscoped());
        assert!(!parse(&SelectArgs { all: true, ..Default::default() }).is_unscoped());
        assert!(!parse(&args(&["all"])).is_unscoped());
        assert!(!parse(&args(&["apple"])).is_unscoped());
        assert!(args(&["apple"]).names_pods() && args(&["all", "apple..bloom"]).names_pods());
        assert!(!args(&["all"]).names_pods() && !SelectArgs { all: true, ..Default::default() }.names_pods());
        assert_eq!(
            parse(&SelectArgs { exclude: vec!["cloud".into()], gpus: Some(2), on: Some("runpod".into()), ..args(&["apple..mayor", "zebra"]) })
                .describe(),
            "apple..mayor zebra, excluding cloud, with 2 GPUs, on runpod"
        );
        assert_eq!(parse(&SelectArgs { all: true, gpus: Some(1), ..Default::default() }).describe(), "all pods, with 1 GPU");
        assert_eq!(SelectArgs::exact(["a", "b"]).targets, ["a", "b"]);
    }

    #[test]
    fn resolve_one_takes_exactly_one_pod() {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let mut pods = fleet();
        let one = |pods: &[Pod], t: &str| resolve_one(&naming, pods, t).map(|i| pods[i].name.clone()).map_err(|e| e.0);
        assert_eq!(one(&pods, "bloom"), Ok("arena8-bloom".into()));
        assert_eq!(one(&pods, "id-james-gpu"), Ok("james-gpu".into()));
        assert_eq!(one(&pods, "@james-gpu"), Ok("james-gpu".into()));
        let e = one(&pods, "bloon").unwrap_err();
        assert_eq!(e, "`bloon`: no pod has that name or id — did you mean arena8-bloom? (see `arena pods list`)");
        assert!(one(&pods, "all").unwrap_err().contains("takes one pod — `all` isn't accepted"));
        assert!(one(&pods, "apple..mayor").unwrap_err().contains("takes one pod — `apple..mayor` is a range"));
        assert!(one(&pods, "mayor..apple").unwrap_err().contains("is reversed"));
        // Two pods with one name: refuse, naming both ids.
        pods.push(Pod { id: "id-second-bloom".into(), ..pods[1].clone() });
        let e = one(&pods, "bloom").unwrap_err();
        assert_eq!(
            e,
            "`bloom` matches 2 pods: arena8-bloom (id id-arena8-bloom), arena8-bloom (id id-second-bloom) — pass the id of the one you mean"
        );
        assert_eq!(one(&pods, "id-second-bloom"), Ok("arena8-bloom".into()));
    }

    #[test]
    fn matcher_and_edit_distance() {
        let l = list();
        let naming = Naming { prefix: "arena8", list: &l };
        let p = pod("arena8-apple", "runpod", None);
        for t in ["arena8-apple", "apple", "id-arena8-apple"] {
            assert!(naming.matches(&p, t), "{t}");
        }
        for t in ["@apple", "appl", "arena9-apple", ""] {
            assert!(!naming.matches(&p, t), "{t}");
        }
        // A synthetic pod with no id is never matched by an empty id.
        assert!(!naming.matches(&Pod { name: "x".into(), ..Default::default() }, ""));
        assert_eq!(edit_distance("bloon", "bloom"), 1);
        assert_eq!(edit_distance("aple", "apple"), 1);
        assert_eq!(edit_distance("zebrra", "zebra"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }
}

//! Shared GPU catalog: one source of truth for the CLI's `--gpu` short-names and the
//! TUI add-pod picker, so both speak the same list. Preset prices are **approximate** and
//! tier-dependent (community vs secure); VRAM is exact. `arena gpus` prefers RunPod's live
//! prices ([`rows_from_live`]) and falls back to these presets.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::metrics::normalize_gpu_name;
use crate::provider::runpod::GpuType;
use crate::table::{self, Align};

/// A known GPU: the provider's type string, a short label, VRAM, and rough RunPod $/hr.
pub struct Gpu {
    pub api: &'static str,
    pub label: &'static str,
    pub vram_gb: u32,
    pub community: f64,
    pub secure: f64,
}

/// Most-wanted first (A4000, 3090, A40, A100), then the rest.
pub const PRESETS: &[Gpu] = &[
    Gpu { api: "NVIDIA RTX A4000", label: "RTX A4000", vram_gb: 16, community: 0.17, secure: 0.32 },
    Gpu { api: "NVIDIA GeForce RTX 3090", label: "RTX 3090", vram_gb: 24, community: 0.22, secure: 0.43 },
    Gpu { api: "NVIDIA A40", label: "A40", vram_gb: 48, community: 0.39, secure: 0.47 },
    Gpu { api: "NVIDIA A100 80GB PCIe", label: "A100 PCIe", vram_gb: 80, community: 1.19, secure: 1.64 },
    Gpu { api: "NVIDIA A100-SXM4-80GB", label: "A100 SXM", vram_gb: 80, community: 1.39, secure: 1.89 },
    Gpu { api: "NVIDIA RTX 4000 Ada Generation", label: "RTX 4000 Ada", vram_gb: 20, community: 0.20, secure: 0.32 },
    Gpu { api: "NVIDIA GeForce RTX 4090", label: "RTX 4090", vram_gb: 24, community: 0.34, secure: 0.69 },
    Gpu { api: "NVIDIA RTX A5000", label: "RTX A5000", vram_gb: 24, community: 0.22, secure: 0.36 },
    Gpu { api: "NVIDIA RTX A6000", label: "RTX A6000", vram_gb: 48, community: 0.49, secure: 0.79 },
    Gpu { api: "NVIDIA H100 80GB HBM3", label: "H100", vram_gb: 80, community: 1.99, secure: 2.79 },
    Gpu { api: "NVIDIA L40S", label: "L40S", vram_gb: 48, community: 0.79, secure: 1.03 },
    Gpu { api: "NVIDIA L40", label: "L40", vram_gb: 48, community: 0.69, secure: 0.99 },
    Gpu { api: "NVIDIA GeForce RTX 5090", label: "RTX 5090", vram_gb: 32, community: 0.89, secure: 1.29 },
    Gpu { api: "NVIDIA RTX 6000 Ada Generation", label: "RTX 6000 Ada", vram_gb: 48, community: 0.77, secure: 1.03 },
    // RunPod's display names: "RTX PRO 6000" is the Server Edition, "RTX PRO 6000 WK" the
    // Workstation Edition (finding #36 — the label here used to name the WK card).
    Gpu { api: "NVIDIA RTX PRO 6000 Blackwell Server Edition", label: "RTX PRO 6000", vram_gb: 96, community: 1.69, secure: 2.09 },
    Gpu { api: "NVIDIA RTX PRO 6000 Blackwell Workstation Edition", label: "RTX PRO 6000 WK", vram_gb: 96, community: 1.69, secure: 2.19 },
];

/// The known GPU matching a provider type string, if any.
pub fn find(api: &str) -> Option<&'static Gpu> {
    PRESETS.iter().find(|g| g.api == api)
}

/// All preset type strings, in preference order — for seeding the picker.
pub fn preset_apis() -> Vec<String> {
    PRESETS.iter().map(|g| g.api.to_string()).collect()
}

/// Map a friendly short-name (`A4000`, `3090`, `a100 sxm`, …) to the provider's full
/// type string. Unknown input passes through unchanged, so a full name still works — which
/// is also how a typo used to reach the provider as an invalid id; on RunPod the CLI now
/// checks the result against the live catalog ([`check_gpu_flag`]). The ids are RunPod's
/// (its GPU-types reference, checked 2026-10-07); the cheap community cards are here so the
/// common `--gpu 3070` works even when the catalog can't be fetched.
pub fn resolve(s: &str) -> String {
    let key = alnum_key(s);
    let api = match key.as_str() {
        "a4000" | "rtxa4000" => "NVIDIA RTX A4000",
        "a4000ada" | "rtx4000ada" | "4000ada" => "NVIDIA RTX 4000 Ada Generation",
        "4000adasff" | "4000sffada" | "rtx4000adasff" | "rtx4000sffada" | "4000sff" | "a4000adasff" => {
            "NVIDIA RTX 4000 SFF Ada Generation"
        }
        "2000ada" | "rtx2000ada" | "a2000ada" => "NVIDIA RTX 2000 Ada Generation",
        "5000ada" | "rtx5000ada" | "a5000ada" => "NVIDIA RTX 5000 Ada Generation",
        "a2000" | "rtxa2000" => "NVIDIA RTX A2000",
        "a4500" | "rtxa4500" => "NVIDIA RTX A4500",
        "3070" | "rtx3070" => "NVIDIA GeForce RTX 3070",
        "3080" | "rtx3080" => "NVIDIA GeForce RTX 3080",
        "3080ti" | "rtx3080ti" => "NVIDIA GeForce RTX 3080 Ti",
        "3090" | "rtx3090" => "NVIDIA GeForce RTX 3090",
        "3090ti" | "rtx3090ti" => "NVIDIA GeForce RTX 3090 Ti",
        "4070ti" | "rtx4070ti" => "NVIDIA GeForce RTX 4070 Ti",
        "4080" | "rtx4080" => "NVIDIA GeForce RTX 4080",
        "4080super" | "rtx4080super" | "4080s" => "NVIDIA GeForce RTX 4080 SUPER",
        "4090" | "rtx4090" => "NVIDIA GeForce RTX 4090",
        "5080" | "rtx5080" => "NVIDIA GeForce RTX 5080",
        "l4" => "NVIDIA L4",
        "a30" => "NVIDIA A30",
        "a40" => "NVIDIA A40",
        "a100" | "a100pcie" => "NVIDIA A100 80GB PCIe",
        "a100sxm" | "a100sxm4" => "NVIDIA A100-SXM4-80GB",
        "a5000" | "rtxa5000" => "NVIDIA RTX A5000",
        "a6000" | "rtxa6000" => "NVIDIA RTX A6000",
        "h100" | "h100sxm" => "NVIDIA H100 80GB HBM3",
        "h100pcie" => "NVIDIA H100 PCIe",
        "h100nvl" => "NVIDIA H100 NVL",
        "h200" | "h200sxm" => "NVIDIA H200",
        "b200" => "NVIDIA B200",
        "l40s" => "NVIDIA L40S",
        "l40" => "NVIDIA L40",
        "5090" | "rtx5090" => "NVIDIA GeForce RTX 5090",
        "rtx6000ada" | "6000ada" | "a6000ada" => "NVIDIA RTX 6000 Ada Generation",
        // As RunPod's catalog names them (`arena gpus`): "RTX PRO 6000" is the Server
        // Edition; the Workstation (WK) and Max-Q cards have names of their own.
        "rtxpro6000" | "pro6000" | "rtx6000pro" | "rtxpro6000server" | "pro6000server" => {
            "NVIDIA RTX PRO 6000 Blackwell Server Edition"
        }
        "rtxpro6000wk" | "pro6000wk" | "rtxpro6000workstation" | "pro6000workstation" => {
            "NVIDIA RTX PRO 6000 Blackwell Workstation Edition"
        }
        "rtxpro6000maxq" | "pro6000maxq" => "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition",
        _ => return s.to_string(),
    };
    api.to_string()
}

/// Lowercase alphanumerics only: `RTX 4000 Ada` → `rtx4000ada`.
fn alnum_key(s: &str) -> String {
    s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// [`alnum_key`] without the vendor/series words that don't tell two GPUs apart, so
/// `NVIDIA GeForce RTX 3070`, `RTX 3070`, `rtx3070` and `3070` all compare as `3070`.
fn core_key(s: &str) -> String {
    const NOISE: [&str; 6] = ["nvidia", "geforce", "rtx", "tesla", "generation", "amd"];
    let words: Vec<String> =
        s.split(|c: char| !c.is_ascii_alphanumeric()).map(str::to_ascii_lowercase).filter(|w| !w.is_empty()).collect();
    let mut key: String = words.iter().filter(|w| !NOISE.contains(&w.as_str())).map(String::as_str).collect();
    // A run-together token (`rtx3070`, `geforcertx3070`) carries the noise as a prefix.
    while let Some(rest) = NOISE.iter().find_map(|n| key.strip_prefix(n).filter(|r| !r.is_empty())) {
        key = rest.to_string();
    }
    key
}

/// Edit distance (insert/delete/substitute), for "did you mean" on typos like `a6oo0`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Catalog rows a `--gpu` token may mean: real GPUs only (RunPod's `unknown` placeholder
/// isn't one).
fn real_gpus(catalog: &[GpuType]) -> impl Iterator<Item = &GpuType> {
    catalog.iter().filter(|t| !t.id.trim().is_empty() && t.id != "unknown")
}

/// Up to four catalog GPUs a bad token probably meant, best first: ones whose short name
/// contains it or is contained in it (`3070x` → RTX 3070), then near-misses by edit
/// distance (`a6oo0` → RTX A6000).
fn suggest<'a>(token: &str, catalog: &'a [GpuType]) -> Vec<&'a GpuType> {
    let t = core_key(token);
    if t.is_empty() {
        return Vec::new();
    }
    let max_edits = (t.len() / 3).max(2);
    let mut scored: Vec<((u8, usize), &GpuType)> = real_gpus(catalog)
        .filter_map(|g| {
            [core_key(&g.id), core_key(&g.display_name)]
                .into_iter()
                .filter(|c| !c.is_empty())
                .filter_map(|c| {
                    if t.len() >= 2 && c.len() >= 2 && (c.contains(&t) || t.contains(&c)) {
                        Some((0u8, c.len().abs_diff(t.len())))
                    } else {
                        let d = edit_distance(&t, &c);
                        (d <= max_edits).then_some((1u8, d))
                    }
                })
                .min()
                .map(|score| (score, g))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, g)| g).take(4).collect()
}

/// `id (display name)` for a message.
fn describe_gpu(g: &GpuType) -> String {
    if g.display_name.is_empty() || g.display_name == g.id {
        format!("`{}`", g.id)
    } else {
        format!("`{}` ({})", g.id, g.display_name)
    }
}

/// Check a `--gpu` value (one token or a comma list) against RunPod's live catalog and
/// return it with every token as the catalog's exact id, comma-joined. Pure, so the whole
/// policy is table-tested; the CLI fetches the catalog and decides what to do when it
/// can't (warn and pass the flag through).
///
/// Per token: a catalog id (any case); else the one GPU whose catalog **display name** is the
/// token (`RTX PRO 6000`, `B300 MIG 34GB` — what `arena gpus` prints, so it means what the
/// operator just read: finding #36, where the hard-coded alias picked another card); else its
/// alias ([`resolve`]) if that's a catalog id; else the one GPU whose short name is exactly
/// the token (`RTX 3070`, `H100 SXM`).
/// Anything else is refused — before, an unknown token went to the provider as-is and
/// failed late and vaguely ("no known price", a 400 at create) — with "did you mean"
/// suggestions, and so is a token that names several GPUs. A value with no tokens at all
/// is returned unchanged for the flag parser to refuse.
pub fn check_gpu_flag(raw: &str, catalog: &[GpuType]) -> Result<String> {
    let tokens = crate::placement::split_list(raw);
    if tokens.is_empty() {
        return Ok(raw.to_string());
    }
    let mut ids = Vec::new();
    let mut problems = Vec::new();
    for token in &tokens {
        // The catalog's own names first: an exact id, then an exact display name — unique.
        let by_id = real_gpus(catalog).find(|g| g.id.eq_ignore_ascii_case(token.trim()));
        let shown = alnum_key(token);
        let mut by_display = real_gpus(catalog).filter(|g| !shown.is_empty() && alnum_key(&g.display_name) == shown);
        let by_display = match (by_display.next(), by_display.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        };
        if let Some(g) = by_id.or(by_display) {
            ids.push(g.id.clone());
            continue;
        }
        let resolved = resolve(token);
        let by_alias = real_gpus(catalog)
            .find(|g| g.id == resolved)
            .or_else(|| real_gpus(catalog).find(|g| g.id.eq_ignore_ascii_case(resolved.trim())));
        if let Some(g) = by_alias {
            ids.push(g.id.clone());
            continue;
        }
        let key = core_key(token);
        let mut exact: Vec<&GpuType> = real_gpus(catalog)
            .filter(|g| !key.is_empty() && (core_key(&g.display_name) == key || core_key(&g.id) == key))
            .collect();
        exact.dedup_by(|a, b| a.id == b.id);
        match exact.as_slice() {
            [one] => ids.push(one.id.clone()),
            [] => {
                let hints: Vec<String> = suggest(token, catalog).into_iter().map(describe_gpu).collect();
                let hint = if hints.is_empty() { String::new() } else { format!(" — did you mean {}?", hints.join(", ")) };
                problems.push(format!("`{token}` isn't a RunPod GPU type{hint}"));
            }
            several => {
                let names: Vec<String> = several.iter().map(|g| describe_gpu(g)).collect();
                problems.push(format!("`{token}` matches several RunPod GPU types: {} — pass the full id", names.join(", ")));
            }
        }
    }
    if !problems.is_empty() {
        return Err(Error::Config(format!(
            "--gpu: {} (`arena offers --all` lists the types and their --gpu names)",
            problems.join("; ")
        )));
    }
    Ok(ids.join(","))
}

/// A picker label including VRAM (`RTX A4000 · 16GB`); falls back to the normalized
/// name for an unknown type.
pub fn label(api: &str) -> String {
    match find(api) {
        Some(g) => format!("{} · {}GB", g.label, g.vram_gb),
        None => normalize_gpu_name(api),
    }
}

/// An approximate price string for a GPU in a provider/cloud context: RunPod shows the
/// community or secure number; Vast shows a "varies" band; others/unknown show nothing.
pub fn price_label(api: &str, provider: &str, cloud: Option<&str>) -> Option<String> {
    let g = find(api)?;
    match provider {
        "runpod" => {
            let secure = cloud.map(|c| c.eq_ignore_ascii_case("SECURE")).unwrap_or(false);
            Some(format!("~${:.2}/hr", if secure { g.secure } else { g.community }))
        }
        "vast" => Some(format!("~${:.2}–{:.2}/hr (varies)", g.community, g.secure)),
        _ => None,
    }
}

/// Where a `gpus` row (or its prices) came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// RunPod's live GraphQL catalog.
    Live,
    /// The local [`PRESETS`] table (no key / not RunPod / live fetch failed).
    Presets,
}

/// One row of `arena gpus` — the `--json` schema and what the table renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuRow {
    /// The exact provider type string to pass to `--gpu`.
    pub id: String,
    pub display_name: String,
    pub memory_gb: u32,
    /// $/h per GPU on the community / secure cloud; `None` = not offered / unknown.
    pub community_price: Option<f64>,
    pub secure_price: Option<f64>,
    /// RunPod's stock hint ("Low"/"Medium"/"High"/"None"; `None` = unreported): GraphQL's
    /// 1-GPU figure on v1, the COMMUNITY tier's on v2.
    pub stock_status: Option<String>,
    /// v2 only: the SECURE tier's stock hint ([`add_secure_stock`]) — v2's availability is
    /// per tier, and a secure-only card read "None" from the community figure alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure_stock_status: Option<String>,
    /// Whether RunPod's create API accepts this id (its OpenAPI enum can lag the catalog).
    /// `None` = couldn't determine (enum unavailable, or a preset row).
    pub creatable: Option<bool>,
    /// Where the row itself came from.
    pub source: Source,
    /// Where the prices came from: live rows use RunPod's prices when it sent any, else
    /// the preset estimate for that GPU; `None` = no price known at all. Kept separate from
    /// `source` so a script never mistakes a preset estimate for a live quote.
    pub price_source: Option<Source>,
}

/// Rows from RunPod's live catalog. Drops RunPod's `unknown`/0-GB placeholders and sorts
/// by VRAM then name. `creatable` is the create-API enum when it could be fetched; rows
/// are flagged against it rather than dropped, so `--json` shows the whole catalog and
/// the table decides what to hide.
pub fn rows_from_live(types: &[GpuType], creatable: Option<&[String]>) -> Vec<GpuRow> {
    let mut rows: Vec<GpuRow> = types
        .iter()
        .filter(|t| t.id != "unknown" && t.memory_gb > 0)
        .map(|t| {
            // Trust RunPod's prices as a pair when it sent any (a None tier then really
            // means "not offered"); only with no live price at all fall back to presets.
            let live = t.community_price.is_some() || t.secure_price.is_some();
            let (community_price, secure_price, price_source) = match find(&t.id) {
                _ if live => (t.community_price, t.secure_price, Some(Source::Live)),
                Some(g) => (Some(g.community), Some(g.secure), Some(Source::Presets)),
                None => (None, None, None),
            };
            GpuRow {
                id: t.id.clone(),
                display_name: t.display_name.clone(),
                memory_gb: t.memory_gb,
                community_price,
                secure_price,
                stock_status: t.stock_status.clone(),
                secure_stock_status: None,
                creatable: creatable.map(|ok| ok.iter().any(|c| c == &t.id)),
                source: Source::Live,
                price_source,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.memory_gb.cmp(&b.memory_gb).then_with(|| a.display_name.cmp(&b.display_name)));
    rows
}

/// v2: fill each row's SECURE stock from that tier's catalog (matched by id). Pure.
pub fn add_secure_stock(rows: &mut [GpuRow], secure: &[GpuType]) {
    for r in rows.iter_mut() {
        r.secure_stock_status = secure.iter().find(|t| t.id == r.id).and_then(|t| t.stock_status.clone());
    }
}

/// Rows from the local presets, in their preference order.
pub fn rows_from_presets() -> Vec<GpuRow> {
    PRESETS
        .iter()
        .map(|g| GpuRow {
            id: g.api.to_string(),
            display_name: g.label.to_string(),
            memory_gb: g.vram_gb,
            community_price: Some(g.community),
            secure_price: Some(g.secure),
            stock_status: None,
            secure_stock_status: None,
            creatable: None,
            source: Source::Presets,
            price_source: Some(Source::Presets),
        })
        .collect()
}

/// The `arena gpus` table. A preset (estimated) price is marked `~` so it can't be
/// mistaken for a live quote; `-` = not offered / unknown.
pub fn render_gpu_table(rows: &[GpuRow]) -> String {
    use Align::{Left, Right};
    let price = |p: Option<f64>, src: Option<Source>| match (p, src) {
        (Some(v), Some(Source::Presets)) => format!("~${v:.2}"),
        (Some(v), _) => format!("${v:.2}"),
        (None, _) => "-".to_string(),
    };
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.display_name.clone(),
                format!("{}G", r.memory_gb),
                price(r.community_price, r.price_source),
                price(r.secure_price, r.price_source),
                match (&r.stock_status, &r.secure_stock_status) {
                    (c, Some(sec)) => format!("{}/{sec}", c.as_deref().unwrap_or("-")),
                    (Some(c), None) => c.clone(),
                    (None, None) => "-".to_string(),
                },
                r.id.clone(),
            ]
        })
        .collect();
    table::render(
        &["GPU", "VRAM", "$/HR COMM", "$/HR SEC", "STOCK", "API NAME (--gpu)"],
        &[Left, Right, Right, Right, Left, Left],
        &cells,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: &str, name: &str, mem: u32, comm: Option<f64>, sec: Option<f64>, stock: Option<&str>) -> GpuType {
        GpuType {
            id: id.into(),
            display_name: name.into(),
            memory_gb: mem,
            community_price: comm,
            secure_price: sec,
            stock_status: stock.map(String::from),
        }
    }

    #[test]
    fn live_rows_prefer_live_prices_and_flag_creatable() {
        let types = vec![
            live("NVIDIA A40", "A40", 48, None, None, None), // no live price: preset estimate
            live("NVIDIA RTX A4000", "RTX A4000", 16, Some(0.17), Some(0.25), Some("Low")),
            live("NVIDIA H100 80GB HBM3", "H100", 80, None, Some(2.69), Some("High")), // secure-only
            live("NVIDIA Mystery", "Mystery", 12, None, None, None), // unpriced, not a preset
            live("unknown", "unknown", 0, None, None, None),          // placeholder: dropped
        ];
        let ok = vec!["NVIDIA RTX A4000".to_string(), "NVIDIA A40".to_string()];
        let rows = rows_from_live(&types, Some(&ok));
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["NVIDIA Mystery", "NVIDIA RTX A4000", "NVIDIA A40", "NVIDIA H100 80GB HBM3"]); // by VRAM
        let a4000 = &rows[1];
        assert_eq!((a4000.community_price, a4000.secure_price), (Some(0.17), Some(0.25)));
        assert_eq!((a4000.price_source, a4000.source, a4000.creatable), (Some(Source::Live), Source::Live, Some(true)));
        assert_eq!(a4000.stock_status.as_deref(), Some("Low"));
        let a40 = &rows[2];
        assert_eq!((a40.community_price, a40.secure_price, a40.price_source), (Some(0.39), Some(0.47), Some(Source::Presets)));
        // A live secure-only GPU is NOT back-filled with the preset community price.
        let h100 = &rows[3];
        assert_eq!((h100.community_price, h100.secure_price, h100.creatable), (None, Some(2.69), Some(false)));
        assert_eq!((rows[0].community_price, rows[0].price_source), (None, None));
        // Enum unavailable => creatable unknown, not false.
        assert!(rows_from_live(&types, None).iter().all(|r| r.creatable.is_none()));
    }

    /// Finding #31: on v2 the STOCK column was the community figure alone, so a secure-only
    /// card read "None"; each tier's stock is shown now (and `--json` adds the secure one).
    #[test]
    fn v2_rows_carry_each_tiers_stock() {
        let comm = vec![
            live("NVIDIA B300 SXM6 AC MIG 1g.34gb", "B300 MIG 34GB", 34, None, Some(0.5), Some("None")),
            live("NVIDIA RTX A4000", "RTX A4000", 16, Some(0.17), Some(0.25), Some("Low")),
        ];
        let secure = vec![
            live("NVIDIA B300 SXM6 AC MIG 1g.34gb", "B300 MIG 34GB", 34, None, Some(0.5), Some("Low")),
            live("NVIDIA RTX A4000", "RTX A4000", 16, Some(0.17), Some(0.25), Some("None")),
        ];
        let ids: Vec<String> = comm.iter().map(|t| t.id.clone()).collect();
        let mut rows = rows_from_live(&comm, Some(&ids));
        add_secure_stock(&mut rows, &secure);
        assert!(rows.iter().all(|r| r.creatable == Some(true)));
        let out = render_gpu_table(&rows);
        let line = |name: &str| out.lines().find(|l| l.starts_with(name)).unwrap().to_string();
        assert!(line("RTX A4000").contains(" Low/None "), "{out}");
        assert!(line("B300 MIG 34GB").contains(" None/Low "), "{out}");
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(v[0]["secure_stock_status"], "None");
        // v1 rows (no secure figure) keep the old JSON shape.
        let v1 = serde_json::to_value(rows_from_live(&comm, None)).unwrap();
        assert!(v1[0].get("secure_stock_status").is_none(), "{v1}");
    }

    #[test]
    fn gpu_rows_json_schema_and_round_trip() {
        let rows = rows_from_live(&[live("NVIDIA RTX A4000", "RTX A4000", 16, Some(0.17), Some(0.25), Some("Low"))], None);
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(
            v,
            serde_json::json!([{
                "id": "NVIDIA RTX A4000", "display_name": "RTX A4000", "memory_gb": 16,
                "community_price": 0.17, "secure_price": 0.25, "stock_status": "Low",
                "creatable": null, "source": "live", "price_source": "live"
            }])
        );
        let back: Vec<GpuRow> = serde_json::from_value(v).unwrap();
        assert_eq!(back, rows);
        let presets = rows_from_presets();
        assert_eq!(presets.len(), PRESETS.len());
        assert_eq!(serde_json::to_value(&presets[0]).unwrap()["source"], "presets");
    }

    #[test]
    fn gpu_table_snapshot() {
        let types = vec![
            live("NVIDIA RTX A4000", "RTX A4000", 16, Some(0.17), Some(0.25), Some("Low")),
            live("NVIDIA A40", "A40", 48, None, None, None),
            live("NVIDIA H100 80GB HBM3", "H100", 80, None, Some(2.69), Some("High")),
        ];
        let out = render_gpu_table(&rows_from_live(&types, None));
        let want = "\
GPU        VRAM  $/HR COMM  $/HR SEC  STOCK  API NAME (--gpu)
RTX A4000   16G      $0.17     $0.25  Low    NVIDIA RTX A4000
A40         48G     ~$0.39    ~$0.47  -      NVIDIA A40
H100        80G          -     $2.69  High   NVIDIA H100 80GB HBM3
";
        assert_eq!(out, want, "\n--- got ---\n{out}");
    }

    /// The live catalog's shape (ids/display names as RunPod's GPU-types reference has them).
    fn catalog() -> Vec<GpuType> {
        [
            ("NVIDIA GeForce RTX 3070", "RTX 3070"),
            ("NVIDIA GeForce RTX 3080", "RTX 3080"),
            ("NVIDIA GeForce RTX 3080 Ti", "RTX 3080 Ti"),
            ("NVIDIA GeForce RTX 3090", "RTX 3090"),
            ("NVIDIA GeForce RTX 4090", "RTX 4090"),
            ("NVIDIA RTX A4000", "RTX A4000"),
            ("NVIDIA RTX A6000", "RTX A6000"),
            ("NVIDIA RTX 4000 Ada Generation", "RTX 4000 Ada"),
            ("NVIDIA RTX 4000 SFF Ada Generation", "RTX 4000 Ada SFF"),
            ("NVIDIA A40", "A40"),
            ("NVIDIA L4", "L4"),
            ("NVIDIA H100 80GB HBM3", "H100 SXM"),
            ("NVIDIA H100 PCIe", "H100 PCIe"),
            ("Tesla V100-PCIE-16GB", "Tesla V100"),
            ("unknown", "unknown"),
        ]
        .iter()
        .map(|(id, name)| live(id, name, 16, None, None, None))
        .collect()
    }

    #[test]
    fn check_gpu_flag_accepts_aliases_ids_and_unique_short_names() {
        let c = catalog();
        // (flag, canonical ids)
        for (flag, want) in [
            ("3070", "NVIDIA GeForce RTX 3070"),                     // alias
            ("A4000", "NVIDIA RTX A4000"),
            ("NVIDIA RTX A4000", "NVIDIA RTX A4000"),                // exact id
            ("nvidia geforce rtx 3090", "NVIDIA GeForce RTX 3090"),  // id, any case
            ("RTX 3080 Ti", "NVIDIA GeForce RTX 3080 Ti"),           // alias
            ("H100 SXM", "NVIDIA H100 80GB HBM3"),
            ("v100", "Tesla V100-PCIE-16GB"),                        // no alias: the unique short name
            ("Tesla V100", "Tesla V100-PCIE-16GB"),                  // …or display name
            ("l4", "NVIDIA L4"),
            ("4000 ada sff", "NVIDIA RTX 4000 SFF Ada Generation"),
            ("A4000, 3070,", "NVIDIA RTX A4000,NVIDIA GeForce RTX 3070"), // lists keep order
            (",", ","),                                              // no tokens: the parser refuses it
        ] {
            assert_eq!(check_gpu_flag(flag, &c).unwrap(), want, "{flag}");
        }
    }

    #[test]
    fn check_gpu_flag_refuses_unknown_tokens_with_suggestions() {
        let c = catalog();
        let err = |flag: &str| check_gpu_flag(flag, &c).unwrap_err().to_string();
        // (flag, must contain)
        for (flag, want) in [
            ("3070x", "did you mean `NVIDIA GeForce RTX 3070` (RTX 3070)"),
            ("RTX 3070 Ti", "`NVIDIA GeForce RTX 3070` (RTX 3070)"),
            ("a6oo0", "`NVIDIA RTX A6000` (RTX A6000)"),   // typo: edit distance
            ("A400", "`NVIDIA RTX A4000` (RTX A4000)"),
            ("NVIDIA RTX A4000x", "`NVIDIA RTX A4000`"),
            ("unknown", "isn't a RunPod GPU type"),         // the placeholder isn't a GPU
            ("zzzzzzzz", "`zzzzzzzz` isn't a RunPod GPU type (`arena offers --all`"), // nothing close: no hint
            ("4000 ada", ""),                               // alias → exact id: fine (checked below)
        ] {
            if want.is_empty() {
                assert!(check_gpu_flag(flag, &c).is_ok(), "{flag}");
                continue;
            }
            let e = err(flag);
            assert!(e.contains(want), "{flag}: {e}");
            assert!(e.contains("arena offers --all"), "{flag}: {e}");
        }
        // Every bad token in a list is named, in one error; the good ones aren't.
        let e = err("A4000,3070x,foo99");
        assert!(e.contains("`3070x`") && e.contains("`foo99`") && !e.contains("`A4000`"), "{e}");
        assert!(matches!(check_gpu_flag("3070x", &c), Err(Error::Config(_))));
    }

    /// Finding #36: `--gpu 'RTX PRO 6000'` — the name `arena gpus` prints for the Server
    /// Edition — created a Workstation Edition, because the hard-coded alias won. The live
    /// catalog's display names come first now (recorded rows from `arena gpus --json`,
    /// 2026-10-08); the alias table agrees with them for when there's no catalog.
    #[test]
    fn check_gpu_flag_follows_the_catalogs_display_names_before_aliases() {
        let mut c = catalog();
        for (id, name) in [
            ("NVIDIA RTX PRO 6000 Blackwell Server Edition", "RTX PRO 6000"),
            ("NVIDIA RTX PRO 6000 Blackwell Workstation Edition", "RTX PRO 6000 WK"),
            ("NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition", "RTX PRO 6000 MaxQ"),
            ("NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb", "PRO 6000 MIG 24GB"),
            ("NVIDIA B300 SXM6 AC MIG 1g.34gb", "B300 MIG 34GB"),
        ] {
            c.push(live(id, name, 96, None, None, None));
        }
        for (flag, want) in [
            ("RTX PRO 6000", "NVIDIA RTX PRO 6000 Blackwell Server Edition"),
            ("rtx pro 6000", "NVIDIA RTX PRO 6000 Blackwell Server Edition"),
            ("RTX PRO 6000 WK", "NVIDIA RTX PRO 6000 Blackwell Workstation Edition"),
            ("RTX PRO 6000 MaxQ", "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition"),
            ("PRO 6000 MIG 24GB", "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb"),
            ("b300 mig 34gb", "NVIDIA B300 SXM6 AC MIG 1g.34gb"),
            ("A4000", "NVIDIA RTX A4000"), // aliases still work where no display name matches
        ] {
            assert_eq!(check_gpu_flag(flag, &c).unwrap(), want, "{flag}");
        }
        // Where a display name and an alias disagree, the catalog's name wins: here the card
        // shown as `H100` is the NVL, while the alias `h100` means the SXM (also listed).
        let mut c2 = catalog();
        c2.push(live("NVIDIA H100 NVL", "H100", 94, None, None, None));
        assert_eq!(resolve("H100"), "NVIDIA H100 80GB HBM3");
        assert_eq!(check_gpu_flag("H100", &c2).unwrap(), "NVIDIA H100 NVL");
        // No catalog: the alias table now means what the catalog does.
        assert_eq!(resolve("RTX PRO 6000"), "NVIDIA RTX PRO 6000 Blackwell Server Edition");
        assert_eq!(find("NVIDIA RTX PRO 6000 Blackwell Server Edition").map(|g| g.label), Some("RTX PRO 6000"));
        assert_eq!(find("NVIDIA RTX PRO 6000 Blackwell Workstation Edition").map(|g| g.label), Some("RTX PRO 6000 WK"));
    }

    #[test]
    fn check_gpu_flag_refuses_a_short_name_that_means_several_gpus() {
        let mut c = catalog();
        c.push(live("Tesla V100-SXM2-16GB", "Tesla V100", 16, None, None, None));
        let e = check_gpu_flag("v100", &c).unwrap_err().to_string();
        assert!(e.contains("`v100` matches several RunPod GPU types"), "{e}");
        assert!(e.contains("`Tesla V100-PCIE-16GB`") && e.contains("`Tesla V100-SXM2-16GB`"), "{e}");
        // The exact id still picks one.
        assert_eq!(check_gpu_flag("Tesla V100-SXM2-16GB", &c).unwrap(), "Tesla V100-SXM2-16GB");
    }

    #[test]
    fn core_key_and_edit_distance() {
        for (s, want) in [
            ("NVIDIA GeForce RTX 3070", "3070"),
            ("RTX 3070", "3070"),
            ("rtx3070", "3070"),
            ("geforcertx3070", "3070"),
            ("RTX A4000", "a4000"),
            ("NVIDIA RTX 4000 Ada Generation", "4000ada"),
            ("Tesla V100-PCIE-16GB", "v100pcie16gb"),
            ("rtx", ""), // only noise: nothing to match on (refused, no suggestions)
        ] {
            assert_eq!(core_key(s), want, "{s}");
        }
        assert_eq!(edit_distance("a6oo0", "a6000"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("3070", "3070"), 0);
    }

    #[test]
    fn resolves_short_names_and_passes_through() {
        assert_eq!(resolve("A4000"), "NVIDIA RTX A4000");
        assert_eq!(resolve("a100 sxm"), "NVIDIA A100-SXM4-80GB");
        assert_eq!(resolve("3090"), "NVIDIA GeForce RTX 3090");
        assert_eq!(resolve("5090"), "NVIDIA GeForce RTX 5090");
        assert_eq!(resolve("A6000 Ada"), "NVIDIA RTX 6000 Ada Generation");
        assert_eq!(resolve("pro 6000"), "NVIDIA RTX PRO 6000 Blackwell Server Edition");
        assert_eq!(resolve("RTX PRO 6000 WK"), "NVIDIA RTX PRO 6000 Blackwell Workstation Edition");
        assert_eq!(resolve("pro6000 maxq"), "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition");
        assert_eq!(resolve("L40"), "NVIDIA L40");
        // The cheap/common community cards (live finding: `--gpu 3070` was passed through
        // as the invalid id "3070").
        for (alias, id) in [
            ("3070", "NVIDIA GeForce RTX 3070"),
            ("RTX 3080", "NVIDIA GeForce RTX 3080"),
            ("3080 Ti", "NVIDIA GeForce RTX 3080 Ti"),
            ("3090ti", "NVIDIA GeForce RTX 3090 Ti"),
            ("4070 Ti", "NVIDIA GeForce RTX 4070 Ti"),
            ("4080", "NVIDIA GeForce RTX 4080"),
            ("4080 SUPER", "NVIDIA GeForce RTX 4080 SUPER"),
            ("5080", "NVIDIA GeForce RTX 5080"),
            ("L4", "NVIDIA L4"),
            ("2000 Ada", "NVIDIA RTX 2000 Ada Generation"),
            ("4000 Ada SFF", "NVIDIA RTX 4000 SFF Ada Generation"),
            ("A2000", "NVIDIA RTX A2000"),
            ("A4500", "NVIDIA RTX A4500"),
            ("RTX A4000", "NVIDIA RTX A4000"),
            ("H100 PCIe", "NVIDIA H100 PCIe"),
        ] {
            assert_eq!(resolve(alias), id, "{alias}");
            assert_eq!(resolve(id), id, "resolving is idempotent: {id}");
        }
        // a full/unknown string is left as-is
        assert_eq!(resolve("NVIDIA Something Custom"), "NVIDIA Something Custom");
    }

    #[test]
    fn labels_and_prices() {
        assert_eq!(label("NVIDIA RTX A4000"), "RTX A4000 · 16GB");
        assert_eq!(label("NVIDIA Whatever"), "Whatever"); // normalized fallback
        assert_eq!(price_label("NVIDIA RTX A4000", "runpod", Some("SECURE")).as_deref(), Some("~$0.32/hr"));
        assert_eq!(price_label("NVIDIA RTX A4000", "runpod", None).as_deref(), Some("~$0.17/hr"));
        assert!(price_label("NVIDIA RTX A4000", "vast", None).unwrap().contains("varies"));
        assert_eq!(price_label("NVIDIA RTX A4000", "hetzner", None), None);
    }
}

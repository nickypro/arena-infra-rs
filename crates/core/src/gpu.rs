//! Shared GPU catalog: one source of truth for the CLI's `--gpu` short-names and the
//! TUI add-pod picker, so both speak the same list. Prices are **approximate** and
//! tier-dependent (community vs secure); VRAM is exact. Real price/availability data
//! can replace the static figures later.

use crate::metrics::normalize_gpu_name;

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
    Gpu { api: "NVIDIA RTX PRO 6000 Blackwell Workstation Edition", label: "RTX PRO 6000", vram_gb: 96, community: 1.79, secure: 2.49 },
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
/// type string. Unknown input passes through unchanged, so a full name still works.
pub fn resolve(s: &str) -> String {
    let key: String = s.to_ascii_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    let api = match key.as_str() {
        "a4000" => "NVIDIA RTX A4000",
        "a4000ada" | "rtx4000ada" | "4000ada" => "NVIDIA RTX 4000 Ada Generation",
        "3090" | "rtx3090" => "NVIDIA GeForce RTX 3090",
        "4090" | "rtx4090" => "NVIDIA GeForce RTX 4090",
        "a40" => "NVIDIA A40",
        "a100" | "a100pcie" => "NVIDIA A100 80GB PCIe",
        "a100sxm" | "a100sxm4" => "NVIDIA A100-SXM4-80GB",
        "a5000" => "NVIDIA RTX A5000",
        "a6000" => "NVIDIA RTX A6000",
        "h100" => "NVIDIA H100 80GB HBM3",
        "l40s" => "NVIDIA L40S",
        "l40" => "NVIDIA L40",
        "5090" | "rtx5090" => "NVIDIA GeForce RTX 5090",
        "rtx6000ada" | "6000ada" | "a6000ada" => "NVIDIA RTX 6000 Ada Generation",
        "rtxpro6000" | "pro6000" | "rtx6000pro" => "NVIDIA RTX PRO 6000 Blackwell Workstation Edition",
        _ => return s.to_string(),
    };
    api.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_short_names_and_passes_through() {
        assert_eq!(resolve("A4000"), "NVIDIA RTX A4000");
        assert_eq!(resolve("a100 sxm"), "NVIDIA A100-SXM4-80GB");
        assert_eq!(resolve("3090"), "NVIDIA GeForce RTX 3090");
        assert_eq!(resolve("5090"), "NVIDIA GeForce RTX 5090");
        assert_eq!(resolve("A6000 Ada"), "NVIDIA RTX 6000 Ada Generation");
        assert_eq!(resolve("pro 6000"), "NVIDIA RTX PRO 6000 Blackwell Workstation Edition");
        assert_eq!(resolve("L40"), "NVIDIA L40");
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

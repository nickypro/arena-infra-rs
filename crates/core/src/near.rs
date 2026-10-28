//! Placement near the proxy (`NEAR`, `NEAR_MAX_EXTRA_PCT`): prefer pods in the region where
//! arena runs, unless that costs more than a set percentage extra.
//!
//! Every participant's SSH and VS Code traffic goes through the control box's SSH proxy (on
//! prod: Hetzner nbg1, Nuremberg). A pod in the US adds ~100 ms to every keystroke for a
//! European cohort, to save cents an hour — so by default placement tries Europe first and
//! takes a European option unless it costs more than 25% over the cheapest acceptable one.
//!
//! What "near" means per provider, from the live APIs (2026-10-28):
//! - **RunPod:** a create takes `dataCenterIds`. Data centres are named by region prefix —
//!   every `EUROPE` data centre in `GET /v2/catalog/datacenters` is `EU-…` or `EUR-…`. The
//!   price is per GPU type and tier, not per data centre, so a near option costs exactly
//!   what the same option anywhere costs. **SECURE only:** COMMUNITY machines have no data
//!   centre (a running community pod's `dataCenterId` is null, the community catalog lists
//!   no data centres for any GPU, and a community create pinned to *every* data centre found
//!   no capacity seconds before an unpinned one succeeded) — pinning one only wastes a create.
//! - **Vast:** every offer carries `geolocation` (`"Quebec, CA"`), and the search filters on
//!   it by country code (`"geolocation": {"in": [...]}`). Prices are per offer, so a near
//!   offer is weighed against the cheapest acceptable one.
//! - **Hetzner** already creates in the nearest location (measured, `HETZNER_LOCATION`), and
//!   **Shadeform** has its own region list: neither reads this.
//!
//! The tables below are the one place a region is defined; they're pure and tested.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};

/// A place "near" can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Region {
    Europe,
    NorthAmerica,
}

/// RunPod's European data centres, as `GET /v2/catalog/datacenters` listed them on
/// 2026-10-28 (all with `region: EUROPE`). What a near create pins (`dataCenterIds`): a data
/// centre RunPod adds later is reached by the "anywhere" option until it's added here.
const RUNPOD_EUROPE: &[&str] = &[
    "EU-CZ-1", "EU-FR-1", "EU-NL-1", "EU-RO-1", "EU-SE-1", "EUR-IS-1", "EUR-IS-2", "EUR-IS-3", "EUR-IS-4", "EUR-IS-5",
    "EUR-NO-1", "EUR-NO-2",
];

/// RunPod's North American data centres (`region: NORTH_AMERICA`), same listing.
const RUNPOD_NORTH_AMERICA: &[&str] = &[
    "CA-MTL-1", "CA-MTL-3", "CA-MTL-4", "US-CA-2", "US-CO-1", "US-GA-2", "US-IL-1", "US-KS-2", "US-MD-1", "US-MO-1",
    "US-MO-2", "US-NC-1", "US-NC-2", "US-NE-1", "US-PA-1", "US-TX-3", "US-TX-4", "US-WA-1", "US-WA-2",
];

/// Europe's ISO 3166-1 country codes, as Vast's `geolocation` ends (`"Czechia, CZ"`): the
/// EU, the EEA, Switzerland, the UK, the Balkans, Ukraine, Moldova and the microstates. Left
/// out: Russia and Belarus (sanctions), and the transcontinental Turkey and Caucasus.
const EUROPE_COUNTRIES: &[&str] = &[
    "AD", "AL", "AT", "BA", "BE", "BG", "CH", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FO", "FR", "GB", "GG", "GI", "GR",
    "HR", "HU", "IE", "IM", "IS", "IT", "JE", "LI", "LT", "LU", "LV", "MC", "MD", "ME", "MK", "MT", "NL", "NO", "PL", "PT",
    "RO", "RS", "SE", "SI", "SK", "SM", "UA", "VA", "XK",
];

const NORTH_AMERICA_COUNTRIES: &[&str] = &["CA", "MX", "US"];

impl Region {
    /// `NEAR`'s value for it, as notes name the setting.
    pub fn key(self) -> &'static str {
        match self {
            Region::Europe => "europe",
            Region::NorthAmerica => "north-america",
        }
    }

    /// How tables and notes name it.
    pub fn label(self) -> &'static str {
        match self {
            Region::Europe => "Europe",
            Region::NorthAmerica => "North America",
        }
    }

    /// RunPod's data-centre id prefixes for the region: how its ids are formed (`EU-RO-1`,
    /// `EUR-IS-2`, `US-KS-2`, `CA-MTL-1`).
    fn runpod_prefixes(self) -> &'static [&'static str] {
        match self {
            Region::Europe => &["EU-", "EUR-"],
            Region::NorthAmerica => &["US-", "CA-"],
        }
    }

    /// Whether RunPod data centre `id` is in the region, by its prefix — so one the table
    /// doesn't list yet (a new `EU-DE-1`) still counts when a catalog names it. Pure.
    pub fn has_runpod_dc(self, id: &str) -> bool {
        let id = id.trim().to_ascii_uppercase();
        self.runpod_prefixes().iter().any(|p| id.starts_with(p))
    }

    /// The RunPod data centres a near create pins.
    pub fn runpod_dcs(self) -> &'static [&'static str] {
        match self {
            Region::Europe => RUNPOD_EUROPE,
            Region::NorthAmerica => RUNPOD_NORTH_AMERICA,
        }
    }

    /// The country codes a near Vast offer may be in.
    pub fn countries(self) -> &'static [&'static str] {
        match self {
            Region::Europe => EUROPE_COUNTRIES,
            Region::NorthAmerica => NORTH_AMERICA_COUNTRIES,
        }
    }

    /// Whether country code `cc` (any case) is in the region. Pure.
    pub fn has_country(self, cc: &str) -> bool {
        let cc = cc.trim().to_ascii_uppercase();
        self.countries().contains(&cc.as_str())
    }

    /// `NEAR`'s value as a region (`None`: not a region name). Pure.
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "europe" | "eu" => Some(Region::Europe),
            "north-america" | "na" => Some(Region::NorthAmerica),
            _ => None,
        }
    }
}

/// The country code at the end of a Vast `geolocation` (`"Quebec, CA"` → `CA`, `"Czechia,
/// CZ"` → `CZ`); `None` when it doesn't end in two letters. Pure.
pub fn vast_country(geolocation: &str) -> Option<String> {
    let last = geolocation.rsplit(',').next()?.trim();
    (last.len() == 2 && last.chars().all(|c| c.is_ascii_alphabetic())).then(|| last.to_ascii_uppercase())
}

/// The config keys.
pub const NEAR_KEY: &str = "NEAR";
pub const EXTRA_KEY: &str = "NEAR_MAX_EXTRA_PCT";

/// `NEAR` unset: Europe — where prod's proxy is, and prod's config predates the key.
pub const DEFAULT_REGION: Region = Region::Europe;

/// `NEAR_MAX_EXTRA_PCT` unset: a near option may cost up to 25% more than the cheapest. The
/// human's call: latency over small savings (an A4000 is $0.17/h; 25% is 4 cents).
pub const DEFAULT_MAX_EXTRA_PCT: f64 = 25.0;

/// Where "near" is and how much more a near option may cost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Near {
    pub region: Region,
    /// A near option is preferred while it costs at most this many percent more than the
    /// cheapest acceptable option.
    pub max_extra_pct: f64,
}

impl Near {
    /// From config: `NEAR` (a region; unset = [`DEFAULT_REGION`]; `any`, `off`, `none` or
    /// empty = off) and `NEAR_MAX_EXTRA_PCT` (`25`, `25%`; unset or empty =
    /// [`DEFAULT_MAX_EXTRA_PCT`]). `Ok(None)`: off. A value it can't read is a config error,
    /// never a guess.
    pub fn from_config(cfg: &Config) -> Result<Option<Near>> {
        Self::parse(cfg.get(NEAR_KEY), cfg.get(EXTRA_KEY))
    }

    /// [`Self::from_config`] on the two raw values (`None` = unset). Pure.
    pub fn parse(near: Option<&str>, extra: Option<&str>) -> Result<Option<Near>> {
        let region = match near.map(str::trim) {
            None => DEFAULT_REGION,
            Some(v) if v.is_empty() || ["any", "off", "none", "anywhere"].contains(&v.to_ascii_lowercase().as_str()) => {
                return Ok(None)
            }
            Some(v) => Region::parse(v).ok_or_else(|| {
                Error::Config(format!("{NEAR_KEY} `{}`: expected europe, north-america, or any (off)", short(v)))
            })?,
        };
        let max_extra_pct = match extra.map(str::trim).filter(|v| !v.is_empty()) {
            None => DEFAULT_MAX_EXTRA_PCT,
            Some(v) => v
                .trim_end_matches('%')
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|p| p.is_finite() && *p >= 0.0)
                .ok_or_else(|| Error::Config(format!("{EXTRA_KEY} `{}`: expected a percentage ≥ 0, like 25", short(v))))?,
        };
        Ok(Some(Near { region, max_extra_pct }))
    }

    /// The most a near option may cost per pod, given the cheapest acceptable option's price.
    pub fn budget(&self, cheapest: f64) -> f64 {
        cheapest * (1.0 + self.max_extra_pct / 100.0)
    }

    /// `Europe, up to +25%` — how notes say it.
    pub fn describe(&self) -> String {
        format!("{}, up to +{}", self.region.label(), self.margin())
    }

    /// `25%`, `12.5%`.
    pub fn margin(&self) -> String {
        format!("{}%", fmt_pct(self.max_extra_pct))
    }
}

/// `25`, `12.5` — a percentage without float noise.
pub(crate) fn fmt_pct(p: f64) -> String {
    let s = format!("{p:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A config value as an error quotes it: whole when short, else its length.
fn short(v: &str) -> String {
    if v.chars().count() <= 24 {
        v.to_string()
    } else {
        format!("({} chars)", v.chars().count())
    }
}

/// Whether near placement can do anything for a create on `provider` with these tiers: Vast
/// always; RunPod only with SECURE among them (COMMUNITY machines have no data centre, see the
/// module doc); nobody else. Pure.
pub fn reaches(provider: &str, clouds: &[String]) -> bool {
    match provider {
        "vast" => true,
        "runpod" => clouds.iter().any(|c| runpod_tier_placeable(c)),
        _ => false,
    }
}

/// Whether a RunPod tier can be pinned to data centres: SECURE, never COMMUNITY. Pure.
pub fn runpod_tier_placeable(cloud: &str) -> bool {
    cloud.trim().eq_ignore_ascii_case("SECURE")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_parse_with_defaults_and_off() {
        let eu = |pct| Some(Near { region: Region::Europe, max_extra_pct: pct });
        // (NEAR, NEAR_MAX_EXTRA_PCT, want)
        let cases: &[(Option<&str>, Option<&str>, Option<Near>)] = &[
            (None, None, eu(25.0)), // prod's config has neither key
            (Some("europe"), None, eu(25.0)),
            (Some(" EU "), Some("10"), eu(10.0)),
            (Some("Europe"), Some("12.5%"), eu(12.5)),
            (Some("europe"), Some(""), eu(25.0)),
            (Some("europe"), Some("0"), eu(0.0)),
            (Some("north-america"), Some("50"), Some(Near { region: Region::NorthAmerica, max_extra_pct: 50.0 })),
            (Some("north_america"), None, Some(Near { region: Region::NorthAmerica, max_extra_pct: 25.0 })),
            (Some("NA"), None, Some(Near { region: Region::NorthAmerica, max_extra_pct: 25.0 })),
            (Some(""), None, None),
            (Some("any"), None, None),
            (Some("ANY"), Some("junk"), None), // off: the percentage isn't read
            (Some("off"), None, None),
            (Some("none"), None, None),
            (Some("anywhere"), None, None),
        ];
        for (near, extra, want) in cases {
            assert_eq!(Near::parse(*near, *extra).unwrap(), *want, "{near:?} {extra:?}");
        }
        for (near, extra, says) in [
            (Some("asia"), None, "NEAR `asia`: expected europe, north-america, or any"),
            (None, Some("-5"), "NEAR_MAX_EXTRA_PCT `-5`"),
            (None, Some("lots"), "NEAR_MAX_EXTRA_PCT `lots`"),
            (None, Some("NaN"), "NEAR_MAX_EXTRA_PCT `NaN`"),
            (Some(&*"x".repeat(40)), None, "NEAR `(40 chars)`"),
        ] {
            let e = Near::parse(near, extra).unwrap_err().to_string();
            assert!(e.contains(says), "{near:?} {extra:?}: {e}");
        }
        let cfg = Config::parse("NEAR=any\n");
        assert_eq!(Near::from_config(&cfg).unwrap(), None);
        assert_eq!(Near::from_config(&Config::parse("")).unwrap(), eu(25.0));
        // Prod's file has neither key: the environment may still turn it off, or change the
        // margin, for one run — and overrides a file that sets them.
        let env = |k: &str| match k {
            NEAR_KEY => Some("any".to_string()),
            EXTRA_KEY => Some("40".to_string()),
            _ => None,
        };
        assert_eq!(Near::from_config(&Config::parse_with_env("IMAGE=x", env)).unwrap(), None);
        let env = |k: &str| (k == EXTRA_KEY).then(|| "40".to_string());
        assert_eq!(Near::from_config(&Config::parse_with_env("NEAR=europe\nNEAR_MAX_EXTRA_PCT=10", env)).unwrap(), eu(40.0));
    }

    #[test]
    fn budget_and_description() {
        let n = Near { region: Region::Europe, max_extra_pct: 25.0 };
        assert!((n.budget(0.17) - 0.2125).abs() < 1e-12);
        assert_eq!(n.describe(), "Europe, up to +25%");
        assert_eq!(Near { max_extra_pct: 12.5, ..n }.describe(), "Europe, up to +12.5%");
        assert_eq!(Near { region: Region::NorthAmerica, max_extra_pct: 0.0 }.describe(), "North America, up to +0%");
    }

    /// The RunPod table is exactly what the prefix rule says — every listed data centre is in
    /// its region and none in the other — and covers the live list's European ones.
    #[test]
    fn runpod_data_centres_by_region() {
        for r in [Region::Europe, Region::NorthAmerica] {
            for dc in r.runpod_dcs() {
                assert!(r.has_runpod_dc(dc), "{r:?} {dc}");
            }
        }
        for dc in RUNPOD_EUROPE {
            assert!(!Region::NorthAmerica.has_runpod_dc(dc), "{dc}");
        }
        for dc in RUNPOD_NORTH_AMERICA {
            assert!(!Region::Europe.has_runpod_dc(dc), "{dc}");
        }
        // (id, Europe?) — case and spaces don't matter; Asia and Oceania are in neither.
        for (dc, eu) in [("EU-RO-1", true), ("eur-is-2", true), (" EU-DE-1 ", true), ("US-KS-2", false), ("AP-JP-1", false), ("OC-AU-1", false), ("EUROPE", false)] {
            assert_eq!(Region::Europe.has_runpod_dc(dc), eu, "{dc}");
        }
        assert!(!Region::NorthAmerica.has_runpod_dc("AP-IN-1"));
        assert_eq!(RUNPOD_EUROPE.len(), 12);
    }

    #[test]
    fn vast_countries_by_region() {
        // (geolocation, its code, Europe?, North America?)
        for (geo, cc, eu, na) in [
            ("Czechia, CZ", Some("CZ"), true, false),
            ("Estonia, EE", Some("EE"), true, false),
            ("Germany, DE", Some("DE"), true, false),
            ("United Kingdom, GB", Some("GB"), true, false),
            ("Quebec, CA", Some("CA"), false, true),
            ("Michigan, US", Some("US"), false, true),
            ("India, IN", Some("IN"), false, false),
            ("Russia, RU", Some("RU"), false, false),
            ("Turkey, TR", Some("TR"), false, false),
            ("se", Some("SE"), true, false),
            ("Somewhere", None, false, false),
            ("", None, false, false),
            ("Bavaria, DEU", None, false, false),
        ] {
            let got = vast_country(geo);
            assert_eq!(got.as_deref(), cc, "{geo}");
            let code = got.unwrap_or_default();
            assert_eq!((Region::Europe.has_country(&code), Region::NorthAmerica.has_country(&code)), (eu, na), "{geo}");
        }
        for r in [Region::Europe, Region::NorthAmerica] {
            let mut sorted = r.countries().to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted, r.countries(), "{r:?}: sorted, no duplicates");
            assert!(r.countries().iter().all(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_uppercase())));
        }
    }

    #[test]
    fn who_near_placement_reaches() {
        let tiers = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(reaches("vast", &tiers(&["COMMUNITY"])));
        assert!(reaches("runpod", &tiers(&["COMMUNITY", "SECURE"])));
        assert!(reaches("runpod", &tiers(&["secure"])));
        assert!(!reaches("runpod", &tiers(&["COMMUNITY"])), "community has no data centres");
        assert!(!reaches("hetzner", &tiers(&["SECURE"])), "hetzner picks its nearest itself");
        assert!(!reaches("shadeform", &tiers(&["SECURE"])));
    }
}

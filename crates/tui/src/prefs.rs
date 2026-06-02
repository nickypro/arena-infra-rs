//! Tiny persistent UI preferences for the dashboard, stored at
//! `$HOME/.config/arena-tui/prefs` as plain `key=value` lines (no extra deps; the file
//! is trivial to read or hand-edit). Currently just whether pod names are shown short.
//!
//! These are *display* preferences only — they never affect what gets sent to a
//! provider, and they live in the user's home, never in the shared prod config.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    /// Show `apple` instead of `arena8-apple` in the NAME column.
    pub short_names: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        // Short names by default — the fully-qualified prefix is just noise on screen.
        Self { short_names: true }
    }
}

fn prefs_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/arena-tui/prefs"))
}

impl Prefs {
    /// Load preferences, falling back to defaults for anything missing or unreadable.
    pub fn load() -> Self {
        match prefs_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) => Self::parse(&text),
            None => Self::default(),
        }
    }

    /// Best-effort save; a failure (e.g. no `$HOME`) is silently ignored — a lost
    /// preference shouldn't crash the dashboard.
    pub fn save(&self) {
        if let Some(path) = prefs_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, self.serialize());
        }
    }

    fn parse(text: &str) -> Self {
        let mut p = Self::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "short_names" {
                    p.short_names = v.trim() == "true";
                }
            }
        }
        p
    }

    fn serialize(&self) -> String {
        format!("short_names={}\n", self.short_names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_short_names() {
        assert!(Prefs::default().short_names);
    }

    #[test]
    fn round_trips_through_text() {
        let p = Prefs { short_names: false };
        assert_eq!(Prefs::parse(&p.serialize()), p);
    }

    #[test]
    fn missing_key_keeps_default() {
        assert!(Prefs::parse("unrelated=1\n").short_names);
        assert!(!Prefs::parse("short_names=false\n").short_names);
    }
}

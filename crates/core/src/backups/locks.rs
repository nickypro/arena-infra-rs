//! Whether a lock is held — read, never taken. The cron's backup line runs under `flock -n`: a
//! tick that finds its lock held exits at once, so a status that took the lock to test it, even
//! for a moment, could make the cron skip a whole 15-minute backup. Linux lists every `flock`
//! held, with its file's device and inode, in `/proc/locks`; a lock file is held when its own
//! device and inode are there (a waiter's `->` line doesn't count). Elsewhere: unknown, said so.

use std::collections::HashSet;
use std::path::Path;

/// `(major, minor, inode)` of each file a lock is held on, from `/proc/locks`' text (pure): its
/// lines read `1: FLOCK  ADVISORY  WRITE 2416 08:01:131090 0 EOF` (device numbers in hex); a
/// `1: -> FLOCK …` line is a process waiting for that lock, not holding it.
pub fn parse_proc_locks(text: &str) -> HashSet<(u32, u32, u64)> {
    let mut out = HashSet::new();
    for line in text.lines() {
        let mut words = line.split_whitespace().skip(1);
        let Some(kind) = words.next() else { continue };
        if kind == "->" {
            continue;
        }
        // kind, ADVISORY/MANDATORY, READ/WRITE, pid, maj:min:inode
        let Some(id) = words.nth(3) else { continue };
        let mut parts = id.split(':');
        let (Some(maj), Some(min), Some(ino), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
        if let (Ok(maj), Ok(min), Ok(ino)) = (u32::from_str_radix(maj, 16), u32::from_str_radix(min, 16), ino.parse::<u64>()) {
            out.insert((maj, min, ino));
        }
    }
    out
}

/// A `st_dev` as `(major, minor)`, the way Linux (glibc's `gnu_dev_major`/`minor`) splits it.
pub fn dev_parts(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// The locks held now, as [`parse_proc_locks`] reads them — `Err(why)` where they can't be read
/// (not Linux, no `/proc`).
pub fn held() -> Result<HashSet<(u32, u32, u64)>, String> {
    std::fs::read_to_string("/proc/locks").map(|t| parse_proc_locks(&t)).map_err(|e| format!("can't read /proc/locks ({e})"))
}

/// Whether `path`'s lock is held, given what's held (`held`). A file that doesn't exist was
/// never locked: `Ok(false)`.
#[cfg(unix)]
pub fn is_held(path: &Path, held: &HashSet<(u32, u32, u64)>) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            let (maj, min) = dev_parts(m.dev());
            Ok(held.contains(&(maj, min, m.ino())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("can't look at {} ({e})", path.display())),
    }
}

#[cfg(not(unix))]
pub fn is_held(_path: &Path, _held: &HashSet<(u32, u32, u64)>) -> Result<bool, String> {
    Err("locks can't be read on this platform".into())
}

//! Screenshot retention for the captures directory.
//!
//! `vs capture` writes one PNG per shot and never used to remove any of
//! them, so the directory grew without bound. Two consumers prune it now:
//! the daemon applies [`RetentionPolicy::default_cap`] automatically after
//! each capture, and `vs capture clean` lets the user prune explicitly
//! (`--all` / `--older-than` / `--keep`). Both go through [`prune`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default number of newest captures the auto-cap keeps.
pub const DEFAULT_KEEP: usize = 200;

/// Default max age for the auto-cap (30 days).
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Which captures to delete. `all` overrides the other two; otherwise a
/// file is deleted if it is older than `max_age` OR falls outside the
/// newest `keep`.
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub all: bool,
    pub keep: Option<usize>,
    pub max_age: Option<Duration>,
}

impl RetentionPolicy {
    /// The cap the daemon enforces after every capture and that
    /// `vs capture clean` applies when given no flags: keep the newest
    /// [`DEFAULT_KEEP`], drop anything older than [`DEFAULT_MAX_AGE`].
    #[must_use]
    pub fn default_cap() -> Self {
        Self {
            all: false,
            keep: Some(DEFAULT_KEEP),
            max_age: Some(DEFAULT_MAX_AGE),
        }
    }
}

/// Outcome of a [`prune`] pass, for logging / user-facing summaries.
#[derive(Debug, Default, Clone, Copy)]
pub struct PruneStats {
    pub deleted: usize,
    pub freed_bytes: u64,
    pub kept: usize,
}

/// Delete capture PNGs in `dir` per `policy`, treating `now` as the
/// current time (injected for testability).
///
/// Only `*.png` files are considered — any other file in the directory
/// is left untouched. A missing/unreadable directory is a no-op
/// (zeroed stats). Individual files that fail to delete are skipped and
/// counted as kept rather than aborting the pass.
#[must_use]
pub fn prune(dir: &Path, policy: &RetentionPolicy, now: SystemTime) -> PruneStats {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return PruneStats::default();
    };

    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        files.push((path, mtime, meta.len()));
    }

    // Newest first, so index >= keep marks the older surplus.
    files.sort_by_key(|f| std::cmp::Reverse(f.1));

    let mut stats = PruneStats::default();
    for (idx, (path, mtime, size)) in files.iter().enumerate() {
        let too_old = policy.max_age.is_some_and(|age| {
            now.duration_since(*mtime)
                .is_ok_and(|elapsed| elapsed > age)
        });
        let beyond_keep = policy.keep.is_some_and(|k| idx >= k);
        let doomed = policy.all || too_old || beyond_keep;

        if doomed && std::fs::remove_file(path).is_ok() {
            stats.deleted += 1;
            stats.freed_bytes += *size;
        } else {
            stats.kept += 1;
        }
    }
    stats
}

/// Parse a human duration: a number with an optional `s`/`m`/`h`/`d`
/// suffix (bare number = seconds). Returns `None` on anything else.
#[must_use]
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return None,
    };
    Some(Duration::from_secs(n.checked_mul(mult)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn touch(dir: &Path, name: &str, bytes: usize, age: Duration, now: SystemTime) {
        let p = dir.join(name);
        std::fs::write(&p, vec![0u8; bytes]).unwrap();
        // Backdate the mtime so age-based pruning is deterministic.
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(now - age).unwrap();
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("10y"), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn keep_newest_n() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for i in 0..5 {
            touch(
                dir.path(),
                &format!("wk-{i}.png"),
                10,
                Duration::from_secs(i * 60),
                now,
            );
        }
        let pol = RetentionPolicy {
            all: false,
            keep: Some(2),
            max_age: None,
        };
        let s = prune(dir.path(), &pol, now);
        assert_eq!(s.deleted, 3);
        assert_eq!(s.kept, 2);
        // The two newest (smallest age) survive.
        assert!(dir.path().join("wk-0.png").exists());
        assert!(dir.path().join("wk-1.png").exists());
        assert!(!dir.path().join("wk-2.png").exists());
    }

    #[test]
    fn drop_older_than() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        touch(dir.path(), "fresh.png", 10, Duration::from_secs(60), now);
        touch(
            dir.path(),
            "stale.png",
            10,
            Duration::from_secs(3 * 86400),
            now,
        );
        let pol = RetentionPolicy {
            all: false,
            keep: None,
            max_age: Some(Duration::from_secs(86400)),
        };
        let s = prune(dir.path(), &pol, now);
        assert_eq!(s.deleted, 1);
        assert!(dir.path().join("fresh.png").exists());
        assert!(!dir.path().join("stale.png").exists());
    }

    #[test]
    fn all_wipes_everything_but_non_png() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        touch(dir.path(), "a.png", 10, Duration::ZERO, now);
        touch(dir.path(), "b.png", 10, Duration::ZERO, now);
        std::fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();
        let pol = RetentionPolicy {
            all: true,
            ..Default::default()
        };
        let s = prune(dir.path(), &pol, now);
        assert_eq!(s.deleted, 2);
        assert!(
            dir.path().join("notes.txt").exists(),
            "non-png must be left alone"
        );
    }

    #[test]
    fn missing_dir_is_noop() {
        let s = prune(
            Path::new("/no/such/vibesurfer/captures"),
            &RetentionPolicy::default_cap(),
            SystemTime::now(),
        );
        assert_eq!(s.deleted, 0);
        assert_eq!(s.kept, 0);
    }
}

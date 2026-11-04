//! Scheduling hints: a running average of how long each compile took in
//! past builds, persisted locally in `.anubis-cache/hints`.
//!
//! The job system starts ready jobs with larger hints first, so the slowest
//! compiles don't end up as the tail of a clean build. Hints only affect
//! ordering, never results, so a missing or stale file is harmless. The
//! file is per-machine (durations depend on the machine) and not meant to
//! be checked in.

use camino::Utf8PathBuf;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

const HINTS_VERSION: u32 = 1;

/// Weight of a new sample in the running average (out of 4).
const NEW_SAMPLE_WEIGHT: u64 = 1;

#[derive(Serialize, Deserialize)]
struct HintsFile {
    version: u32,
    /// key -> average duration in milliseconds
    durations_ms: BTreeMap<String, u32>,
}

#[derive(Debug)]
pub struct Hints {
    path: Option<Utf8PathBuf>,
    durations_ms: DashMap<String, u32>,
    /// Mean of the loaded hints; used for actions never seen before so a new
    /// file isn't automatically scheduled last.
    default_ms: u32,
    dirty: AtomicBool,
}

impl Hints {
    /// In-memory only (tests, or no cache directory).
    pub fn empty() -> Self {
        Hints { path: None, durations_ms: DashMap::new(), default_ms: 0, dirty: AtomicBool::new(false) }
    }

    /// Load from `path`. A missing, unreadable or old-format file gives no hints.
    pub fn load(path: Utf8PathBuf) -> Self {
        let mut hints = Self::empty();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(file) = rmp_serde::from_slice::<HintsFile>(&bytes) {
                if file.version == HINTS_VERSION && !file.durations_ms.is_empty() {
                    let sum: u64 = file.durations_ms.values().map(|&v| v as u64).sum();
                    hints.default_ms = (sum / file.durations_ms.len() as u64) as u32;
                    hints.durations_ms = file.durations_ms.into_iter().collect();
                }
            }
        }
        hints.path = Some(path);
        hints
    }

    /// Expected duration of `key` in ms (the mean of known hints if unseen).
    pub fn expected_ms(&self, key: &str) -> u32 {
        self.durations_ms.get(key).map(|v| *v).unwrap_or(self.default_ms)
    }

    pub fn get(&self, key: &str) -> Option<u32> {
        self.durations_ms.get(key).map(|v| *v)
    }

    /// Record a measured duration, folding it into the running average.
    pub fn record(&self, key: &str, duration: std::time::Duration) {
        let sample = duration.as_millis().min(u32::MAX as u128) as u64;
        let mut entry = self.durations_ms.entry(key.to_string()).or_insert(sample as u32);
        let old = *entry as u64;
        *entry = ((old * (4 - NEW_SAMPLE_WEIGHT) + sample * NEW_SAMPLE_WEIGHT) / 4) as u32;
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Write the file if anything changed (atomically, via a temp file).
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let file = HintsFile {
            version: HINTS_VERSION,
            durations_ms: self.durations_ms.iter().map(|e| (e.key().clone(), *e.value())).collect(),
        };
        let bytes = rmp_serde::to_vec_named(&file)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn running_average_round_trips_and_defaults_to_mean() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("hints")).unwrap();

        let hints = Hints::load(path.clone());
        assert_eq!(hints.expected_ms("a"), 0);
        hints.record("a", Duration::from_millis(1000));
        assert_eq!(hints.get("a"), Some(1000)); // first sample taken as-is
        hints.record("a", Duration::from_millis(2000));
        assert_eq!(hints.get("a"), Some(1250)); // 3/4 old + 1/4 new
        hints.record("b", Duration::from_millis(250));
        hints.save().unwrap();

        let loaded = Hints::load(path.clone());
        assert_eq!(loaded.get("a"), Some(1250));
        assert_eq!(loaded.get("b"), Some(250));
        assert_eq!(loaded.expected_ms("unseen"), 750); // mean of known

        std::fs::write(&path, b"garbage").unwrap();
        assert_eq!(Hints::load(path).get("a"), None);
    }
}

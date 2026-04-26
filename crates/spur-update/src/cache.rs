use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::debug;

const CACHE_FILENAME: &str = "update-check.json";
const CACHE_TTL_HOURS: i64 = 1;

/// Cached result of a version check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCache {
    pub checked_at: DateTime<Utc>,
    pub current_version: String,
    pub latest_tag: String,
    pub update_available: bool,
    pub channel: String,
}

/// Read cache from disk. Returns None if file doesn't exist or is corrupt.
pub fn read_cache(cache_dir: &Path) -> Option<UpdateCache> {
    let path = cache_dir.join(CACHE_FILENAME);
    let data = std::fs::read_to_string(&path).ok()?;
    let cache: UpdateCache = serde_json::from_str(&data).ok()?;
    debug!(path = %path.display(), "read update cache");
    Some(cache)
}

/// Write cache to disk. Creates directory if needed.
/// Silently ignores write failures (non-critical).
pub fn write_cache(cache_dir: &Path, cache: &UpdateCache) {
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        debug!("failed to create cache dir {}: {e}", cache_dir.display());
        return;
    }
    let path = cache_dir.join(CACHE_FILENAME);
    match serde_json::to_string_pretty(cache) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                debug!("failed to write cache {}: {e}", path.display());
            }
        }
        Err(e) => debug!("failed to serialize cache: {e}"),
    }
}

/// Returns true if the cache is still fresh (less than 1 hour old).
pub fn is_cache_fresh(cache: &UpdateCache) -> bool {
    let age = Utc::now() - cache.checked_at;
    age < Duration::hours(CACHE_TTL_HOURS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cache_within_ttl() {
        let cache = UpdateCache {
            checked_at: Utc::now(),
            current_version: "0.1.0".into(),
            latest_tag: "v0.2.0".into(),
            update_available: true,
            channel: "stable".into(),
        };
        assert!(is_cache_fresh(&cache));
    }

    #[test]
    fn stale_cache_beyond_ttl() {
        let cache = UpdateCache {
            checked_at: Utc::now() - Duration::hours(2),
            current_version: "0.1.0".into(),
            latest_tag: "v0.2.0".into(),
            update_available: true,
            channel: "stable".into(),
        };
        assert!(!is_cache_fresh(&cache));
    }

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join("spur-update-test-cache");
        let _ = std::fs::remove_dir_all(&dir);

        let cache = UpdateCache {
            checked_at: Utc::now(),
            current_version: "0.1.0".into(),
            latest_tag: "v0.2.2".into(),
            update_available: true,
            channel: "stable".into(),
        };

        write_cache(&dir, &cache);
        let loaded = read_cache(&dir).expect("should read back cache");
        assert_eq!(loaded.latest_tag, "v0.2.2");
        assert!(loaded.update_available);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

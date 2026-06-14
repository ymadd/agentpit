use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const REPO_OWNER: &str = "ymadd";
const REPO_NAME: &str = "agentpit";
const BIN_NAME: &str = "agentpit";
const CACHE_TTL: Duration = Duration::from_secs(60);

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionCache {
    pub checked_at: u64,
    pub latest_tag: String,
}

pub fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("agentpit")
            .join("version_check.json"),
    )
}

pub fn load_cache() -> Option<VersionCache> {
    let path = cache_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_cache(cache: &VersionCache) -> Result<()> {
    let path = cache_path().ok_or_else(|| anyhow!("cache dir unavailable"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = serde_json::to_string(cache)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn is_fresh(cache: &VersionCache) -> bool {
    now_secs().saturating_sub(cache.checked_at) < CACHE_TTL.as_secs()
}

pub fn version_is_newer(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

fn parse_version(s: &str) -> (u64, u64, u64) {
    let stripped = s.trim_start_matches('v');
    let mut parts = stripped.split(['.', '-', '+']);
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

pub fn fetch_latest_tag() -> Result<String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()
        .map_err(|e| anyhow!("self_update config error: {e}"))?
        .fetch()
        .map_err(|e| anyhow!("failed to fetch releases from {REPO_OWNER}/{REPO_NAME}: {e}"))?;
    let latest = releases
        .first()
        .ok_or_else(|| anyhow!("no releases published for {REPO_OWNER}/{REPO_NAME}"))?;
    Ok(latest.version.clone())
}

pub fn refresh_cache() -> Result<VersionCache> {
    let latest_tag = fetch_latest_tag()?;
    let cache = VersionCache {
        checked_at: now_secs(),
        latest_tag,
    };
    save_cache(&cache)?;
    Ok(cache)
}

pub fn ensure_fresh_cache() -> Result<VersionCache> {
    if let Some(cache) = load_cache()
        && is_fresh(&cache)
    {
        return Ok(cache);
    }
    refresh_cache()
}

pub fn compute_banner() -> Option<String> {
    let cache = match ensure_fresh_cache() {
        Ok(c) => c,
        Err(_) => {
            // Poison the cache for CACHE_TTL so a 404 / offline run does not
            // re-hit the network on every subsequent startup.
            let _ = save_cache(&VersionCache {
                checked_at: now_secs(),
                latest_tag: String::new(),
            });
            return None;
        }
    };
    if version_is_newer(&cache.latest_tag, current_version()) {
        Some(format!(
            "[update available: {} -> {} (run `agentpit update`)]",
            current_version(),
            cache.latest_tag.trim_start_matches('v')
        ))
    } else {
        None
    }
}

pub struct UpdateOutcome {
    pub already_up_to_date: bool,
    pub installed_version: String,
}

pub fn perform_update() -> Result<UpdateOutcome> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .bin_path_in_archive(BIN_NAME)
        .target(self_update::get_target())
        .show_download_progress(true)
        .current_version(current_version())
        .build()
        .map_err(|e| anyhow!("self_update config error: {e}"))?
        .update()
        .map_err(|e| anyhow!("update failed: {e}"))?;
    Ok(UpdateOutcome {
        already_up_to_date: status.uptodate(),
        installed_version: status.version().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_versions() {
        assert_eq!(parse_version("1.2.3"), (1, 2, 3));
        assert_eq!(parse_version("v1.2.3"), (1, 2, 3));
        assert_eq!(parse_version("0.4.0"), (0, 4, 0));
    }

    #[test]
    fn parses_prerelease_as_base_version() {
        assert_eq!(parse_version("1.2.3-beta"), (1, 2, 3));
        assert_eq!(parse_version("1.2.3+build.42"), (1, 2, 3));
    }

    #[test]
    fn detects_newer_versions() {
        assert!(version_is_newer("0.2.0", "0.1.0"));
        assert!(version_is_newer("v1.0.0", "0.99.99"));
        assert!(version_is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn equal_or_older_is_not_newer() {
        assert!(!version_is_newer("0.1.0", "0.1.0"));
        assert!(!version_is_newer("0.1.0", "0.2.0"));
        assert!(!version_is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn fresh_cache_is_within_window() {
        let cache = VersionCache {
            checked_at: now_secs(),
            latest_tag: "v0.1.0".into(),
        };
        assert!(is_fresh(&cache));
    }

    #[test]
    fn stale_cache_is_outside_window() {
        let cache = VersionCache {
            checked_at: now_secs().saturating_sub(CACHE_TTL.as_secs() + 10),
            latest_tag: "v0.1.0".into(),
        };
        assert!(!is_fresh(&cache));
    }

    #[test]
    fn empty_poison_tag_is_not_newer() {
        // poisoned cache (empty tag after a 404) must not trigger a banner.
        assert!(!version_is_newer("", current_version()));
    }
}

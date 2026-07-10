use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const REPO_OWNER: &str = "ymadd";
const REPO_NAME: &str = "agentpit";
const BIN_NAME: &str = "agentpit";
#[cfg(not(windows))]
const DASHBOARD_BIN_NAME: &str = "agentpit-dashboard";
#[cfg(windows)]
const DASHBOARD_BIN_NAME: &str = "agentpit-dashboard.exe";
const DASHBOARD_VERSION_MARKER: &str = "dashboard-version";
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
    pub dashboard: DashboardUpdateOutcome,
}

#[derive(Debug)]
pub enum DashboardUpdateOutcome {
    NotInstalled,
    UpToDate {
        path: PathBuf,
    },
    Updated {
        path: PathBuf,
        installed_version: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

pub fn perform_update() -> Result<UpdateOutcome> {
    let target = self_update::get_target();
    // A release contains both `agentpit-<target>` and
    // `agentpit-dashboard-<target>`. Include the target in the identifier so the CLI updater
    // cannot accidentally select the dashboard asset (both names contain "agentpit").
    let asset_identifier = asset_identifier(BIN_NAME, target);
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .bin_path_in_archive(BIN_NAME)
        .target(target)
        .identifier(&asset_identifier)
        .show_download_progress(true)
        .current_version(current_version())
        .build()
        .map_err(|e| anyhow!("self_update config error: {e}"))?
        .update()
        .map_err(|e| anyhow!("update failed: {e}"))?;
    let installed_version = status.version().to_string();
    let dashboard = sync_installed_dashboard(&installed_version);
    Ok(UpdateOutcome {
        already_up_to_date: status.uptodate(),
        installed_version,
        dashboard,
    })
}

fn asset_identifier(bin_name: &str, target: &str) -> String {
    format!(
        "{}-{target}",
        bin_name.trim_end_matches(std::env::consts::EXE_SUFFIX)
    )
}

/// Find the dashboard using the same precedence as `agentpit dashboard`: explicit override,
/// sibling of the CLI executable, then PATH. A missing dashboard stays missing; `agentpit update`
/// never installs a new desktop app behind the user's back.
pub fn locate_dashboard() -> Option<PathBuf> {
    let explicit = std::env::var_os("AGENTPIT_DASHBOARD_BIN").map(PathBuf::from);
    let current_exe = std::env::current_exe().ok();
    locate_dashboard_with(
        explicit.as_deref(),
        current_exe.as_deref(),
        std::env::var_os("PATH").as_deref(),
        DASHBOARD_BIN_NAME,
    )
}

fn locate_dashboard_with(
    explicit: Option<&Path>,
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
    bin_name: &str,
) -> Option<PathBuf> {
    if let Some(path) = explicit
        && path.is_file()
    {
        return Some(path.to_path_buf());
    }
    if let Some(path) = current_exe
        .and_then(Path::parent)
        .map(|dir| dir.join(bin_name))
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    path_env
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(bin_name))
        .find(|path| path.is_file())
}

fn dashboard_version_marker_path() -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("agentpit")
            .join(DASHBOARD_VERSION_MARKER),
    )
}

fn normalized_version(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

#[derive(Debug, Serialize, Deserialize)]
struct DashboardVersionMarker {
    version: String,
    path: String,
    len: u64,
    modified_at: u64,
}

fn dashboard_file_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_at = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((metadata.len(), modified_at))
}

fn dashboard_marker_matches(version: &str, dashboard_path: &Path) -> bool {
    let Some(path) = dashboard_version_marker_path() else {
        return false;
    };
    let Some((len, modified_at)) = dashboard_file_identity(dashboard_path) else {
        return false;
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<DashboardVersionMarker>(&raw).ok())
        .is_some_and(|stored| {
            normalized_version(&stored.version) == normalized_version(version)
                && stored.path == dashboard_path.to_string_lossy()
                && stored.len == len
                && stored.modified_at == modified_at
        })
}

fn save_dashboard_marker(version: &str, dashboard_path: &Path) {
    let Some(path) = dashboard_version_marker_path() else {
        return;
    };
    let Some((len, modified_at)) = dashboard_file_identity(dashboard_path) else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let marker = DashboardVersionMarker {
        version: normalized_version(version).to_string(),
        path: dashboard_path.to_string_lossy().into_owned(),
        len,
        modified_at,
    };
    if let Ok(raw) = serde_json::to_vec(&marker) {
        let _ = std::fs::write(path, raw);
    }
}

/// Ensure the binary at `path` is executable. Gzip release assets carry no file mode, and
/// `self_update` only restores permissions when replacing the running executable
/// (`self_replace`); installing to any other path is a plain `Move` that leaves the
/// extracted file 0644 — spawning it then fails with EACCES.
#[cfg(not(windows))]
pub fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    let mode = permissions.mode();
    if mode & 0o111 != 0o111 {
        permissions.set_mode(mode | 0o755);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn ensure_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn sync_installed_dashboard(version: &str) -> DashboardUpdateOutcome {
    let Some(path) = locate_dashboard() else {
        return DashboardUpdateOutcome::NotInstalled;
    };
    if dashboard_marker_matches(version, &path) {
        // Heal a dashboard left non-executable by a pre-0.1.23 co-update (the marker can
        // match a binary whose executable bit was never set).
        let _ = ensure_executable(&path);
        return DashboardUpdateOutcome::UpToDate { path };
    }

    let target = self_update::get_target();
    let asset_identifier = asset_identifier(DASHBOARD_BIN_NAME, target);
    let tag = format!("v{}", normalized_version(version));
    let result = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(DASHBOARD_BIN_NAME)
        .bin_path_in_archive(DASHBOARD_BIN_NAME)
        .bin_install_path(&path)
        .target(target)
        .identifier(&asset_identifier)
        .target_version_tag(&tag)
        // A target tag forces this exact release; use a synthetic old version so the library's
        // status remains an update rather than depending on desktop binary introspection.
        .current_version("0.0.0")
        .show_download_progress(true)
        .show_output(false)
        .no_confirm(true)
        .build()
        .map_err(|error| anyhow!("self_update config error: {error}"))
        .and_then(|updater| {
            updater
                .update()
                .map_err(|error| anyhow!("update failed: {error}"))
        });

    match result {
        Ok(status) => {
            let installed_version = status.version().to_string();
            if let Err(error) = ensure_executable(&path) {
                return DashboardUpdateOutcome::Failed {
                    path,
                    error: format!("{error:#}"),
                };
            }
            save_dashboard_marker(&installed_version, &path);
            DashboardUpdateOutcome::Updated {
                path,
                installed_version,
            }
        }
        Err(error) => DashboardUpdateOutcome::Failed {
            path,
            error: format!("{error:#}"),
        },
    }
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

    #[test]
    fn asset_identifiers_distinguish_cli_from_dashboard() {
        let target = "aarch64-apple-darwin";
        let cli = asset_identifier(BIN_NAME, target);
        let dashboard = asset_identifier(DASHBOARD_BIN_NAME, target);
        assert_eq!(cli, "agentpit-aarch64-apple-darwin");
        assert_eq!(dashboard, "agentpit-dashboard-aarch64-apple-darwin");
        assert!(!format!("agentpit-dashboard-{target}.gz").contains(&cli));
    }

    #[test]
    fn dashboard_locator_prefers_override_then_sibling_then_path() {
        let temp = tempfile::tempdir().unwrap();
        let explicit = temp.path().join("explicit-dashboard");
        let sibling_dir = temp.path().join("sibling");
        let path_dir = temp.path().join("path");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::write(&explicit, b"explicit").unwrap();
        std::fs::write(sibling_dir.join("dashboard"), b"sibling").unwrap();
        std::fs::write(path_dir.join("dashboard"), b"path").unwrap();
        let current_exe = sibling_dir.join("agentpit");
        let path_env = std::env::join_paths([&path_dir]).unwrap();

        assert_eq!(
            locate_dashboard_with(
                Some(&explicit),
                Some(&current_exe),
                Some(&path_env),
                "dashboard",
            ),
            Some(explicit)
        );
        assert_eq!(
            locate_dashboard_with(None, Some(&current_exe), Some(&path_env), "dashboard"),
            Some(sibling_dir.join("dashboard"))
        );
        assert_eq!(
            locate_dashboard_with(None, None, Some(&path_env), "dashboard"),
            Some(path_dir.join("dashboard"))
        );
    }

    #[test]
    fn version_normalization_accepts_release_tags() {
        assert_eq!(normalized_version("v0.1.21\n"), "0.1.21");
        assert_eq!(normalized_version("0.1.21"), "0.1.21");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_restores_missing_exec_bits() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("dashboard");
        std::fs::write(&bin, b"bin").unwrap();
        // Regression: a gz-extracted asset installed via a plain Move lands as 0644.
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        ensure_executable(&bin).unwrap();
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode();
        assert_eq!(mode & 0o755, 0o755);
        // Idempotent on an already-executable file.
        ensure_executable(&bin).unwrap();
        assert_eq!(
            std::fs::metadata(&bin).unwrap().permissions().mode() & 0o755,
            0o755
        );
    }
}

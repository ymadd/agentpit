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
    /// macOS: replacing a binary inside a signed `.app` bundle breaks the bundle seal, and
    /// Apple Silicon refuses to launch a broken-seal app. After an in-place update the touched
    /// bundles are ad-hoc re-signed; this carries the error when that re-sign failed (the app
    /// may not relaunch until `codesign --force --deep --sign - <app>` is run by hand).
    pub resign_error: Option<String>,
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

pub fn perform_update(quiet: bool) -> Result<UpdateOutcome> {
    let latest = fetch_latest_tag()?;
    if !version_is_newer(&latest, current_version()) {
        // CLI already current — still co-heal an installed dashboard (its binary can lag
        // after a partial earlier update) and report cleanly.
        let dashboard = sync_installed_dashboard(current_version());
        let resign_error = resign_touched_bundles(false, &dashboard, current_version());
        return Ok(UpdateOutcome {
            already_up_to_date: true,
            installed_version: current_version().to_string(),
            dashboard,
            resign_error,
        });
    }

    if !is_supported_release_tag(&latest) {
        return Err(anyhow!(
            "latest release tag {latest:?} is not a plain vMAJOR.MINOR.PATCH — refusing to update"
        ));
    }
    let tag = format!("v{}", normalized_version(&latest));
    let target = self_update::get_target();
    let asset_name = format!("{}.gz", asset_identifier(BIN_NAME, target));
    if !quiet {
        eprintln!("downloading {asset_name} ({tag}) with checksum verification…");
    }
    let binary = fetch_verified_binary(&tag, &asset_name)?;

    // Stage next to the current exe (same filesystem) and swap in via self-replace — the
    // same mechanism self_update used, minus its unverified download. Make it executable
    // BEFORE it is ever swapped in, so no window exists where the installed path is a
    // non-executable file.
    let current_exe = std::env::current_exe().context("cannot locate the running executable")?;
    let staged = current_exe.with_extension(format!("update.{}", std::process::id()));
    write_new_file(&staged, &binary)
        .with_context(|| format!("failed to stage update at {}", staged.display()))?;
    let staged_ok = ensure_executable(&staged)
        .and_then(|()| assert_staged_version(&staged, normalized_version(&latest)));
    if let Err(error) = staged_ok {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    let replaced = self_replace::self_replace(&staged);
    let _ = std::fs::remove_file(&staged);
    replaced.context("failed to replace the running executable")?;

    let installed_version = normalized_version(&latest).to_string();
    let dashboard = sync_installed_dashboard(&installed_version);
    let resign_error = resign_touched_bundles(true, &dashboard, &installed_version);
    Ok(UpdateOutcome {
        already_up_to_date: false,
        installed_version,
        dashboard,
        resign_error,
    })
}

/// Write `bytes` to `path`, failing if anything is already there.
///
/// The staging path is derived from the executable's own path plus our pid, so it is
/// predictable. A plain `fs::write` opens with `O_CREAT|O_TRUNC` and **follows symlinks**,
/// so anyone able to create files in that directory could pre-place a symlink and have the
/// update's bytes land on an arbitrary file. `create_new` (`O_EXCL`) refuses to follow a
/// symlink or reuse an existing entry, so a collision fails closed instead.
/// Nothing is removed first: clearing the path would defeat the guard, since a symlink
/// planted there would simply be deleted and recreated as a regular file. An occupied path
/// (a leftover from a crashed update that happened to share our pid) fails the update
/// rather than silently reusing it.
fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A release tag we are willing to turn into a download URL and a bundle version string.
/// `fetch_latest_tag` returns whatever the release is named, and that value flows into URL
/// paths and `PlistBuddy` arguments, so anything that is not a plain `vMAJOR.MINOR.PATCH`
/// (no slashes, no whitespace) is rejected rather than sanitized.
fn is_supported_release_tag(tag: &str) -> bool {
    let core = tag.trim().trim_start_matches('v');
    let mut parts = core.split('.');
    let ok = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    });
    ok && parts.next().is_none()
}

/// Run the staged binary's `--version` and require it to report `expected`.
///
/// The checksum only proves the bytes match the digest the release published; it says
/// nothing about *which* build those bytes are. A release cut from the wrong ref (the
/// workflow builds the current branch, not the requested tag) yields a perfectly
/// checksummed binary of some other version. This catches that before the swap.
fn assert_staged_version(staged: &Path, expected: &str) -> Result<()> {
    let output = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .with_context(|| format!("staged binary at {} would not run", staged.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "staged binary exited {} on --version — refusing to install",
            output.status
        ));
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    let matches = reported
        .split_whitespace()
        .any(|token| normalized_version(token) == expected);
    if !matches {
        return Err(anyhow!(
            "staged binary reports {:?} but the release claims {expected} — refusing to install",
            reported.trim()
        ));
    }
    Ok(())
}

/// Download a release asset plus its `.sha256` sibling, check the digest, and gunzip.
///
/// **What this does and does not prove.** The digest is fetched from the same release, over
/// the same channel, under the same write authority as the payload, so it establishes
/// *integrity* (the bytes are the ones the release published — a truncated or corrupted
/// download is caught) and NOT *authenticity* (anyone who can replace the asset can replace
/// its `.sha256` too). Publisher authenticity needs a signature verified against a key that
/// does not travel with the release; that is tracked separately. The staged binary's
/// self-reported version is checked as a second, independent gate.
fn fetch_verified_binary(tag: &str, asset_name: &str) -> Result<Vec<u8>> {
    let gz = download_bytes(&asset_url(tag, asset_name))?;
    let sha_bytes = download_bytes(&asset_url(tag, &format!("{asset_name}.sha256")))?;
    verify_and_decompress(&gz, &String::from_utf8_lossy(&sha_bytes), asset_name)
}

/// The integrity gate itself, split out from the network so it can be tested against
/// tampered inputs offline: parse the digest line, compare it to the payload's actual
/// SHA-256, and only then decompress. Every failure path returns `Err` — the caller writes
/// nothing to disk unless this returns bytes.
fn verify_and_decompress(gz: &[u8], sha_line: &str, asset_name: &str) -> Result<Vec<u8>> {
    let expected = parse_sha256_line(sha_line)
        .ok_or_else(|| anyhow!("malformed .sha256 asset for {asset_name}: {sha_line:?}"))?;
    let actual = sha256_hex(gz);
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(anyhow!(
            "checksum mismatch for {asset_name}: expected {expected}, got {actual} — refusing to install"
        ));
    }
    gunzip(gz).with_context(|| format!("failed to decompress {asset_name}"))
}

fn asset_url(tag: &str, asset_name: &str) -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{tag}/{asset_name}")
}

fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    self_update::Download::from_url(url)
        .download_to(&mut buf)
        .map_err(|e| anyhow!("download failed for {url}: {e}"))?;
    Ok(buf)
}

/// First token of a `shasum -a 256` output line (`<hex>  <filename>`), if it looks like a
/// SHA-256 digest.
fn parse_sha256_line(line: &str) -> Option<String> {
    let token = line.split_whitespace().next()?;
    (token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())).then(|| token.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ceiling on a decompressed release binary. Well above any real build (the CLI and the
/// dashboard are tens of MiB), and low enough that a gzip bomb — which passes the digest
/// check when the digest is what was published — cannot exhaust memory during decompression.
const MAX_DECOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;

fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    let mut limited = flate2::read::GzDecoder::new(bytes).take(MAX_DECOMPRESSED_BYTES + 1);
    limited.read_to_end(&mut out)?;
    if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(std::io::Error::other(format!(
            "decompressed payload exceeds {MAX_DECOMPRESSED_BYTES} bytes — refusing to install"
        )));
    }
    Ok(out)
}

/// The nearest proper ancestor of the executable `path` that is a macOS `.app` bundle root.
fn enclosing_app_bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.extension() == Some(OsStr::new("app")))
        .map(Path::to_path_buf)
}

/// Ad-hoc re-sign every `.app` bundle whose nested binaries this update replaced, restoring the
/// bundle seal (see [`UpdateOutcome::resign_error`]). The bundle's `Info.plist` version keys are
/// rewritten to `installed_version` first, so Finder and the OS report the version the binaries
/// actually are — and so the rewrite is covered by the fresh seal. Returns the combined error
/// text when any step failed; `None` on success, off macOS, or when no bundle was touched.
fn resign_touched_bundles(
    cli_updated: bool,
    dashboard: &DashboardUpdateOutcome,
    installed_version: &str,
) -> Option<String> {
    let mut bundles: Vec<PathBuf> = Vec::new();
    if cli_updated
        && let Some(bundle) = std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(enclosing_app_bundle)
    {
        bundles.push(bundle);
    }
    if let DashboardUpdateOutcome::Updated { path, .. } = dashboard
        && let Some(bundle) = enclosing_app_bundle(path)
        && !bundles.contains(&bundle)
    {
        bundles.push(bundle);
    }

    let mut errors: Vec<String> = Vec::new();
    for bundle in bundles {
        if let Err(error) = update_bundle_version_plist(&bundle, installed_version) {
            errors.push(error);
        }
        if let Err(error) = resign_app_bundle(&bundle) {
            errors.push(error);
        }
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// Rewrite `CFBundleShortVersionString` / `CFBundleVersion` in the bundle's `Info.plist` to
/// `version`. Without this, an in-place binary update leaves the plist advertising the old
/// version (Finder's Get Info, `defaults read`) even though the binaries are new. Must run
/// *before* [`resign_app_bundle`] — editing the plist afterwards would break the fresh seal.
#[cfg(target_os = "macos")]
fn update_bundle_version_plist(bundle: &Path, version: &str) -> std::result::Result<(), String> {
    let plist = bundle.join("Contents").join("Info.plist");
    if !plist.is_file() {
        return Ok(()); // not a standard bundle layout; nothing to rewrite
    }
    let version = version.trim().trim_start_matches('v');
    for key in ["CFBundleShortVersionString", "CFBundleVersion"] {
        let output = std::process::Command::new("/usr/libexec/PlistBuddy")
            .arg("-c")
            .arg(format!("Set :{key} {version}"))
            .arg(&plist)
            .output()
            .map_err(|error| format!("failed to run PlistBuddy on {}: {error}", plist.display()))?;
        if !output.status.success() {
            return Err(format!(
                "PlistBuddy Set {key} failed on {}: {}",
                plist.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn update_bundle_version_plist(_bundle: &Path, _version: &str) -> std::result::Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn resign_app_bundle(bundle: &Path) -> std::result::Result<(), String> {
    let output = std::process::Command::new("codesign")
        .args([
            OsStr::new("--force"),
            OsStr::new("--deep"),
            OsStr::new("--sign"),
            OsStr::new("-"),
        ])
        .arg(bundle)
        .output()
        .map_err(|error| format!("failed to run codesign on {}: {error}", bundle.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "codesign failed on {} ({}): {}",
        bundle.display(),
        output.status,
        stderr.trim()
    ))
}

#[cfg(not(target_os = "macos"))]
fn resign_app_bundle(_bundle: &Path) -> std::result::Result<(), String> {
    Ok(())
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
    let asset_name = format!("{}.gz", asset_identifier(DASHBOARD_BIN_NAME, target));
    let tag = format!("v{}", normalized_version(version));
    let result = fetch_verified_binary(&tag, &asset_name).and_then(|binary| {
        // Stage as a sibling (same filesystem) and rename into place so a crash mid-write
        // never leaves a half-written dashboard binary. The executable bit goes on the
        // STAGED file: a gz asset extracts as 0644, and chmod-after-rename left a window
        // where the installed dashboard was present but not runnable if anything failed
        // in between.
        let staged = path.with_extension(format!("update.{}", std::process::id()));
        write_new_file(&staged, &binary)
            .with_context(|| format!("failed to stage {}", staged.display()))?;
        if let Err(error) = ensure_executable(&staged) {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        std::fs::rename(&staged, &path).map_err(|error| {
            let _ = std::fs::remove_file(&staged);
            anyhow!("failed to install {}: {error}", path.display())
        })?;
        Ok(())
    });

    match result {
        Ok(()) => {
            let installed_version = normalized_version(version).to_string();
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
    fn enclosing_app_bundle_finds_nearest_app_ancestor() {
        assert_eq!(
            enclosing_app_bundle(Path::new(
                "/Applications/agentpit.app/Contents/MacOS/agentpit"
            )),
            Some(PathBuf::from("/Applications/agentpit.app"))
        );
        // Outside a bundle (Homebrew-style install) there is nothing to re-sign.
        assert_eq!(
            enclosing_app_bundle(Path::new("/Users/x/.local/bin/agentpit")),
            None
        );
        // The binary itself never counts as a bundle root.
        assert_eq!(enclosing_app_bundle(Path::new("/tmp/not-an.app")), None);
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

    #[test]
    fn sha256_verification_helpers_work() {
        // Digest of the empty input is a well-known constant.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // shasum output line parses to its digest token; junk does not.
        let line =
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  agentpit-x.gz\n";
        assert_eq!(
            parse_sha256_line(line).as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(parse_sha256_line("not a digest"), None);
        assert_eq!(parse_sha256_line(""), None);
    }

    /// The gate must *reject*, not merely compute: a payload whose digest does not match
    /// its `.sha256` line never reaches the caller, so nothing is ever staged or installed.
    #[test]
    fn verify_and_decompress_rejects_tampered_payloads() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"the genuine release binary").unwrap();
        let gz = enc.finish().unwrap();
        let good_line = format!("{}  agentpit-x.gz\n", sha256_hex(&gz));

        // Honest asset: verifies and decompresses.
        assert_eq!(
            verify_and_decompress(&gz, &good_line, "agentpit-x.gz").unwrap(),
            b"the genuine release binary"
        );

        // One flipped byte in the payload: rejected, with the digests named.
        let mut tampered = gz.clone();
        *tampered.last_mut().unwrap() ^= 0xff;
        let err = format!(
            "{:#}",
            verify_and_decompress(&tampered, &good_line, "agentpit-x.gz").unwrap_err()
        );
        assert!(err.contains("checksum mismatch"), "got: {err}");

        // A digest line for a *different* payload (substituted .sha256): also rejected.
        let other = format!("{}  agentpit-x.gz\n", sha256_hex(b"something else"));
        assert!(verify_and_decompress(&gz, &other, "agentpit-x.gz").is_err());

        // Missing / malformed digest line: rejected rather than skipped.
        for line in ["", "not-a-digest  agentpit-x.gz", "404: Not Found"] {
            let err = format!(
                "{:#}",
                verify_and_decompress(&gz, line, "agentpit-x.gz").unwrap_err()
            );
            assert!(err.contains("malformed"), "line {line:?} gave: {err}");
        }
    }

    /// Review finding (2026-07-25): a checksum proves the bytes are the ones the release
    /// published, not that they are the version the tag claims — a release cut from the
    /// wrong ref yields a valid checksum over the wrong build. The staged binary has to
    /// say who it is before it replaces a working one.
    #[cfg(unix)]
    #[test]
    fn staged_binary_must_report_the_expected_version() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let fake = |name: &str, body: &str| {
            let path = temp.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };

        let right = fake("right", "echo 'agentpit 0.1.34'");
        assert!(assert_staged_version(&right, "0.1.34").is_ok());

        // A build of some other version, correctly checksummed, must not install.
        let wrong = fake("wrong", "echo 'agentpit 0.1.33'");
        let err = format!("{:#}", assert_staged_version(&wrong, "0.1.34").unwrap_err());
        assert!(err.contains("refusing to install"), "got: {err}");

        // A binary that cannot even report a version is not installable either.
        let broken = fake("broken", "exit 3");
        assert!(assert_staged_version(&broken, "0.1.34").is_err());
        assert!(assert_staged_version(&temp.path().join("absent"), "0.1.34").is_err());
    }

    /// Security-review finding (2026-07-25): the tag flows into download URLs and into
    /// PlistBuddy arguments, and `normalized_version` only trims whitespace and a leading
    /// `v` — a tag containing a slash would reshape the URL path. Only plain semver tags
    /// are accepted.
    #[test]
    fn only_plain_semver_release_tags_are_accepted() {
        for ok in ["v0.1.34", "0.1.34", " v1.20.300 \n"] {
            assert!(is_supported_release_tag(ok), "should accept {ok:?}");
        }
        for bad in [
            "v0.1.34/../../evil",
            "v0.1",
            "v0.1.34-rc1",
            "v0.1.34 extra",
            "latest",
            "",
            "v1.2.3.4",
        ] {
            assert!(!is_supported_release_tag(bad), "should reject {bad:?}");
        }
    }

    /// Security-review finding (2026-07-25): staging used `fs::write`, which follows a
    /// symlink, so a pre-placed symlink at the predictable staging path could redirect the
    /// update's bytes onto another file. `create_new` fails closed instead.
    #[cfg(unix)]
    #[test]
    fn staging_refuses_to_follow_a_pre_placed_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        std::fs::write(&victim, b"important").unwrap();
        let staged = temp.path().join("agentpit.update.1");
        std::os::unix::fs::symlink(&victim, &staged).unwrap();

        assert!(write_new_file(&staged, b"payload").is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"important");

        // A clean path still stages normally.
        let fresh = temp.path().join("agentpit.update.2");
        write_new_file(&fresh, b"payload").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"payload");
    }

    #[test]
    fn gunzip_rejects_a_payload_that_expands_past_the_ceiling() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;

        // A highly compressible payload just over the ceiling: valid gzip, small on the
        // wire, refused on expansion.
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        let chunk = vec![0_u8; 1024 * 1024];
        for _ in 0..=(MAX_DECOMPRESSED_BYTES / chunk.len() as u64) {
            enc.write_all(&chunk).unwrap();
        }
        let bomb = enc.finish().unwrap();
        assert!(
            (bomb.len() as u64) < MAX_DECOMPRESSED_BYTES,
            "the compressed form should be small"
        );
        let err = gunzip(&bomb).unwrap_err();
        assert!(format!("{err}").contains("refusing to install"), "{err}");
    }

    #[test]
    fn gunzip_round_trips() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write as _;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"agentpit binary bytes").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(gunzip(&gz).unwrap(), b"agentpit binary bytes");
        assert!(gunzip(b"definitely not gzip").is_err());
    }

    /// Live network check of the full verified-download path against the real latest
    /// release: asset + .sha256 fetched, digest matches, payload gunzips to a Mach-O/ELF
    /// binary. `#[ignore]` so CI stays offline; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "hits github.com"]
    fn live_fetch_verified_binary_for_latest_release() {
        let latest = fetch_latest_tag().expect("latest tag");
        let tag = format!("v{}", normalized_version(&latest));
        let target = self_update::get_target();
        let asset = format!("{}.gz", asset_identifier(BIN_NAME, target));
        let binary = fetch_verified_binary(&tag, &asset).expect("verified download");
        assert!(binary.len() > 1_000_000, "implausibly small CLI binary");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn update_bundle_version_plist_rewrites_both_version_keys() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("fake.app");
        let contents = bundle.join("Contents");
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleShortVersionString</key>
    <string>0.1.32</string>
    <key>CFBundleVersion</key>
    <string>0.1.32</string>
</dict>
</plist>
"#,
        )
        .unwrap();

        update_bundle_version_plist(&bundle, "v0.1.33").unwrap();

        let read = |key: &str| {
            let out = std::process::Command::new("/usr/libexec/PlistBuddy")
                .arg("-c")
                .arg(format!("Print :{key}"))
                .arg(contents.join("Info.plist"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(read("CFBundleShortVersionString"), "0.1.33");
        assert_eq!(read("CFBundleVersion"), "0.1.33");

        // A bundle without an Info.plist (bare directory) is silently skipped.
        let bare = temp.path().join("bare.app");
        std::fs::create_dir_all(&bare).unwrap();
        update_bundle_version_plist(&bare, "0.1.33").unwrap();
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

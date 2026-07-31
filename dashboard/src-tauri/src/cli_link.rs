//! Put the bundled `agentpit` CLI on the user's PATH, and keep it correct afterwards.
//!
//! The desktop app is the primary distribution and already carries a matching CLI sidecar, but a
//! terminal cannot reach inside the bundle. Installing the standalone CLI as well produces two
//! copies that drift apart silently — the desktop updates itself while `~/.local/bin/agentpit`
//! stays at whatever version it was, which is exactly how a `0.2.5` CLI ends up next to a `0.2.9`
//! app. This module removes the second copy entirely: PATH points AT the bundle.
//!
//! **A shim script, not a symlink.** Both follow the bundle when the app updates, but they differ
//! where it matters. Rust's `std::env::current_exe()` does not resolve a symlink on macOS, so
//! `agentpit update` invoked through a symlinked CLI replaces the *link* with a regular file — the
//! link silently disappears and the drift is back, with no bundle re-signing either. `exec`
//! replaces the process image, so a one-line shim makes `current_exe()` report the real binary
//! inside the bundle: `agentpit update` then updates the sidecar in place and re-signs the bundle,
//! which is the path the updater already handles. With a shim, updating from either side is
//! correct; with a symlink, only one side is.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Marks a shim as ours. Anything on the target path without this line was put there by the user
/// (most likely a standalone CLI install) and is never overwritten without an explicit replace.
const SHIM_MARKER: &str = "# managed by agentpit desktop";

/// Where the shim goes. `~/.local/bin` is the same directory the standalone CLI install docs use,
/// so a user who followed those already has it on PATH.
const SHIM_DIR: &str = ".local/bin";
const SHIM_NAME: &str = "agentpit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    /// Our shim is present and points at this bundle's sidecar.
    Linked,
    /// Our shim is present but points somewhere else — usually a bundle that has since moved.
    Stale,
    /// Something else occupies the path: a standalone CLI, or a hand-made link.
    Foreign,
    /// Nothing is there.
    Absent,
    /// No sidecar to point at (a development build with no sibling CLI).
    Unavailable,
}

/// Serialized SNAKE_CASE to match the neighbouring settings/update payloads the same pane
/// already reads (`AppUpdateInfo`); `cli_versions` uses camelCase for its own view. The wire
/// names are asserted in the tests so the frontend cannot silently drift onto `undefined`.
#[derive(Debug, Clone, Serialize)]
pub struct CliLinkStatus {
    pub state: LinkState,
    /// Where the shim goes / is.
    pub shim_path: String,
    /// The bundled CLI it should point at, when one was found.
    pub sidecar_path: Option<String>,
    /// What currently occupies `shim_path`, for the Foreign/Stale cases.
    pub occupant: Option<String>,
    /// What a login shell resolves `agentpit` to, and its version — the only honest way to answer
    /// "is it on PATH?", since a GUI app's own PATH is not the user's.
    pub resolved_on_path: Option<String>,
    pub resolved_version: Option<String>,
}

/// The CLI shipped alongside the running dashboard: its sibling in the bundle's binary directory.
/// Mirrors `cli_runner::development_cli`'s sibling lookup so both agree on which CLI is "ours".
fn sidecar_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "agentpit.exe"
    } else {
        SHIM_NAME
    };
    let sibling = exe.parent()?.join(name);
    sibling.is_file().then_some(sibling)
}

fn shim_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(SHIM_DIR)
            .join(SHIM_NAME),
    )
}

/// The shim body. `exec` is load-bearing, not stylistic — see the module docs.
fn shim_body(sidecar: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         {SHIM_MARKER} — reinstall from Settings → アプリと更新.\n\
         # Runs the CLI inside the app bundle, so `agentpit update` updates the bundle itself\n\
         # rather than replacing this file.\n\
         exec {} \"$@\"\n",
        shell_quote(&sidecar.to_string_lossy())
    )
}

/// Single-quote for `sh`. The bundle path is not user input, but it can contain spaces, and an
/// unquoted `exec` line would break the shim in a way that only shows up at run time.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The sidecar a shim body points at, when the file is one of ours.
fn shim_target(body: &str) -> Option<PathBuf> {
    if !body.contains(SHIM_MARKER) {
        return None;
    }
    let line = body.lines().find(|l| l.starts_with("exec "))?;
    let rest = line.trim_start_matches("exec ").trim();
    let quoted = rest.strip_suffix(" \"$@\"").unwrap_or(rest);
    let unquoted = quoted
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .map(|s| s.replace(r"'\''", "'"))
        .unwrap_or_else(|| quoted.to_string());
    Some(PathBuf::from(unquoted))
}

/// Classify what is on the shim path right now. Pure over its inputs so every branch is testable
/// without touching a real HOME.
fn classify(shim: &Path, sidecar: Option<&Path>) -> (LinkState, Option<String>) {
    let Some(sidecar) = sidecar else {
        return (LinkState::Unavailable, None);
    };
    // `symlink_metadata` so a dangling symlink is seen as an occupant rather than as absent.
    let Ok(meta) = std::fs::symlink_metadata(shim) else {
        return (LinkState::Absent, None);
    };
    if meta.is_symlink() {
        let target = std::fs::read_link(shim)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "symlink".into());
        return (LinkState::Foreign, Some(target));
    }
    match std::fs::read_to_string(shim)
        .ok()
        .as_deref()
        .and_then(shim_target)
    {
        Some(target) if target == sidecar => {
            (LinkState::Linked, Some(target.display().to_string()))
        }
        Some(target) => (LinkState::Stale, Some(target.display().to_string())),
        None => (
            LinkState::Foreign,
            Some(format!("{} (not managed by the app)", shim.display())),
        ),
    }
}

/// What a LOGIN shell resolves `agentpit` to. A GUI app inherits a minimal PATH that says nothing
/// about the user's terminal, so asking the shell is the only answer worth showing.
fn resolve_via_login_shell() -> Option<(String, Option<String>)> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let out = std::process::Command::new(shell)
        .args(["-lc", "command -v agentpit"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || path.is_empty() {
        return None;
    }
    let version = std::process::Command::new(&path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty());
    Some((path, version))
}

pub fn status() -> CliLinkStatus {
    let sidecar = sidecar_path();
    let shim = shim_path().unwrap_or_else(|| PathBuf::from(SHIM_DIR).join(SHIM_NAME));
    let (state, occupant) = classify(&shim, sidecar.as_deref());
    let (resolved_on_path, resolved_version) = match resolve_via_login_shell() {
        Some((p, v)) => (Some(p), v),
        None => (None, None),
    };
    CliLinkStatus {
        state,
        shim_path: shim.display().to_string(),
        sidecar_path: sidecar.map(|p| p.display().to_string()),
        occupant,
        resolved_on_path,
        resolved_version,
    }
}

/// Write the shim. Refuses to clobber a file the app did not write unless `replace` is set — that
/// file is almost certainly the user's own standalone CLI, and losing it silently is precisely the
/// kind of surprise this feature exists to end.
pub fn install(replace: bool) -> Result<CliLinkStatus, String> {
    let sidecar = sidecar_path()
        .ok_or_else(|| "この起動には同梱CLIが見つかりません（開発ビルドの可能性）".to_string())?;
    let shim = shim_path().ok_or_else(|| "HOME が取得できません".to_string())?;

    let (state, occupant) = classify(&shim, Some(&sidecar));
    if matches!(state, LinkState::Foreign) && !replace {
        return Err(format!(
            "{} には別のファイルがあります（{}）。置き換えると失われます。",
            shim.display(),
            occupant.unwrap_or_else(|| "内容不明".into())
        ));
    }

    let dir = shim
        .parent()
        .ok_or_else(|| "シム配置先の親ディレクトリを解決できません".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;

    // Write beside the target and rename in, so a crash mid-write cannot leave a half-written
    // executable on PATH.
    let staged = shim.with_extension(format!("shim.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    std::fs::write(&staged, shim_body(&sidecar))
        .map_err(|e| format!("{} を書けません: {e}", staged.display()))?;
    if let Err(e) = make_executable(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    std::fs::rename(&staged, &shim).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("{} を設置できません: {e}", shim.display())
    })?;

    Ok(status())
}

/// Remove our shim. Never touches a file the app did not write.
pub fn remove() -> Result<CliLinkStatus, String> {
    let shim = shim_path().ok_or_else(|| "HOME が取得できません".to_string())?;
    let (state, _) = classify(&shim, sidecar_path().as_deref());
    match state {
        LinkState::Linked | LinkState::Stale => std::fs::remove_file(&shim)
            .map_err(|e| format!("{} を削除できません: {e}", shim.display()))?,
        LinkState::Foreign => {
            return Err(format!(
                "{} はアプリが設置したものではないため削除しません。",
                shim.display()
            ));
        }
        LinkState::Absent | LinkState::Unavailable => {}
    }
    Ok(status())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("実行権限を付与できません: {e}"))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub fn cli_link_status() -> CliLinkStatus {
    status()
}

#[tauri::command]
pub fn cli_link_install(replace: bool) -> Result<CliLinkStatus, String> {
    install(replace)
}

#[tauri::command]
pub fn cli_link_remove() -> Result<CliLinkStatus, String> {
    remove()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_execs_the_sidecar_so_current_exe_reports_the_real_binary() {
        let body = shim_body(Path::new(
            "/Applications/agentpit.app/Contents/MacOS/agentpit",
        ));
        // `exec` is the whole point: without it `current_exe()` would report the shim and
        // `agentpit update` would overwrite this file instead of the bundle.
        assert!(
            body.contains("exec '/Applications/agentpit.app/Contents/MacOS/agentpit' \"$@\""),
            "{body}"
        );
        assert!(body.starts_with("#!/bin/sh\n"));
        assert!(body.contains(SHIM_MARKER));
    }

    #[test]
    fn a_path_with_spaces_or_quotes_round_trips_through_the_shim() {
        for raw in [
            "/Applications/agent pit.app/Contents/MacOS/agentpit",
            "/tmp/it's here/agentpit",
        ] {
            let body = shim_body(Path::new(raw));
            assert_eq!(
                shim_target(&body).as_deref(),
                Some(Path::new(raw)),
                "{body}"
            );
        }
    }

    #[test]
    fn a_file_without_our_marker_is_never_recognised_as_ours() {
        // A standalone CLI is a binary, not a script; and even a script that happens to exec the
        // same path is not ours unless we wrote the marker.
        assert!(shim_target("#!/bin/sh\nexec '/somewhere/agentpit' \"$@\"\n").is_none());
        assert!(shim_target("\u{7f}ELF binary bytes").is_none());
    }

    /// The JSX reads these keys directly. A rename here would not fail any Rust test, it would
    /// just render blanks, so the wire shape is pinned.
    #[test]
    fn the_wire_shape_matches_what_the_settings_pane_reads() {
        let json = serde_json::to_value(CliLinkStatus {
            state: LinkState::Linked,
            shim_path: "/home/u/.local/bin/agentpit".into(),
            sidecar_path: Some("/Applications/agentpit.app/Contents/MacOS/agentpit".into()),
            occupant: None,
            resolved_on_path: Some("/home/u/.local/bin/agentpit".into()),
            resolved_version: Some("agentpit 0.2.10".into()),
        })
        .unwrap();
        assert_eq!(json["state"], "linked");
        assert!(json["shim_path"].is_string());
        assert!(json["sidecar_path"].is_string());
        assert!(json["resolved_on_path"].is_string());
        assert!(json["resolved_version"].is_string());
    }

    #[test]
    fn classify_separates_absent_linked_stale_and_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("bundle/agentpit");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"cli").unwrap();
        let shim = dir.path().join("bin/agentpit");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();

        assert_eq!(classify(&shim, Some(&sidecar)).0, LinkState::Absent);
        // No sidecar at all outranks whatever is on the path — there is nothing to point at.
        assert_eq!(classify(&shim, None).0, LinkState::Unavailable);

        std::fs::write(&shim, shim_body(&sidecar)).unwrap();
        assert_eq!(classify(&shim, Some(&sidecar)).0, LinkState::Linked);

        // Our shim, but the bundle moved.
        std::fs::write(&shim, shim_body(Path::new("/moved/agentpit"))).unwrap();
        assert_eq!(classify(&shim, Some(&sidecar)).0, LinkState::Stale);

        // Someone else's file: the user's standalone CLI.
        std::fs::write(&shim, b"a real binary").unwrap();
        assert_eq!(classify(&shim, Some(&sidecar)).0, LinkState::Foreign);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_counts_as_foreign_even_when_it_points_at_our_sidecar() {
        // A hand-made symlink is exactly the arrangement that breaks `agentpit update`, so it
        // must never be reported as correctly linked.
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("agentpit");
        std::fs::write(&sidecar, b"cli").unwrap();
        let shim = dir.path().join("link");
        std::os::unix::fs::symlink(&sidecar, &shim).unwrap();
        assert_eq!(classify(&shim, Some(&sidecar)).0, LinkState::Foreign);
    }
}

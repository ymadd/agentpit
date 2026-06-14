use std::process::Stdio;

use tokio::process::Command;

use super::check::{AuthStatus, check_auth};
use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub launched: bool,
    pub message: String,
}

fn escape_for_applescript(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn launch_in_mac_terminal(command: &str) -> LaunchOutcome {
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        escape_for_applescript(command)
    );
    match Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => LaunchOutcome {
            launched: true,
            message: format!("Opened Terminal.app and ran: {command}"),
        },
        Ok(status) => LaunchOutcome {
            launched: false,
            message: format!(
                "osascript exited with code {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            ),
        },
        Err(err) => LaunchOutcome {
            launched: false,
            message: format!("Failed to open Terminal.app: {err}"),
        },
    }
}

pub async fn launch_login(backend: BackendId) -> (AuthStatus, Option<LaunchOutcome>) {
    let status = check_auth(backend).await;
    launch_terminal_login(status).await
}

pub async fn launch_terminal_login(status: AuthStatus) -> (AuthStatus, Option<LaunchOutcome>) {
    if status.ok {
        return (status, None);
    }
    if status.login_command.is_empty() {
        return (status, None);
    }

    if cfg!(target_os = "macos") {
        let outcome = launch_in_mac_terminal(&status.login_command).await;
        (status, Some(outcome))
    } else {
        let cmd = status.login_command.clone();
        (
            status,
            Some(LaunchOutcome {
                launched: false,
                message: format!("Auto-launch is only supported on macOS. Run manually: {cmd}"),
            }),
        )
    }
}

//! Run the `agentpit` CLI embedded in the desktop bundle.
//!
//! Production bundles register `agentpit` as a Tauri sidecar, which gives every platform the
//! correct bundle-relative executable path. Development builds deliberately keep a PATH/sibling
//! fallback so `cargo run -p agentpit-dashboard` remains useful without first assembling a bundle.

use std::path::PathBuf;

use tauri::AppHandle;
use tauri_plugin_shell::{process::CommandEvent, ShellExt};

#[derive(Debug)]
pub struct CliOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CliOutput {
    pub fn failure_message(&self, what: &str) -> String {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            match self.code {
                Some(code) => format!("{what} exited with code {code}"),
                None => format!("{what} was terminated"),
            }
        } else {
            format!("{what} failed: {message}")
        }
    }
}

/// Resolve the unbundled development CLI: sibling first, then PATH.
fn development_cli() -> PathBuf {
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let name = if cfg!(windows) {
                "agentpit.exe"
            } else {
                "agentpit"
            };
            let sibling = directory.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("agentpit")
}

async fn run_sidecar(
    app: &AppHandle,
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<CliOutput, String> {
    let command = app
        .shell()
        .sidecar("agentpit")
        .map_err(|error| format!("bundled CLI is unavailable: {error}"))?
        .args(args);

    if let Some(input) = stdin {
        let (mut events, mut child) = command
            .set_raw_out(true)
            .spawn()
            .map_err(|error| format!("bundled CLI is unavailable: {error}"))?;
        child
            .write(input)
            .map_err(|error| format!("failed to send input to bundled CLI: {error}"))?;
        // Dropping the writer closes stdin so commands that read to EOF can continue.
        drop(child);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut code = None;
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => stdout.extend(bytes),
                CommandEvent::Stderr(bytes) => stderr.extend(bytes),
                CommandEvent::Error(error) => {
                    if !stderr.is_empty() {
                        stderr.push(b'\n');
                    }
                    stderr.extend(error.as_bytes());
                }
                CommandEvent::Terminated(payload) => code = payload.code,
                _ => {}
            }
        }
        Ok(CliOutput {
            success: code == Some(0),
            code,
            stdout,
            stderr,
        })
    } else {
        let output = command
            .output()
            .await
            .map_err(|error| format!("bundled CLI is unavailable: {error}"))?;
        Ok(CliOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

async fn run_development_fallback(
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<CliOutput, String> {
    use tokio::io::AsyncWriteExt;

    let binary = development_cli();
    let mut command = tokio::process::Command::new(&binary);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if stdin.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to launch {}: {error}", binary.display()))?;
    if let Some(input) = stdin {
        let mut writer = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open the CLI stdin".to_string())?;
        writer
            .write_all(input)
            .await
            .map_err(|error| format!("failed to send input to {}: {error}", binary.display()))?;
        drop(writer);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("failed to wait for {}: {error}", binary.display()))?;
    Ok(CliOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Prefer the bundled CLI, with a sibling/PATH fallback for unbundled development builds.
pub async fn run(
    app: &AppHandle,
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<CliOutput, String> {
    match run_sidecar(app, args, stdin).await {
        Ok(output) => Ok(output),
        Err(_) => run_development_fallback(args, stdin).await,
    }
}

/// Require the bundled CLI. App self-update uses this to avoid mutating an unrelated `agentpit`
/// found on PATH when running an unbundled development build.
pub async fn run_bundled(
    app: &AppHandle,
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<CliOutput, String> {
    run_sidecar(app, args, stdin).await
}

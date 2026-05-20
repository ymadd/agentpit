use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin_input: Option<String>,
}

pub struct ExecRunOptions {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub on_stdout: Option<Arc<dyn Fn(&str) + Send + Sync + 'static>>,
}

pub struct ExecOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
}

pub async fn run_spec(
    id: BackendId,
    spec: ExecSpec,
    options: ExecRunOptions,
) -> Result<ExecOutcome> {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .current_dir(&options.cwd)
        .stdin(if spec.stdin_input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn {}: {e}", spec.command))?;

    if let Some(input) = spec.stdin_input.as_ref() {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).await?;
            stdin.shutdown().await.ok();
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{} stdout not available", id))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("{} stderr not available", id))?;

    let on_stdout = options.on_stdout.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut collected = String::new();
        let mut buf = Vec::with_capacity(1024);
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                break;
            }
            let chunk = String::from_utf8_lossy(&buf).into_owned();
            if let Some(cb) = &on_stdout {
                cb(&chunk);
            }
            collected.push_str(&chunk);
        }
        Ok::<String, std::io::Error>(collected)
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut collected = String::new();
        let mut buf = Vec::with_capacity(1024);
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                break;
            }
            collected.push_str(&String::from_utf8_lossy(&buf));
        }
        Ok::<String, std::io::Error>(collected)
    });

    let cancel = options.cancel.clone();
    let exit_status = tokio::select! {
        status = child.wait() => status?,
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            child.wait().await?
        }
    };

    let stdout_text = stdout_task.await??;
    let stderr_text = stderr_task.await??;

    let code = exit_status.code();
    if !exit_status.success() {
        let detail = if stderr_text.trim().is_empty() {
            String::new()
        } else {
            format!("\nstderr: {}", stderr_text.trim())
        };
        return Err(anyhow!(
            "{} exited with code {}{detail}",
            id,
            code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        ));
    }

    Ok(ExecOutcome {
        output: stdout_text,
        exit_code: code,
    })
}

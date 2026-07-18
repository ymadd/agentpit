use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::types::BackendId;

use super::stream::{DecodedChunk, StreamDecoder, StreamFormat};

/// Hard cap on how much stdout/stderr we retain in memory per backend run. A wedged or
/// runaway backend can emit unbounded output; we keep streaming chunks to the caller
/// (the dashboard does its own windowing) but stop growing the collected buffer past this
/// so one run can't exhaust hub memory. ~8 MiB is far above any real agent transcript.
const MAX_CAPTURED_BYTES: usize = 8 * 1024 * 1024;

/// Callback invoked with each streamed stdout chunk (e.g. tee to terminal + capture file).
pub type OutputSink = Arc<dyn Fn(&str) + Send + Sync + 'static>;

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
    pub on_stdout: Option<OutputSink>,
    /// Optional model to pin for this run. `None` = the backend CLI's own default (no `--model`
    /// flag emitted — byte-identical to the pre-model behaviour). Threaded to `build_spec`.
    pub model: Option<String>,
}

pub struct ExecOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
}

pub async fn run_spec(
    id: BackendId,
    spec: ExecSpec,
    options: ExecRunOptions,
    stream_format: StreamFormat,
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

    // Worker isolation for the human back-channel: a child spawned here inherits the parent's
    // full environment (we deliberately never `env_clear`). The workflow manager leg sets
    // AGENTPIT_ASK_ALLOWED so it alone may reach the human; strip it here so every backend we
    // spawn — which would otherwise inherit it — cannot. Only a spec that explicitly re-adds it
    // below (the manager leg) keeps it.
    cmd.env_remove(crate::ask::ENV_ASK_ALLOWED);

    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to spawn {}: {e}", spec.command))?;

    if let Some(input) = spec.stdin_input.as_ref()
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await.ok();
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
        let mut collected = String::new();
        let mut truncated = false;
        let mut decoder = StreamDecoder::new(stream_format);

        if stream_format == StreamFormat::Text {
            // Human-readable CLIs do not necessarily flush on newline boundaries. Read arbitrary
            // bytes so Antigravity's `--print` output can appear as soon as the process writes it,
            // while retaining an incomplete UTF-8 sequence until the next read.
            let mut stdout = stdout;
            let mut read_buf = [0_u8; 4096];
            let mut pending = Vec::new();
            loop {
                let n = stdout.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&read_buf[..n]);
                drain_valid_utf8(
                    &mut pending,
                    false,
                    &on_stdout,
                    &mut collected,
                    &mut truncated,
                );
            }
            drain_valid_utf8(
                &mut pending,
                true,
                &on_stdout,
                &mut collected,
                &mut truncated,
            );
        } else {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::with_capacity(1024);
            loop {
                buf.clear();
                let n = reader.read_until(b'\n', &mut buf).await?;
                if n == 0 {
                    break;
                }
                let chunk = String::from_utf8_lossy(&buf).into_owned();
                consume_decoded(
                    decoder.decode_line(&chunk),
                    &on_stdout,
                    &mut collected,
                    &mut truncated,
                );
            }
            consume_decoded(decoder.finish(), &on_stdout, &mut collected, &mut truncated);
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
            if collected.len() < MAX_CAPTURED_BYTES {
                collected.push_str(&String::from_utf8_lossy(&buf));
            }
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
        let mut detail = String::new();
        if !stdout_text.trim().is_empty() {
            detail.push_str(&format!("\nstdout: {}", stdout_text.trim()));
        }
        if !stderr_text.trim().is_empty() {
            detail.push_str(&format!("\nstderr: {}", stderr_text.trim()));
        }
        return Err(anyhow!(
            "{} exited with code {}{detail}",
            id,
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ));
    }

    Ok(ExecOutcome {
        output: stdout_text,
        exit_code: code,
    })
}

fn drain_valid_utf8(
    pending: &mut Vec<u8>,
    eof: bool,
    on_stdout: &Option<OutputSink>,
    collected: &mut String,
    truncated: &mut bool,
) {
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                if !text.is_empty() {
                    consume_decoded(
                        DecodedChunk {
                            display: Some(text.to_string()),
                            answer: Some(text.to_string()),
                        },
                        on_stdout,
                        collected,
                        truncated,
                    );
                }
                pending.clear();
                return;
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid = error.valid_up_to();
                let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                pending.drain(..valid);
                consume_decoded(
                    DecodedChunk {
                        display: Some(text.clone()),
                        answer: Some(text),
                    },
                    on_stdout,
                    collected,
                    truncated,
                );
            }
            Err(error) if error.error_len().is_some() || eof => {
                let invalid_len = error.error_len().unwrap_or(pending.len()).max(1);
                let end = invalid_len.min(pending.len());
                let text = String::from_utf8_lossy(&pending[..end]).into_owned();
                pending.drain(..end);
                consume_decoded(
                    DecodedChunk {
                        display: Some(text.clone()),
                        answer: Some(text),
                    },
                    on_stdout,
                    collected,
                    truncated,
                );
            }
            Err(_) => return,
        }
    }
}

fn consume_decoded(
    chunk: DecodedChunk,
    on_stdout: &Option<OutputSink>,
    collected: &mut String,
    truncated: &mut bool,
) {
    if let Some(display) = chunk.display
        && let Some(cb) = on_stdout
    {
        cb(&display);
    }

    let Some(answer) = chunk.answer else {
        return;
    };
    if collected.len() < MAX_CAPTURED_BYTES {
        let remaining = MAX_CAPTURED_BYTES - collected.len();
        if answer.len() <= remaining {
            collected.push_str(&answer);
        } else {
            let mut end = remaining;
            while end > 0 && !answer.is_char_boundary(end) {
                end -= 1;
            }
            collected.push_str(&answer[..end]);
            *truncated = true;
            collected.push_str("\n[output truncated: exceeded capture limit]\n");
        }
    } else if !*truncated {
        *truncated = true;
        collected.push_str("\n[output truncated: exceeded capture limit]\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_ask_allowed_spec(env: Vec<(String, String)>) -> ExecSpec {
        ExecSpec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s' \"${AGENTPIT_ASK_ALLOWED:-ABSENT}\"".into(),
            ],
            env,
            stdin_input: None,
        }
    }

    async fn run_echo(spec: ExecSpec) -> String {
        run_spec(
            BackendId::Gemini,
            spec,
            ExecRunOptions {
                cwd: std::env::current_dir().unwrap(),
                cancel: CancellationToken::new(),
                on_stdout: None,
                model: None,
            },
            StreamFormat::Text,
        )
        .await
        .unwrap()
        .output
        .trim()
        .to_string()
    }

    #[test]
    fn raw_text_stream_holds_incomplete_utf8_until_the_next_chunk() {
        let mut pending = vec![0xe3, 0x81];
        let mut collected = String::new();
        let mut truncated = false;
        drain_valid_utf8(&mut pending, false, &None, &mut collected, &mut truncated);
        assert!(collected.is_empty());
        assert_eq!(pending, vec![0xe3, 0x81]);

        pending.push(0x82);
        drain_valid_utf8(&mut pending, false, &None, &mut collected, &mut truncated);
        assert_eq!(collected, "あ");
        assert!(pending.is_empty());
        assert!(!truncated);
    }

    // R3 worker-isolation fix: the parent (this test process, standing in for the manager) has
    // AGENTPIT_ASK_ALLOWED set, but a spawned child must NOT inherit it unless its spec re-adds it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ask_allow_token_is_stripped_from_children_unless_respecced() {
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized under STATE_ENV_LOCK.
        unsafe {
            std::env::set_var(crate::ask::ENV_ASK_ALLOWED, "run-7");
        }

        // A worker spec (no token in env) must see the inherited token stripped.
        let worker = run_echo(echo_ask_allowed_spec(vec![])).await;
        assert_eq!(worker, "ABSENT", "worker must not inherit the allow token");

        // The manager leg re-adds the token in its spec env, so it survives the strip.
        let manager = run_echo(echo_ask_allowed_spec(vec![(
            crate::ask::ENV_ASK_ALLOWED.to_string(),
            "run-7".to_string(),
        )]))
        .await;
        assert_eq!(manager, "run-7", "manager leg keeps the re-added token");

        unsafe {
            std::env::remove_var(crate::ask::ENV_ASK_ALLOWED);
        }
    }
}

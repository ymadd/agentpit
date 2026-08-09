//! `agentpit attach [id]` — the daemon-backed interactive client (design §5.3, B2/B7).
//!
//! Connects to a session's worker (spawning daemon/worker as needed), renders the
//! transcript tail, then loops: read a line → `send` → stream chunks live. Exiting
//! (Ctrl-D, /quit, closing the terminal) is a pure detach — the worker and any in-flight
//! turn keep running, and `agentpit attach <id>` comes back to it.

use anyhow::Result;
use console::style;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::daemon::client::{create_session, open_session};
use crate::daemon::protocol::{Event, Frame, RequestBody, ResponseData};

pub async fn run(session: Option<String>) -> Result<()> {
    let cwd = crate::cli::resolve_cwd(None)?;
    let (session_id, mut conn) = match &session {
        Some(id) => open_session(id, true).await?,
        None => create_session(&cwd, true).await?,
    };
    let short = &session_id[session_id.len().saturating_sub(12)..];

    // Attach: subscribe + transcript tail.
    let tail = crate::config::load_config(None)
        .map(|l| l.config.session.transcript_tail)
        .unwrap_or(400);
    let data = conn.request(RequestBody::Attach { tail }).await?;
    if let ResponseData::Snapshot {
        transcript,
        total_entries,
        shown,
        ..
    } = data
    {
        for (who, text) in &transcript {
            match who.as_str() {
                "user" => println!("{} {text}", style("you:").green().bold()),
                "summary" => println!("{}\n{text}", style("── summary ──").magenta()),
                backend => println!("{} {text}", style(format!("{backend}:")).cyan().bold()),
            }
        }
        if total_entries > shown {
            println!(
                "{}",
                style(format!(
                    "Showing latest {shown} of {total_entries} messages for faster open \
                     (`agentpit sessions show {short}` prints everything)."
                ))
                .dim()
            );
        }
    }
    eprintln!(
        "{}",
        style(format!(
            "[attached to {short} — detach with Ctrl-D or /quit; the session keeps running]"
        ))
        .dim()
    );

    let mut editor = DefaultEditor::new()?;
    loop {
        let line = tokio::task::block_in_place(|| editor.readline("> "));
        let line = match line {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);
        match trimmed {
            "/quit" | "/exit" | "/detach" => break,
            "/tree" => {
                if let Ok(ResponseData::Lines { lines }) = conn.request(RequestBody::Tree).await {
                    for l in lines {
                        println!("{l}");
                    }
                }
                continue;
            }
            _ if trimmed.starts_with('/') => {
                eprintln!(
                    "{}",
                    style(
                        "attach supports free text, /tree, and /quit. \
                         Use `agentpit repl --resume` for the full command set."
                    )
                    .yellow()
                );
                continue;
            }
            _ => {}
        }

        // Send and stream: events render live until this request's response arrives.
        let req_id = conn
            .send_request(RequestBody::Send {
                text: trimmed.to_string(),
                backend: None,
            })
            .await?;
        loop {
            match conn.recv_frame().await {
                Err(e) => {
                    eprintln!(
                        "{} connection lost ({e:#}). Re-attach with `agentpit attach {short}` — \
                         the turn keeps running in the worker.",
                        style("error:").red()
                    );
                    return Ok(());
                }
                Ok(Frame::Event(Event::TurnStarted { backend })) => {
                    eprintln!("{}", style(format!("[→ {backend}]")).dim());
                }
                Ok(Frame::Event(Event::Chunk { text })) => {
                    print!("{text}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
                Ok(Frame::Event(Event::TurnFinished { .. })) => {}
                Ok(Frame::Event(Event::Notice { text })) => {
                    eprintln!("{} {text}", style("session:").yellow());
                }
                Ok(Frame::Response(resp)) if resp.id == req_id => {
                    if !resp.ok {
                        eprintln!(
                            "{} {}",
                            style("error:").red(),
                            resp.error.unwrap_or_default()
                        );
                    } else if let Some(ResponseData::Turn { status, .. }) = resp.data
                        && status != "ok"
                    {
                        eprintln!("{}", style(format!("[turn ended: {status}]")).yellow());
                    } else {
                        println!();
                    }
                    break;
                }
                Ok(Frame::Response(_)) => {}
            }
        }
    }

    let _ = conn.request(RequestBody::Detach).await;
    eprintln!(
        "{}",
        style(format!(
            "[detached — {}]",
            crate::cli::guidance::detach_hint(short)
        ))
        .dim()
    );
    Ok(())
}

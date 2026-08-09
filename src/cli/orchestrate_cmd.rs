//! `agentpit orchestrate` — drive a session's orchestration REPL (design §10).
//!
//! Interactive: each line is one TypeScript cell evaluated in the session worker's deno
//! sidecar. One-shot (`--cell`): evaluate a single cell and exit — the workflow manager's
//! entry point (§10.9 R3): heap state persists in the WORKER across invocations, so a
//! manager shelling out repeatedly still accumulates `S`/`store` state.

use anyhow::Result;
use console::style;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::daemon::client::{create_session, open_session};
use crate::daemon::protocol::{Event, Frame, RequestBody, ResponseData};

pub async fn run(session: Option<String>, cell: Option<String>) -> Result<()> {
    let cwd = crate::cli::resolve_cwd(None)?;
    let (session_id, mut conn) = match &session {
        Some(id) => open_session(id, true).await?,
        None => create_session(&cwd, true).await?,
    };
    let short = crate::cli::guidance::short_id(&session_id);

    if let Some(code) = cell {
        // One-shot: result on stdout, diagnostics on stderr, exit code reflects success.
        // The session line lets a shelled-out manager capture the id for later cells.
        eprintln!("session: {short}");
        let ok = eval_and_render(&mut conn, &code).await?;
        if !ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    eprintln!(
        "{}",
        style(format!(
            "[orchestration REPL on session {short} — one TypeScript cell per line]\n\
             persist across cells via S (S.x = …); end a cell with `return <expr>` to see it;\n\
             dispatch()/store/session are the only exits. /quit leaves (the worker keeps state)."
        ))
        .dim()
    );
    let mut editor = DefaultEditor::new()?;
    loop {
        let line = tokio::task::block_in_place(|| editor.readline("ts> "));
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
        if matches!(trimmed, "/quit" | "/exit") {
            break;
        }
        let _ = eval_and_render(&mut conn, trimmed).await?;
    }
    eprintln!(
        "{}",
        style(format!(
            "[left the REPL — {}]",
            crate::cli::guidance::detach_hint(short)
        ))
        .dim()
    );
    Ok(())
}

/// Send one cell, stream dispatch chunks live, render the result. Returns cell success.
async fn eval_and_render(conn: &mut crate::daemon::client::Conn, code: &str) -> Result<bool> {
    let req_id = conn
        .send_request(RequestBody::ReplCell {
            code: code.to_string(),
        })
        .await?;
    loop {
        match conn.recv_frame().await? {
            Frame::Event(Event::Chunk { text }) => {
                use std::io::Write;
                eprint!("{}", style(text).dim());
                let _ = std::io::stderr().flush();
            }
            Frame::Event(_) => {}
            Frame::Response(resp) if resp.id == req_id => {
                if !resp.ok {
                    eprintln!(
                        "{} {}",
                        style("error:").red(),
                        resp.error.unwrap_or_default()
                    );
                    return Ok(false);
                }
                match resp.data {
                    Some(ResponseData::Cell { ok: true, repr, .. }) => {
                        println!("{repr}");
                        return Ok(true);
                    }
                    Some(ResponseData::Cell {
                        ok: false,
                        error,
                        check_failed,
                        ..
                    }) => {
                        let label = if check_failed {
                            style("type error (cell not run):").yellow()
                        } else {
                            style("runtime error:").red()
                        };
                        eprintln!("{label} {}", error.unwrap_or_default());
                        return Ok(false);
                    }
                    other => {
                        eprintln!("{} unexpected response: {other:?}", style("error:").red());
                        return Ok(false);
                    }
                }
            }
            Frame::Response(_) => {}
        }
    }
}

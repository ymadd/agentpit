//! The loop commands the webview invokes (design §11), and the bridge operations behind
//! them. Every command returns `Result<_, BridgeError>` so the webview can branch on
//! `code`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use agentpit_events::loops::{
    is_valid_loop_id, is_valid_op_id, loop_dir_in, new_op_id, read_page, step_file_rel, LoopOp,
    LoopView, OpRequest, OpResult, Origin, Surface, READ_DEFAULT_BYTES, READ_MAX_BYTES,
};
use agentpit_events::wire::{BlueprintSource, LoopFile, RequestBody, ResponseData};
use serde::{Deserialize, Serialize};
use tauri::State;

use super::board::{BoardSnapshot, DaemonStatus};
use super::view::read_disk;
use super::{lock, Bridge, BridgeError};

/// Tauri-managed handle on the bridge.
pub struct BridgeState(pub Arc<Bridge>);

/// `loop_start`'s argument (snake_case, like the wire).
#[derive(Debug, Clone, Deserialize)]
pub struct StartRequest {
    pub blueprint: BlueprintSource,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// The directory the loop works in (absolute).
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Start at once (otherwise the loop waits in `created` for a `start` op).
    #[serde(default = "yes")]
    pub start: bool,
    /// Makes a retried start idempotent: the same op id never creates a second loop.
    #[serde(default)]
    pub op_id: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StartedLoop {
    pub loop_id: String,
    /// This op id had already created the loop.
    pub duplicate: bool,
}

/// A page of a step file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepOutput {
    pub offset: u64,
    pub next_offset: u64,
    pub size: u64,
    pub text: String,
}

fn check_loop_id(loop_id: &str) -> Result<(), BridgeError> {
    if is_valid_loop_id(loop_id) {
        Ok(())
    } else {
        Err(BridgeError::bad_request(format!(
            "{loop_id:?} is not a loop id"
        )))
    }
}

impl Bridge {
    pub async fn start_loop(&self, req: StartRequest) -> Result<StartedLoop, BridgeError> {
        let cwd = Path::new(&req.cwd);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(BridgeError::bad_request(format!(
                "the working directory must be an existing absolute directory, got {:?}",
                req.cwd
            )));
        }
        let op_id = match req.op_id {
            Some(id) if is_valid_op_id(&id) => id,
            Some(id) => return Err(BridgeError::bad_request(format!("{id:?} is not an op id"))),
            None => new_op_id(),
        };
        let mut daemon = self.daemon(true).await?;
        match daemon
            .request(RequestBody::LoopStart {
                op_id,
                blueprint: req.blueprint,
                inputs: req.inputs,
                cwd: req.cwd,
                title: req.title.filter(|t| !t.trim().is_empty()),
                start: req.start,
                origin: Some(Origin {
                    surface: Surface::Dashboard,
                    client: Some(self.client.clone()),
                    session_id: None,
                }),
            })
            .await?
        {
            ResponseData::LoopStarted {
                loop_id, duplicate, ..
            } => Ok(StartedLoop { loop_id, duplicate }),
            other => Err(BridgeError::protocol(format!(
                "unexpected answer to loop_start: {other:?}"
            ))),
        }
    }

    /// One operation on a loop, through its runner (woken if parked). An open view of
    /// the loop attaches to the same runner right away.
    pub async fn control(
        &self,
        loop_id: &str,
        op: LoopOp,
        op_id: Option<String>,
        expect_seq: Option<u64>,
    ) -> Result<OpResult, BridgeError> {
        check_loop_id(loop_id)?;
        if matches!(op, LoopOp::Unknown) {
            return Err(BridgeError::bad_request("unknown operation"));
        }
        let op_id = match op_id {
            Some(id) if is_valid_op_id(&id) => id,
            Some(id) => return Err(BridgeError::bad_request(format!("{id:?} is not an op id"))),
            None => new_op_id(),
        };
        let socket = self.ensure_runner(loop_id).await?;
        self.kick_view_runner(loop_id, socket.clone());
        let mut conn = self.runner(&socket).await?;
        match conn
            .request(RequestBody::LoopOp(OpRequest {
                loop_id: loop_id.to_string(),
                op_id,
                expect_seq,
                op,
            }))
            .await?
        {
            ResponseData::OpResult(result) => Ok(result),
            other => Err(BridgeError::protocol(format!(
                "unexpected answer to loop_op: {other:?}"
            ))),
        }
    }

    /// A page of one step's file, read from disk (no runner needed, so reading a parked
    /// or finished loop's output never wakes it). Without `offset`, the last `max_bytes`.
    pub async fn step_output(
        &self,
        loop_id: &str,
        step_id: &str,
        what: LoopFile,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> Result<StepOutput, BridgeError> {
        let dir = loop_dir_in(&self.paths.loops_root, loop_id)
            .ok_or_else(|| BridgeError::bad_request(format!("{loop_id:?} is not a loop id")))?;
        let rel = step_file_rel(what, step_id)
            .ok_or_else(|| BridgeError::bad_request("unknown file kind or step id"))?;
        // Like the runner's loop_read: only steps of this loop.
        let known = match self.open_state(loop_id) {
            Some(state) => lock(&state).step(step_id).is_some(),
            None => read_disk(&dir).await?.0.step(step_id).is_some(),
        };
        if !known {
            return Err(BridgeError::new(
                "not_found",
                format!("{step_id} is not a step of this loop"),
            ));
        }
        let max = max_bytes
            .unwrap_or(READ_DEFAULT_BYTES)
            .clamp(1, READ_MAX_BYTES);
        let path = dir.join(rel);
        let page = tokio::task::spawn_blocking(move || {
            let offset = match offset {
                Some(o) => o,
                None => std::fs::metadata(&path)
                    .map(|m| m.len().saturating_sub(max))
                    .unwrap_or(0),
            };
            read_page(&path, offset, max)
        })
        .await
        .map_err(|e| BridgeError::internal(e.to_string()))?
        .map_err(|e| BridgeError::new("io", format!("read the step file: {e}")))?;
        Ok(StepOutput {
            offset: page.offset,
            next_offset: page.next_offset,
            size: page.size,
            text: page.text,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Tauri commands.

#[tauri::command]
pub async fn loops_board(bridge: State<'_, BridgeState>) -> Result<BoardSnapshot, BridgeError> {
    Ok(bridge.0.board_snapshot())
}

#[tauri::command]
pub async fn daemon_status(bridge: State<'_, BridgeState>) -> Result<DaemonStatus, BridgeError> {
    Ok(bridge.0.daemon_status())
}

#[tauri::command]
pub async fn loop_open(
    bridge: State<'_, BridgeState>,
    loop_id: String,
) -> Result<LoopView, BridgeError> {
    bridge.0.open_loop(&loop_id).await
}

#[tauri::command]
pub async fn loop_close(
    bridge: State<'_, BridgeState>,
    loop_id: String,
) -> Result<(), BridgeError> {
    bridge.0.close_loop(&loop_id);
    Ok(())
}

#[tauri::command]
pub async fn loop_start(
    bridge: State<'_, BridgeState>,
    request: StartRequest,
) -> Result<StartedLoop, BridgeError> {
    bridge.0.start_loop(request).await
}

#[tauri::command]
pub async fn loop_control(
    bridge: State<'_, BridgeState>,
    loop_id: String,
    op: LoopOp,
    op_id: Option<String>,
    expect_seq: Option<u64>,
) -> Result<OpResult, BridgeError> {
    bridge.0.control(&loop_id, op, op_id, expect_seq).await
}

#[tauri::command]
pub async fn gate_resolve(
    bridge: State<'_, BridgeState>,
    loop_id: String,
    gate_id: String,
    option: String,
    comment: Option<String>,
    op_id: Option<String>,
) -> Result<OpResult, BridgeError> {
    let op = LoopOp::ResolveGate {
        gate_id,
        option,
        comment: comment.filter(|c| !c.trim().is_empty()),
    };
    bridge.0.control(&loop_id, op, op_id, None).await
}

#[tauri::command]
pub async fn step_output(
    bridge: State<'_, BridgeState>,
    loop_id: String,
    step_id: String,
    what: LoopFile,
    offset: Option<u64>,
    max_bytes: Option<u64>,
) -> Result<StepOutput, BridgeError> {
    bridge
        .0
        .step_output(&loop_id, &step_id, what, offset, max_bytes)
        .await
}

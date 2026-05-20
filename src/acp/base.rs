use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use anyhow::{Result, anyhow};
use tokio_util::sync::CancellationToken;

use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub command_line: String,
}

pub struct AcpOutcome {
    pub output: String,
}

pub async fn run_acp_prompt(
    id: BackendId,
    spec: SpawnSpec,
    task: &str,
    cwd: &Path,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: CancellationToken,
) -> Result<AcpOutcome> {
    let agent = AcpAgent::from_str(&spec.command_line)
        .map_err(|e| anyhow!("failed to parse spawn command for {id}: {e}"))?;

    let collected: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let collected_for_notif = collected.clone();
    let on_chunk_for_notif = on_chunk.clone();

    let cwd_buf: PathBuf = cwd.to_path_buf();
    let task_owned = task.to_string();

    let task_future = async move {
        agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
                        if let ContentBlock::Text(TextContent { text, .. }) = chunk.content {
                            on_chunk_for_notif(&text);
                            if let Ok(mut buf) = collected_for_notif.lock() {
                                buf.push_str(&text);
                            }
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    let option_id = request.options.first().map(|opt| opt.option_id.clone());
                    if let Some(id) = option_id {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                        ))
                    } else {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = connection
                    .send_request(NewSessionRequest::new(cwd_buf))
                    .block_task()
                    .await?;

                let session_id = new_session.session_id;

                connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(task_owned))],
                    ))
                    .block_task()
                    .await?;

                Ok::<(), agent_client_protocol::Error>(())
            })
            .await
    };

    tokio::select! {
        result = task_future => {
            result.map_err(|e| anyhow!("ACP backend {id} failed: {e}"))?;
        }
        _ = cancel.cancelled() => {
            return Err(anyhow!("ACP backend {id} cancelled"));
        }
    }

    let output = collected
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();

    Ok(AcpOutcome { output })
}

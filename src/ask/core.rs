//! The shared ask-core: post a question to the supervising human and block for an answer.
//!
//! ONE [`ask`] fn backs both the MCP `ask_human` tool and the `agentpit ask` CLI twin — the
//! same single-source-of-truth shape as [`crate::cli::workflow::run_capture`]. The mailbox is
//! two files under [`asks_dir`]: a request sidecar we write (`<ask_id>.json`) and a response
//! sidecar the dashboard writes (`<ask_id>.response.json`) which we poll.
//!
//! NOTHING here writes to stdout: when reached via the MCP tool we are running inside
//! `agentpit mcp serve`, whose stdout carries the JSON-RPC framing — a stray write corrupts it.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::events::{
    RunLogger, ask_request_path, ask_response_path, asks_dir, next_ask_token, now_ms,
};
use crate::workflow::guard::ENV_PARENT_RUN_ID;

/// Returned to the manager when an ask is not answered before its timeout (or could not be
/// posted at all). This is NOT an error — the manager is instructed to proceed with the safe,
/// conservative choice and note it, so an absent human never deadlocks the swarm.
pub const HUMAN_UNAVAILABLE: &str = "HUMAN_UNAVAILABLE";

/// Default time a manager waits on a human before giving up. Kept modest so several asks still
/// fit comfortably under the manager's own dispatch timeout even if the human is away.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;
/// Hard ceiling on any requested timeout, deliberately far below the manager's dispatch timeout
/// (`DEFAULT_DISPATCH_TIMEOUT_SECS` = 1800) so a single ask can never get the manager run killed.
pub const MAX_TIMEOUT_SECS: u64 = 600;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Urgency lane of an ask. `Blocking` means a worker is stalled until answered (it sorts to the
/// top of the inbox and fires a notification); `Review` means nothing is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    Blocking,
    Review,
}

impl AskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AskKind::Blocking => "blocking",
            AskKind::Review => "review",
        }
    }

    /// Parse a caller-supplied kind, defaulting to `Review` for anything unrecognized — an
    /// unknown kind must never silently escalate into a blocking alert.
    pub fn parse_or_default(s: Option<&str>) -> Self {
        match s.map(|x| x.trim().to_ascii_lowercase()).as_deref() {
            Some("blocking") => AskKind::Blocking,
            _ => AskKind::Review,
        }
    }
}

/// A request to put a question to the supervising human.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub prompt: String,
    pub options: Vec<String>,
    pub kind: AskKind,
    /// Requested timeout in seconds; `0` means "use [`DEFAULT_TIMEOUT_SECS`]". Always clamped
    /// to `1..=MAX_TIMEOUT_SECS`.
    pub timeout_secs: u64,
}

/// The result of an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    /// The human answered with this string (one of the options, or free text).
    Answered(String),
    /// No answer before the timeout, the ask was cancelled, or it could not be posted. The
    /// caller surfaces this to the manager as the [`HUMAN_UNAVAILABLE`] sentinel.
    Unavailable,
}

/// The request sidecar the dashboard reads to render an inbox card.
#[derive(Debug, Serialize, Deserialize)]
struct AskRecord {
    ask_id: String,
    run_id: String,
    ts: u64,
    prompt: String,
    options: Vec<String>,
    kind: String,
    timeout_secs: u64,
    pid: u32,
}

/// The response sidecar the dashboard writes when the human answers. Only `answer` is needed
/// here; `ask_id`/`ts` are accepted for round-tripping but optional.
#[derive(Debug, Serialize, Deserialize)]
struct AskResponse {
    #[serde(default)]
    ask_id: String,
    answer: String,
    #[serde(default)]
    ts: u64,
}

/// Put a question to the human and block until answered, timed out, or cancelled.
pub async fn ask(req: AskRequest, cancel: CancellationToken) -> AskOutcome {
    // Correlate with the manager's run when present (set on the manager leg's env); otherwise
    // emit under a synthetic id. The dashboard reads the `asks/` files, not these events, so a
    // standalone run_id is only an audit breadcrumb.
    let run_id = std::env::var(ENV_PARENT_RUN_ID)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(next_ask_token);
    let logger = RunLogger::adopt(run_id.clone());

    let ask_id = next_ask_token();
    let timeout = if req.timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        req.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)
    };
    let pid = std::process::id();

    let record = AskRecord {
        ask_id: ask_id.clone(),
        run_id,
        ts: now_ms(),
        prompt: req.prompt.clone(),
        options: req.options.clone(),
        kind: req.kind.as_str().to_string(),
        timeout_secs: timeout,
        pid,
    };

    // Atomic publish (temp + rename) so the dashboard never reads a half-written request. If we
    // can't post it at all, the human could never see it → Unavailable, uniform with a timeout.
    if write_request(&ask_id, &record).is_err() {
        logger.ask_answered(&ask_id, None, true);
        return AskOutcome::Unavailable;
    }
    logger.ask(
        &ask_id,
        &req.prompt,
        req.kind.as_str(),
        req.options.len(),
        timeout,
        pid,
    );

    let outcome = poll_for_answer(&ask_id, timeout, &cancel).await;

    let timed_out = matches!(outcome, AskOutcome::Unavailable);
    let answer = match &outcome {
        AskOutcome::Answered(a) => Some(a.as_str()),
        AskOutcome::Unavailable => None,
    };
    logger.ask_answered(&ask_id, answer, timed_out);

    cleanup(&ask_id);
    outcome
}

/// Poll the response sidecar until it appears, the timeout elapses, or the token is cancelled.
async fn poll_for_answer(
    ask_id: &str,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> AskOutcome {
    let Some(resp_path) = ask_response_path(ask_id) else {
        return AskOutcome::Unavailable; // unreachable: ask_id is always a safe token
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if cancel.is_cancelled() {
            return AskOutcome::Unavailable;
        }
        // A present-but-unparsable response (the dashboard's rename should preclude it) simply
        // fails the let-chain and we keep polling.
        if let Ok(bytes) = std::fs::read(&resp_path)
            && let Ok(resp) = serde_json::from_slice::<AskResponse>(&bytes)
        {
            return AskOutcome::Answered(resp.answer);
        }
        if Instant::now() >= deadline {
            return AskOutcome::Unavailable;
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = cancel.cancelled() => return AskOutcome::Unavailable,
        }
    }
}

/// Write the request sidecar atomically: temp file in the same dir, then rename into place.
fn write_request(ask_id: &str, record: &AskRecord) -> std::io::Result<()> {
    let dir = asks_dir();
    std::fs::create_dir_all(&dir)?;
    let final_path =
        ask_request_path(ask_id).ok_or_else(|| std::io::Error::other("unsafe ask id"))?;
    let tmp = dir.join(format!("{ask_id}.{}.tmp", std::process::id()));
    let body = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &body)?;
    match std::fs::rename(&tmp, &final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Best-effort removal of both sidecars once the ask is resolved.
fn cleanup(ask_id: &str) {
    if let Some(p) = ask_request_path(ask_id) {
        let _ = std::fs::remove_file(p);
    }
    if let Some(p) = ask_response_path(ask_id) {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // XDG_STATE_HOME is process-global; serialize every crate test that sets it.
    use crate::ask::STATE_ENV_LOCK as ENV_LOCK;

    fn set_temp_state(tmp: &std::path::Path) {
        // SAFETY: callers hold ENV_LOCK, so only one test mutates the env at a time.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp);
            std::env::remove_var(ENV_PARENT_RUN_ID);
        }
    }
    fn clear_temp_state() {
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn ask_kind_parses_and_defaults_to_review() {
        assert_eq!(
            AskKind::parse_or_default(Some("blocking")),
            AskKind::Blocking
        );
        assert_eq!(
            AskKind::parse_or_default(Some("BLOCKING")),
            AskKind::Blocking
        );
        assert_eq!(AskKind::parse_or_default(Some("review")), AskKind::Review);
        assert_eq!(AskKind::parse_or_default(Some("garbage")), AskKind::Review);
        assert_eq!(AskKind::parse_or_default(None), AskKind::Review);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn answered_when_response_written() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        set_temp_state(tmp.path());

        let req = AskRequest {
            prompt: "Proceed?".into(),
            options: vec!["yes".into(), "no".into()],
            kind: AskKind::Blocking,
            timeout_secs: 30,
        };
        let handle = tokio::spawn(ask(req, CancellationToken::new()));

        // Wait for the request sidecar, then answer it.
        let dir = crate::events::asks_dir();
        let ask_id = loop {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                let found = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .find(|n| {
                        n.starts_with("ask-") && n.ends_with(".json") && !n.contains(".response.")
                    });
                if let Some(name) = found {
                    break name.trim_end_matches(".json").to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let resp_path = crate::events::ask_response_path(&ask_id).unwrap();
        std::fs::write(
            &resp_path,
            format!("{{\"ask_id\":\"{ask_id}\",\"answer\":\"yes\",\"ts\":1}}"),
        )
        .unwrap();

        let outcome = handle.await.unwrap();
        assert_eq!(outcome, AskOutcome::Answered("yes".to_string()));
        clear_temp_state();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn unavailable_on_timeout_and_records_it() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        set_temp_state(tmp.path());

        let req = AskRequest {
            prompt: "?".into(),
            options: vec![],
            kind: AskKind::Review,
            timeout_secs: 1,
        };
        let start = Instant::now();
        let outcome = ask(req, CancellationToken::new()).await;
        assert_eq!(outcome, AskOutcome::Unavailable);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should time out near 1s"
        );

        let log =
            std::fs::read_to_string(tmp.path().join("agentpit/events.jsonl")).unwrap_or_default();
        assert!(
            log.contains("\"event\":\"ask\""),
            "ask event missing: {log}"
        );
        assert!(
            log.contains("\"timed_out\":true"),
            "timeout not recorded: {log}"
        );
        clear_temp_state();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn unavailable_on_cancel() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        set_temp_state(tmp.path());

        let cancel = CancellationToken::new();
        let req = AskRequest {
            prompt: "?".into(),
            options: vec![],
            kind: AskKind::Review,
            timeout_secs: 60,
        };
        let handle = tokio::spawn(ask(req, cancel.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome, AskOutcome::Unavailable);
        clear_temp_state();
    }
}

//! The Needs-You inbox back-channel.
//!
//! `get_pending_asks` scans the `asks/` sidecar files the `agentpit ask` core posts (the source
//! of truth — immune to events.jsonl compaction) and renders the unanswered ones as cards.
//! `answer_ask` writes the human's reply back atomically so the blocked manager's poll picks it
//! up. Both are best-effort and never panic — a telemetry/back-channel hiccup must not crash the
//! dashboard.

use serde::Serialize;

use crate::pid_alive;

/// One pending ask, rendered as an inbox card. camelCase for the JS frontend.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskCard {
    pub ask_id: String,
    pub run_id: String,
    pub ts: u64,
    pub prompt: String,
    pub options: Vec<String>,
    /// "blocking" (a worker is stalled) or "review" (nothing blocked).
    pub kind: String,
    pub timeout_secs: u64,
}

/// The request sidecar shape written by the `agentpit ask` core.
#[derive(serde::Deserialize)]
struct AskRecord {
    ask_id: String,
    run_id: String,
    ts: u64,
    prompt: String,
    #[serde(default)]
    options: Vec<String>,
    kind: String,
    #[serde(default)]
    timeout_secs: u64,
    #[serde(default)]
    pid: u32,
}

fn now_ms() -> u64 {
    agentpit_events::now_ms()
}

/// List the pending asks for the inbox: unanswered requests, with the reaper applied, sorted
/// blocking-before-review then oldest-first (FIFO within a lane).
#[tauri::command]
pub fn get_pending_asks() -> Vec<AskCard> {
    let dir = agentpit_events::asks_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = now_ms();
    let mut cards: Vec<AskCard> = Vec::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Request sidecars are "<ask_id>.json" but NOT "<ask_id>.response.json" or a temp file.
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.ends_with(".response") || !agentpit_events::is_safe_log_component(stem) {
            continue;
        }
        let Some(req_path) = agentpit_events::ask_request_path(stem) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&req_path) else {
            continue;
        };
        let Ok(rec) = serde_json::from_slice::<AskRecord>(&bytes) else {
            continue;
        };
        // The recorded id must match the filename (defence against a stray/renamed file).
        if rec.ask_id != stem {
            continue;
        }
        // Already answered → skip; the asker will clean both sidecars up shortly.
        if agentpit_events::ask_response_path(stem).is_some_and(|p| p.exists()) {
            continue;
        }

        // REAPER. Timeout-elapsed is REQUIRED before reaping (a dead-pid check alone is unsafe —
        // the OS recycles pids within seconds, so a live unrelated process could keep a zombie
        // card forever, or a recycled pid could reap a still-valid one). Once the deadline has
        // also passed, a dead asker lets us delete the orphaned request eagerly.
        let deadline = rec.ts.saturating_add(rec.timeout_secs.saturating_mul(1000));
        let expired = rec.timeout_secs > 0 && now >= deadline;
        if expired {
            if rec.pid == 0 || !pid_alive(rec.pid) {
                let _ = std::fs::remove_file(&req_path);
            }
            // Expired but asker still alive: its own poll is about to time out and clean up. Hide
            // the card either way — the manager has stopped waiting on a human answer.
            continue;
        }

        let kind = if rec.kind == "blocking" {
            "blocking"
        } else {
            "review"
        }
        .to_string();
        cards.push(AskCard {
            ask_id: rec.ask_id,
            run_id: rec.run_id,
            ts: rec.ts,
            prompt: rec.prompt,
            options: rec.options,
            kind,
            timeout_secs: rec.timeout_secs,
        });
    }

    // Blocking before review, then oldest-first (FIFO) within a lane.
    cards.sort_by(|a, b| {
        let lane = |k: &str| u8::from(k != "blocking");
        lane(&a.kind).cmp(&lane(&b.kind)).then(a.ts.cmp(&b.ts))
    });
    cards
}

/// The result of answering an ask.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn answer_err(msg: &str) -> AnswerResult {
    AnswerResult {
        ok: false,
        error: Some(msg.to_string()),
    }
}

/// Record the human's answer to an ask. Writes the response sidecar atomically (temp + rename in
/// the same dir) so the manager's poll never reads a partial file. Refuses an unknown or
/// already-answered ask, and validates the id against path traversal first.
#[tauri::command]
pub fn answer_ask(ask_id: String, value: String) -> AnswerResult {
    if !agentpit_events::is_safe_log_component(&ask_id) {
        return answer_err("invalid ask id");
    }
    let (Some(req_path), Some(resp_path)) = (
        agentpit_events::ask_request_path(&ask_id),
        agentpit_events::ask_response_path(&ask_id),
    ) else {
        return answer_err("invalid ask id");
    };
    if !req_path.exists() {
        return answer_err("no such ask (it may already be resolved)");
    }
    if resp_path.exists() {
        return answer_err("already answered");
    }

    let body = serde_json::json!({ "ask_id": ask_id, "answer": value, "ts": now_ms() }).to_string();
    let tmp =
        agentpit_events::asks_dir().join(format!("{ask_id}.response.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return answer_err("failed to write response");
    }
    if std::fs::rename(&tmp, &resp_path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return answer_err("failed to publish response");
    }
    // Post-rename TOCTOU close: if the request vanished while we wrote (the asker timed out and
    // cleaned up), our response is an orphan that would otherwise look like a fresh ask — drop it.
    if !req_path.exists() {
        let _ = std::fs::remove_file(&resp_path);
    }
    AnswerResult {
        ok: true,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // XDG_STATE_HOME is process-global; serialize the tests that set it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_request(ask_id: &str, kind: &str, ts: u64, timeout_secs: u64, pid: u32) {
        let dir = agentpit_events::asks_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "ask_id": ask_id, "run_id": "run-1", "ts": ts, "prompt": "Proceed?",
            "options": ["yes", "no"], "kind": kind, "timeout_secs": timeout_secs, "pid": pid,
        })
        .to_string();
        std::fs::write(agentpit_events::ask_request_path(ask_id).unwrap(), body).unwrap();
    }

    #[test]
    fn answer_round_trips_and_rejects_unsafe_and_double_answer() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }

        // Traversal id is rejected before any fs touch.
        assert!(!answer_ask("../etc/passwd".into(), "x".into()).ok);

        // Unknown ask → rejected.
        assert!(!answer_ask("ask-1-2-3".into(), "yes".into()).ok);

        // Post a request, answer it, and confirm the response sidecar holds the answer.
        write_request("ask-1-2-3", "blocking", now_ms(), 120, std::process::id());
        let r = answer_ask("ask-1-2-3".into(), "yes".into());
        assert!(r.ok, "answer failed: {:?}", r.error);
        let resp =
            std::fs::read_to_string(agentpit_events::ask_response_path("ask-1-2-3").unwrap())
                .unwrap();
        assert!(resp.contains("\"answer\":\"yes\""), "got: {resp}");

        // Answering again → rejected (response already present).
        assert!(!answer_ask("ask-1-2-3".into(), "no".into()).ok);

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn pending_lists_fresh_hides_answered_and_reaps_expired_dead() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let now = now_ms();

        // A fresh blocking ask (alive asker, ample timeout) → listed.
        write_request("ask-1-2-fresh", "blocking", now, 300, std::process::id());
        // A fresh review ask → listed, but after the blocking one.
        write_request("ask-1-2-review", "review", now, 300, std::process::id());
        // An expired ask from a dead pid → reaped (file removed), not listed.
        write_request("ask-1-2-dead", "blocking", 1, 1, 999_999_999);

        let cards = get_pending_asks();
        let ids: Vec<&str> = cards.iter().map(|c| c.ask_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ask-1-2-fresh", "ask-1-2-review"],
            "blocking sorts first; expired-dead reaped"
        );
        // The reaped request file is gone.
        assert!(!agentpit_events::ask_request_path("ask-1-2-dead")
            .unwrap()
            .exists());

        // Answered asks are hidden.
        answer_ask("ask-1-2-fresh".into(), "yes".into());
        let after = get_pending_asks();
        assert_eq!(
            after.iter().map(|c| c.ask_id.as_str()).collect::<Vec<_>>(),
            vec!["ask-1-2-review"]
        );

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}

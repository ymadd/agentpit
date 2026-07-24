//! The embedding side of similarity routing (`--features similarity` only).
//!
//! Wraps fastembed's multilingual-e5-small (ONNX, ~120MB): small enough for a CLI, and
//! multilingual because task texts are frequently Japanese. The model is NEVER downloaded
//! implicitly — `agentpit similarity init` fetches it into `<state>/models` and drops a
//! ready marker; until that marker exists every entry point here returns "not ready" and
//! the router falls through exactly as in a build without the feature.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use super::{RouteSample, SimilarityPick, parse_samples, pick_backend, routes_path};
use crate::config::SimilaritySection;
use crate::types::BackendId;

fn models_dir() -> PathBuf {
    crate::events::state_dir().join("models")
}

fn ready_marker() -> PathBuf {
    models_dir().join(".multilingual-e5-small.ready")
}

/// True once `agentpit similarity init` completed a model download.
pub fn model_ready() -> bool {
    ready_marker().is_file()
}

fn load_model(show_progress: bool) -> Result<TextEmbedding> {
    TextEmbedding::try_new(
        TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_cache_dir(models_dir())
            .with_show_download_progress(show_progress),
    )
    .context("failed to load the multilingual-e5-small embedding model")
}

/// Download the embedding model into the state dir and mark it ready.
pub fn init() -> Result<()> {
    std::fs::create_dir_all(models_dir())?;
    let mut model = load_model(true)?;
    // Prove the runtime works end-to-end before declaring readiness.
    let probe = model
        .embed(vec!["embedding smoke test".to_string()], None)
        .context("model loaded but embedding failed")?;
    anyhow::ensure!(
        probe.first().map(Vec::len).unwrap_or(0) > 0,
        "model returned an empty embedding"
    );
    std::fs::write(ready_marker(), b"ok")?;
    Ok(())
}

/// Bulk-embed task texts (the `profile learn` ingestion path). Requires a ready model.
pub fn embed_texts(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    anyhow::ensure!(
        model_ready(),
        "embedding model not downloaded — run `agentpit similarity init` first"
    );
    let mut model = load_model(false)?;
    model
        .embed(texts.to_vec(), None)
        .context("embedding failed")
}

/// Process-wide model instance so repeated resolves (REPL, MCP server) pay the ONNX session
/// load once.
static EMBEDDER: OnceLock<Mutex<TextEmbedding>> = OnceLock::new();

/// Embed one query, giving up after `timeout` (the cold ONNX load can exceed a routing
/// budget; the loader thread keeps warming the process-wide instance for the next call).
fn embed_query_with_timeout(task: String, timeout: Duration) -> Option<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let embedding = (|| -> Result<Vec<f32>> {
            if EMBEDDER.get().is_none() {
                let model = load_model(false)?;
                let _ = EMBEDDER.set(Mutex::new(model));
            }
            let mutex = EMBEDDER.get().expect("just set");
            let mut model = mutex.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = model.embed(vec![task], None)?;
            Ok(out.pop().unwrap_or_default())
        })();
        let _ = tx.send(embedding.ok());
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// The router's similarity stage: embed the task and consult the sample store. Every miss —
/// disabled, model not ready, no samples, slow load, thin evidence — returns `None` so the
/// resolve falls through to the profile route unchanged.
pub fn route(
    task: &str,
    cfg: &SimilaritySection,
    available: &HashSet<BackendId>,
) -> Option<SimilarityPick> {
    if !cfg.enabled || !model_ready() {
        return None;
    }
    // Cheap early exit before any model work.
    let raw = std::fs::read_to_string(routes_path()).ok()?;
    let samples: Vec<RouteSample> = parse_samples(&raw);
    if samples.is_empty() {
        return None;
    }
    let query = embed_query_with_timeout(
        task.to_string(),
        Duration::from_millis(cfg.load_timeout_ms.max(1)),
    )?;
    pick_backend(&query, &samples, cfg, |b| available.contains(&b))
}

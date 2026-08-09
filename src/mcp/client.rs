//! The only module in agentpit that starts an MCP server process.
//!
//! It is reached from exactly two places, both of them something the user just typed:
//! `agentpit mcp refresh` (and the `/mcp refresh` that suspends into it), and invoking a
//! `/<server>:<prompt>` command, which fetches that prompt's body with [`get_prompt`]. No
//! startup path, no slash-registry build, and no `mcp list` call comes here; those read
//! [`super::cache`]. That is the property to preserve when touching this file: a server runs
//! because the user asked for something from it, never because agentpit started.
//!
//! ## Bounding one connection
//!
//! A server is someone else's program, and a broken one hangs rather than failing. Two
//! `tokio::time::timeout`s bound each interaction — one around the spawn plus the
//! `initialize` handshake ([`connect`]), one around the question that follows it
//! (`prompts/list` or `prompts/get`) — each with the same per-server budget
//! (`[mcp].connect_timeout_secs`). Neither is an overall deadline shared with other servers:
//! [`refresh_all`] visits them one at a time, so a slow server delays the refresh by its own
//! budget and no more, and an invoked prompt waits for its own server alone.
//!
//! ## Killing the child
//!
//! Three layers, because a leaked MCP server holds a terminal and a port for the rest of the
//! session:
//!
//! 1. `Command::kill_on_drop(true)` — tokio's own backstop on the handle.
//! 2. rmcp's `TokioChildProcess` owns a `ChildWithCleanup` whose `Drop` calls `kill()`. The
//!    handshake timeout drops the future the transport was moved into, so a server that
//!    never answers `initialize` is killed by unwinding alone.
//! 3. After a successful handshake the child belongs to the running service; every exit path
//!    below goes through [`shutdown`], which calls `cancel()` — closing the transport, which
//!    for this transport *is* `graceful_shutdown()`: wait briefly, then kill.
//!
//! What that covers is the process agentpit started. It is a `kill(pid)`, not a process-group
//! kill, so a launcher that forks (`npx` running `node`) has its own child reaped by the
//! *other* half of the stdio contract instead: every layer above drops the pipe agentpit
//! holds, the server sees EOF on stdin, and an MCP stdio server exits on EOF. A grandchild
//! that ignores stdin outlives the refresh. Making this a group kill would trade that hole
//! for a worse one — a new process group is no longer in the shell's foreground group, so a
//! Ctrl-C during a refresh would stop reaching the server at all.
//!
//! `stderr` is `Stdio::null()`. A server that logs to stderr would otherwise scribble over
//! the TUI's alternate screen or the REPL prompt, and its logs are not agentpit's to relay.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;

use super::cache::{CachedArgument, CachedPrompt, PromptCache};
use super::prompts::FetchedMessage;
use super::servers::ServerDef;

/// Used when `[mcp].connect_timeout_secs` is 0 or absent.
const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Refuse a server that would take longer than this even if configured to: a per-server
/// budget is meant to bound a refresh, not to let one config line hang the CLI for an hour.
const MAX_TIMEOUT_SECS: u64 = 120;

pub fn timeout_from(secs: u64) -> Duration {
    let secs = if secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        secs
    };
    Duration::from_secs(secs.min(MAX_TIMEOUT_SECS))
}

/// What one connection to a server is, before any question is asked of it.
type Connection = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Spawn `def` and complete the `initialize` handshake.
///
/// Shared by [`list_prompts`] and [`get_prompt`] so both spend the same budget and get the
/// same three-layer cleanup; every early return here has already killed the child.
async fn connect(def: &ServerDef, budget: Duration) -> Result<Connection> {
    let mut cmd = tokio::process::Command::new(&def.command);
    cmd.args(&def.args);
    for (k, v) in &def.env {
        cmd.env(k, v);
    }
    if !def.cwd.trim().is_empty() {
        cmd.current_dir(&def.cwd);
    }
    // Layer 1 of the cleanup story (see module docs).
    cmd.kill_on_drop(true);

    let (child, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start MCP server '{}' ({})",
                def.name, def.command
            )
        })?;

    // `()` is rmcp's do-nothing client handler: agentpit asks questions here and serves no
    // callbacks. The transport is MOVED into this future, so dropping it on timeout drops
    // the child with it (layer 2).
    tokio::time::timeout(budget, ().serve(child))
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server '{}' did not complete the handshake within {}s",
                def.name,
                budget.as_secs()
            )
        })?
        .with_context(|| format!("MCP server '{}' failed to initialize", def.name))
}

/// Spawn `def`, read its prompt list, and shut it down.
///
/// Returns the prompts as the cache stores them.
pub async fn list_prompts(def: &ServerDef, budget: Duration) -> Result<Vec<CachedPrompt>> {
    let service = connect(def, budget).await?;
    let listed = tokio::time::timeout(budget, service.list_all_prompts()).await;
    // One exit point for the child, taken on the timeout path too (layer 3).
    let result = match listed {
        Err(_) => Err(anyhow!(
            "MCP server '{}' did not answer prompts/list within {}s",
            def.name,
            budget.as_secs()
        )),
        Ok(Err(e)) => Err(anyhow!(
            "MCP server '{}' refused prompts/list: {e}",
            def.name
        )),
        Ok(Ok(prompts)) => Ok(prompts.into_iter().map(to_cached).collect()),
    };
    shutdown(service, &def.name).await;
    result
}

/// Spawn `def`, fetch one prompt's body with `prompts/get`, and shut it down.
///
/// `arguments` is what [`super::prompts::map_arguments`] made of the user's text: only names
/// the prompt itself declared, so nothing agentpit invented reaches the wire.
///
/// Every failure is an `Err` the surface shows as a refusal. That is the whole point of
/// returning a `Result` here rather than a best-effort turn: the body lives on the server,
/// and a server that cannot be reached has nothing to send — dispatching *something* anyway
/// would spend a turn on a prompt nobody has.
pub async fn get_prompt(
    def: &ServerDef,
    prompt: &str,
    arguments: &BTreeMap<String, String>,
    budget: Duration,
) -> Result<Vec<FetchedMessage>> {
    let service = connect(def, budget).await?;
    let mut params = rmcp::model::GetPromptRequestParams::new(prompt);
    if !arguments.is_empty() {
        params = params.with_arguments(
            arguments
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        );
    }
    let fetched = tokio::time::timeout(budget, service.get_prompt(params)).await;
    let result = match fetched {
        Err(_) => Err(anyhow!(
            "MCP server '{}' did not answer prompts/get for '{prompt}' within {}s",
            def.name,
            budget.as_secs()
        )),
        Ok(Err(e)) => Err(anyhow!(
            "MCP server '{}' refused prompts/get for '{prompt}': {e}",
            def.name
        )),
        Ok(Ok(result)) => Ok(result.messages.into_iter().map(to_fetched).collect()),
    };
    shutdown(service, &def.name).await;
    result
}

/// Close one connection, killing the child if it will not leave.
///
/// Failures are reported, not propagated: the caller's result is about the prompt list, and
/// a server that exits badly after answering has still answered.
async fn shutdown<S>(service: rmcp::service::RunningService<rmcp::RoleClient, S>, name: &str)
where
    S: rmcp::service::Service<rmcp::RoleClient>,
{
    if let Err(e) = service.cancel().await {
        eprintln!("mcp: '{name}' did not shut down cleanly: {e}");
    }
}

/// One wire message flattened into the role/text pair the composer works in.
///
/// The flattening lives here rather than in [`super::prompts`] so that the composer stays a
/// pure function over agentpit's own type, testable without constructing rmcp models.
///
/// A prompt may carry content a text turn cannot: an image, an embedded blob, a link. Those
/// become a bracketed note rather than being dropped, so the turn says what was in the prompt
/// even when it cannot carry it. An embedded *text* resource is real text and is kept.
pub(crate) fn to_fetched(message: rmcp::model::PromptMessage) -> FetchedMessage {
    use rmcp::model::{PromptMessageContent, PromptMessageRole, ResourceContents};
    let role = match message.role {
        PromptMessageRole::User => "user",
        PromptMessageRole::Assistant => "assistant",
    };
    let text = match message.content {
        PromptMessageContent::Text { text } => text,
        PromptMessageContent::Resource { resource } => match &resource.resource {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            ResourceContents::BlobResourceContents { uri, .. } => {
                format!("[embedded binary resource omitted: {uri}]")
            }
        },
        PromptMessageContent::Image { .. } => "[image content omitted]".to_string(),
        PromptMessageContent::ResourceLink { link } => {
            format!("[resource link: {}]", link.uri)
        }
    };
    FetchedMessage {
        role: role.to_string(),
        text,
    }
}

fn to_cached(prompt: rmcp::model::Prompt) -> CachedPrompt {
    CachedPrompt {
        name: prompt.name,
        description: prompt.description.unwrap_or_default(),
        arguments: prompt
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|a| CachedArgument {
                name: a.name,
                required: a.required.unwrap_or(false),
            })
            .collect(),
    }
}

/// What one server's refresh produced, for the report `mcp refresh` prints.
pub struct RefreshOutcome {
    pub name: String,
    pub result: Result<usize>,
}

/// Refresh `defs`, one at a time, writing the cache once at the end.
///
/// Sequential rather than concurrent: these are child processes started on the user's
/// machine on their behalf, and N servers booting at once is a spike the user did not ask
/// for. A failing server does not stop the others, and does not evict what it had cached —
/// losing yesterday's working prompt list because a server is briefly broken would be the
/// wrong trade.
pub async fn refresh_all(
    defs: &[ServerDef],
    budget: Duration,
) -> (Vec<RefreshOutcome>, PromptCache) {
    let mut cache = PromptCache::load();
    let mut outcomes = Vec::new();
    for def in defs {
        let result = list_prompts(def, budget).await;
        let outcome = match result {
            Ok(prompts) => {
                let n = prompts.len();
                cache.put(def, prompts);
                Ok(n)
            }
            Err(e) => Err(e),
        };
        outcomes.push(RefreshOutcome {
            name: def.name.clone(),
            result: outcome,
        });
    }
    (outcomes, cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_is_bounded_at_both_ends() {
        assert_eq!(timeout_from(0), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(timeout_from(5), Duration::from_secs(5));
        assert_eq!(
            timeout_from(u64::MAX),
            Duration::from_secs(MAX_TIMEOUT_SECS),
            "one config line must not be able to hang the CLI indefinitely"
        );
    }

    /// The wire half of the message-to-turn conversion. Text — including multibyte text —
    /// survives byte for byte; content a text turn cannot carry is NAMED rather than
    /// silently dropped, so a turn composed out of these says what was in the prompt.
    #[test]
    fn wire_messages_flatten_to_role_and_text_and_name_what_they_cannot_carry() {
        use rmcp::model::{
            Annotated, PromptMessage, PromptMessageContent, PromptMessageRole, RawEmbeddedResource,
            ResourceContents,
        };

        let jp = "レビュー担当者に、具体的な反論を三つ求めてください。🔍";
        let text = to_fetched(PromptMessage::new(
            PromptMessageRole::User,
            PromptMessageContent::text(jp),
        ));
        assert_eq!(text.role, "user");
        assert_eq!(text.text, jp, "no byte of the wire text is lost");

        assert_eq!(
            to_fetched(PromptMessage::new(
                PromptMessageRole::Assistant,
                PromptMessageContent::text("ok"),
            ))
            .role,
            "assistant"
        );

        // An embedded TEXT resource is real text and is kept…
        let embedded = to_fetched(PromptMessage::new(
            PromptMessageRole::User,
            PromptMessageContent::Resource {
                resource: Annotated::new(
                    RawEmbeddedResource {
                        meta: None,
                        resource: ResourceContents::text("cargo build", "file:///build.md"),
                    },
                    None,
                ),
            },
        ));
        assert_eq!(embedded.text, "cargo build");

        // …while a blob is named, not pasted as base64 into a prompt.
        let blob = to_fetched(PromptMessage::new(
            PromptMessageRole::User,
            PromptMessageContent::Resource {
                resource: Annotated::new(
                    RawEmbeddedResource {
                        meta: None,
                        resource: ResourceContents::BlobResourceContents {
                            uri: "file:///logo.png".into(),
                            mime_type: Some("image/png".into()),
                            blob: "AAAA".into(),
                            meta: None,
                        },
                    },
                    None,
                ),
            },
        ));
        assert_eq!(
            blob.text,
            "[embedded binary resource omitted: file:///logo.png]"
        );
        assert!(!blob.text.contains("AAAA"), "{}", blob.text);
    }
}

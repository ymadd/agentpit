//! Model candidates for the workflow settings UI.
//!
//! Only fixed, audited commands are launched here. The frontend supplies no executable or
//! arguments, so model discovery cannot become a general-purpose command runner. Claude Code has
//! no non-interactive model-list command; its stable aliases come from the official model-config
//! documentation. Gemini remains free-form in the UI because its model picker is interactive.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

use crate::cli_versions::resolve_command;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MODELS: usize = 4096;
const MAX_ERROR_CHARS: usize = 600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub backend: String,
    /// `cli` for live discovery and `static` for the documented Claude aliases.
    pub kind: String,
    pub source: String,
    pub models: Vec<ModelOption>,
    pub error: Option<String>,
}

pub async fn list(refresh: bool) -> Vec<ModelCatalog> {
    // These probes are independent and can each involve network/auth checks. Running them
    // concurrently keeps opening Settings bounded by the slowest CLI instead of their sum.
    let (codex, antigravity, opencode, prime_agent) = tokio::join!(
        codex_catalog(),
        antigravity_catalog(),
        opencode_catalog(refresh),
        prime_agent_catalog()
    );

    vec![claude_catalog(), codex, antigravity, opencode, prime_agent]
}

fn claude_catalog() -> ModelCatalog {
    // https://code.claude.com/docs/en/model-config#model-aliases
    // Aliases intentionally win over pinned version ids here: they follow the account/provider's
    // supported release, while the editable input still accepts a pinned or gateway-specific id.
    let options = [
        ("default", "Default (account recommended)"),
        ("best", "Best available"),
        ("sonnet", "Sonnet (latest)"),
        ("opus", "Opus (latest)"),
        ("haiku", "Haiku (latest)"),
        ("sonnet[1m]", "Sonnet (1M context)"),
        ("opus[1m]", "Opus (1M context)"),
        ("opusplan", "Opus for planning, Sonnet for execution"),
    ];
    ModelCatalog {
        backend: "claude".into(),
        kind: "static".into(),
        source: "Claude Code documentation".into(),
        models: options
            .into_iter()
            .map(|(value, label)| ModelOption {
                value: value.into(),
                label: label.into(),
            })
            .collect(),
        error: None,
    }
}

async fn codex_catalog() -> ModelCatalog {
    let source = "codex debug models";
    match run_cli("codex", &["debug", "models"]).await {
        Ok(stdout) => match parse_codex_models(&stdout) {
            Ok(models) if !models.is_empty() => success("codex", source, models),
            Ok(_) => failure("codex", source, "Codex returned no selectable models"),
            Err(error) => failure("codex", source, error),
        },
        Err(error) => failure("codex", source, error),
    }
}

async fn antigravity_catalog() -> ModelCatalog {
    let source = "agy models";
    match run_cli("agy", &["models"]).await {
        Ok(stdout) => {
            let models = parse_line_models(&stdout, false);
            if models.is_empty() {
                failure("antigravity", source, "Antigravity returned no models")
            } else {
                success("antigravity", source, models)
            }
        }
        Err(error) => failure("antigravity", source, error),
    }
}

async fn opencode_catalog(refresh: bool) -> ModelCatalog {
    let (args, source): (&[&str], &str) = if refresh {
        (&["models", "--refresh"], "opencode models --refresh")
    } else {
        (&["models"], "opencode models")
    };
    match run_cli("opencode", args).await {
        Ok(stdout) => {
            let models = parse_line_models(&stdout, true);
            if models.is_empty() {
                failure("opencode", source, "OpenCode returned no models")
            } else {
                success("opencode", source, models)
            }
        }
        Err(error) => failure("opencode", source, error),
    }
}

async fn prime_agent_catalog() -> ModelCatalog {
    let source = "prime-agent model list";
    match run_cli("prime-agent", &["model", "list"]).await {
        Ok(stdout) => {
            let models = parse_prime_agent_models(&stdout);
            if models.is_empty() {
                failure("prime-agent", source, "Prime Agent returned no models")
            } else {
                success("prime-agent", source, models)
            }
        }
        Err(error) => failure("prime-agent", source, error),
    }
}

fn success(backend: &str, source: &str, models: Vec<ModelOption>) -> ModelCatalog {
    ModelCatalog {
        backend: backend.into(),
        kind: "cli".into(),
        source: source.into(),
        models,
        error: None,
    }
}

fn failure(backend: &str, source: &str, error: impl Into<String>) -> ModelCatalog {
    ModelCatalog {
        backend: backend.into(),
        kind: "cli".into(),
        source: source.into(),
        models: Vec::new(),
        error: Some(error.into()),
    }
}

async fn run_cli(command: &str, args: &[&str]) -> Result<String, String> {
    let path = resolve_command(command).ok_or_else(|| format!("{command} is not installed"))?;
    let mut process = Command::new(path);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = timeout(COMMAND_TIMEOUT, process.output())
        .await
        .map_err(|_| format!("{command} model discovery timed out after 20s"))?
        .map_err(|error| format!("failed to run {command}: {error}"))?;
    if !output.status.success() {
        let stderr = clean_text(&String::from_utf8_lossy(&output.stderr));
        let stdout = clean_text(&String::from_utf8_lossy(&output.stdout));
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let detail = if detail.is_empty() {
            "no error output".to_string()
        } else {
            truncate_chars(&detail, MAX_ERROR_CHARS)
        };
        return Err(format!(
            "{command} exited with {}: {detail}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into())
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Deserialize)]
struct CodexCatalog {
    models: Vec<CodexModel>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    display_name: Option<String>,
    visibility: Option<String>,
}

fn parse_codex_models(raw: &str) -> Result<Vec<ModelOption>, String> {
    let catalog: CodexCatalog = serde_json::from_str(raw.trim())
        .map_err(|error| format!("could not parse Codex model catalog: {error}"))?;
    let mut seen = HashSet::new();
    Ok(catalog
        .models
        .into_iter()
        .filter(|model| model.visibility.as_deref() == Some("list"))
        .filter(|model| !model.slug.trim().is_empty())
        .filter(|model| seen.insert(model.slug.clone()))
        .take(MAX_MODELS)
        .map(|model| ModelOption {
            label: model
                .display_name
                .filter(|label| !label.trim().is_empty())
                .unwrap_or_else(|| model.slug.clone()),
            value: model.slug,
        })
        .collect())
}

fn parse_line_models(raw: &str, require_provider_prefix: bool) -> Vec<ModelOption> {
    let clean = clean_text(raw);
    let mut seen = HashSet::new();
    clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !require_provider_prefix
                || (line.contains('/') && !line.chars().any(char::is_whitespace))
        })
        .filter(|line| seen.insert((*line).to_string()))
        .take(MAX_MODELS)
        .map(|line| ModelOption {
            value: line.to_string(),
            label: line.to_string(),
        })
        .collect()
}

/// `prime-agent model list` prints a padded table, not one id per line:
///
/// ```text
/// provider         model                     context  max-out  thinking  images
/// anthropic        claude-opus-5             1M       128K     yes       yes
/// prime-inference  anthropic/claude-opus-5   1M       128K     yes       yes
/// ```
///
/// The option VALUE is the canonical `provider/model` selector. prime-agent resolves
/// `--model` by splitting on the FIRST slash and treating a recognized prefix as a
/// provider, so passing a bare Prime Inference id like `anthropic/claude-opus-5` would
/// route to Anthropic directly — different credentials, billing and data destination.
/// Deduplication is by the full selector, so the same id under two providers stays
/// selectable under both. Only rows after the `provider  model …` header are parsed:
/// diagnostic prose ("No models available…") can never become a selectable model.
fn parse_prime_agent_models(raw: &str) -> Vec<ModelOption> {
    let clean = clean_text(raw);
    let mut seen = HashSet::new();
    let mut in_table = false;
    clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let provider = fields.next()?;
            let model = fields.next()?;
            if provider == "provider" && model == "model" {
                in_table = true;
                return None;
            }
            in_table.then(|| (provider.to_string(), model.to_string()))
        })
        .filter(|(provider, model)| seen.insert(format!("{provider}/{model}")))
        .take(MAX_MODELS)
        .map(|(provider, model)| ModelOption {
            label: format!("{model} ({provider})"),
            value: format!("{provider}/{model}"),
        })
        .collect()
}

fn clean_text(raw: &str) -> String {
    // Strip ANSI CSI sequences emitted by some CLIs while keeping Unicode model names intact.
    let mut clean = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else if ch != '\r' {
            clean.push(ch);
        }
    }
    clean.trim().to_string()
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from `prime-agent model list` (0.7.1, 2026-08-08), padding and all.
    const PRIME_AGENT_TABLE: &str = concat!(
        "provider         model                       context  max-out  thinking  images\n",
        "anthropic        claude-opus-5               1M       128K     yes       yes   \n",
        "prime-inference  anthropic/claude-opus-5     1M       128K     yes       yes   \n",
        "prime-inference  deepseek/deepseek-v4-pro    1.0M     384K     yes       no    \n",
    );

    #[test]
    fn prime_agent_table_yields_canonical_selectors() {
        let models = parse_prime_agent_models(PRIME_AGENT_TABLE);
        assert_eq!(
            models.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            [
                "anthropic/claude-opus-5",
                "prime-inference/anthropic/claude-opus-5",
                "prime-inference/deepseek/deepseek-v4-pro"
            ],
            "values must keep the provider: a bare `anthropic/claude-opus-5` would be \
             resolved by prime-agent as the Anthropic provider, not Prime Inference"
        );
        // The label still shows the raw id plus provider, as before.
        assert_eq!(models[1].label, "anthropic/claude-opus-5 (prime-inference)");
        assert!(parse_prime_agent_models("").is_empty());
    }

    #[test]
    fn prime_agent_prose_and_headerless_output_yield_no_models() {
        // A successful invocation with no models prints prose, not a table; two words on a
        // line must not become a bogus model entry.
        let prose = "No models available to display.\nUse /login to sign in first.";
        assert!(parse_prime_agent_models(prose).is_empty());
    }

    #[test]
    fn claude_uses_documented_aliases_and_keeps_custom_input_possible_in_ui() {
        let catalog = claude_catalog();
        let values: Vec<_> = catalog
            .models
            .iter()
            .map(|model| model.value.as_str())
            .collect();
        assert_eq!(
            values,
            vec![
                "default",
                "best",
                "sonnet",
                "opus",
                "haiku",
                "sonnet[1m]",
                "opus[1m]",
                "opusplan"
            ]
        );
        assert_eq!(catalog.kind, "static");
    }

    #[test]
    fn codex_parser_keeps_only_visible_unique_models() {
        let raw = r#"{"models":[
          {"slug":"gpt-visible","display_name":"GPT Visible","visibility":"list"},
          {"slug":"gpt-hidden","display_name":"GPT Hidden","visibility":"hide"},
          {"slug":"gpt-visible","display_name":"duplicate","visibility":"list"}
        ]}"#;
        let models = parse_codex_models(raw).unwrap();
        assert_eq!(
            models,
            vec![ModelOption {
                value: "gpt-visible".into(),
                label: "GPT Visible".into()
            }]
        );
    }

    #[test]
    fn line_parser_strips_ansi_deduplicates_and_can_require_provider_ids() {
        let raw = "\u{1b}[32mprovider/model-a\u{1b}[0m\nheading text\nprovider/model-a\nprovider/model-b\n";
        assert_eq!(
            parse_line_models(raw, true),
            vec![
                ModelOption {
                    value: "provider/model-a".into(),
                    label: "provider/model-a".into()
                },
                ModelOption {
                    value: "provider/model-b".into(),
                    label: "provider/model-b".into()
                }
            ]
        );
    }

    #[test]
    fn char_truncation_does_not_split_unicode() {
        assert_eq!(truncate_chars("あいう", 2), "あい…");
        assert_eq!(truncate_chars("ab", 2), "ab");
    }
}

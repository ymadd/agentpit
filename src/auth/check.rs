use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use crate::acp::opencode::opencode_binary;
use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub backend: BackendId,
    pub ok: bool,
    pub hint: String,
    pub login_command: String,
}

fn home_join(parts: &[&str]) -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    for part in parts {
        p.push(part);
    }
    p
}

async fn file_exists(path: &PathBuf) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

async fn run_exit_code(command: &str, args: &[&str]) -> i32 {
    match Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 127,
    }
}

async fn check_codex() -> AuthStatus {
    let exit = run_exit_code("codex", &["login", "status"]).await;
    let ok = exit == 0;
    AuthStatus {
        backend: BackendId::Codex,
        ok,
        hint: if ok {
            "Codex is authenticated.".into()
        } else {
            "Codex CLI is not logged in. Authenticate it once via OAuth.".into()
        },
        login_command: "codex login".into(),
    }
}

async fn check_antigravity() -> AuthStatus {
    // agy is the Gemini CLI successor and reuses Gemini OAuth credentials.
    // antigravity_state.pbtxt is created once the user has actually launched agy.
    let creds = home_join(&[".gemini", "oauth_creds.json"]);
    let state = home_join(&[".gemini", "antigravity", "antigravity_state.pbtxt"]);
    let ok = file_exists(&creds).await || file_exists(&state).await;
    AuthStatus {
        backend: BackendId::Antigravity,
        ok,
        hint: if ok {
            "Antigravity (agy) credentials are present (shares ~/.gemini/oauth_creds.json with Gemini CLI).".into()
        } else {
            "Antigravity CLI is not authenticated. Run `agy` once to sign in, or `agy auth login` for headless setups.".into()
        },
        login_command: "agy auth login".into(),
    }
}

async fn check_claude() -> AuthStatus {
    // A stale ~/.claude.json survives logout, so file presence is not an authentication
    // check. Claude Code exposes a non-interactive status command whose exit code tracks the
    // actual login state (and whose stdout is JSON in current releases).
    let exit = run_exit_code("claude", &["auth", "status"]).await;
    claude_status_from_exit(exit)
}

fn claude_status_from_exit(exit: i32) -> AuthStatus {
    let ok = exit == 0;
    AuthStatus {
        backend: BackendId::Claude,
        ok,
        hint: if ok {
            "Claude Code is authenticated.".into()
        } else {
            "Claude Code is not logged in. Authenticate it via `claude auth login`.".into()
        },
        login_command: "claude auth login".into(),
    }
}

/// Every provider credential prime-agent can hold: an `auth.json` key (OAuth or API key), plus
/// the environment variable that provider falls back to. Kept in sync with prime-agent's
/// `docs/providers.md`; a provider missing here only means agentpit cannot *see* that credential,
/// never that a dispatch would fail.
const PRIME_AGENT_PROVIDERS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("prime-inference", "PRIME_API_KEY"),
    ("google", "GEMINI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("mistral", "MISTRAL_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("cerebras", "CEREBRAS_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("zai", "ZAI_API_KEY"),
    ("opencode", "OPENCODE_API_KEY"),
    ("huggingface", "HF_TOKEN"),
    ("fireworks", "FIREWORKS_API_KEY"),
    ("vercel-ai-gateway", "AI_GATEWAY_API_KEY"),
    ("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
    ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
    ("kimi-coding", "KIMI_API_KEY"),
    ("minimax", "MINIMAX_API_KEY"),
    ("xiaomi", "XIAOMI_API_KEY"),
];

/// prime-agent has no non-interactive `login status` command — credentials live in
/// `~/.prime/agent/auth.json` (written by the TUI's `/login`) or in a provider environment
/// variable, and the auth file wins. So the check reads the same two sources prime-agent
/// itself resolves from, and names the provider it found rather than claiming a bare "ok".
fn prime_agent_status(auth_file: &str, env_provider: Option<&str>) -> AuthStatus {
    let from_file: Vec<&str> = serde_json::from_str::<serde_json::Value>(auth_file)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            PRIME_AGENT_PROVIDERS
                .iter()
                .map(|(provider, _)| *provider)
                .filter(|provider| map.contains_key(*provider))
                .collect()
        })
        .unwrap_or_default();

    let found = from_file.first().copied().or(env_provider);
    AuthStatus {
        backend: BackendId::PrimeAgent,
        ok: found.is_some(),
        hint: match found {
            Some(provider) if from_file.is_empty() => format!(
                "Prime Agent has a {provider} credential in the environment (no ~/.prime/agent/auth.json entry)."
            ),
            Some(_) => format!(
                "Prime Agent is authenticated for: {} (~/.prime/agent/auth.json).",
                from_file.join(", ")
            ),
            None => "Prime Agent has no credentials. Run `prime-agent` and use /login to sign in \
                     with a Claude Pro/Max, ChatGPT, or Copilot subscription, or export a \
                     provider API key such as ANTHROPIC_API_KEY."
                .into(),
        },
        // prime-agent's login is the TUI's `/login` slash command: there is no `prime-agent
        // login` subcommand to shell out to, so the launcher opens the TUI and the user runs it.
        login_command: "prime-agent".into(),
    }
}

async fn check_prime_agent() -> AuthStatus {
    let auth_file = tokio::fs::read_to_string(home_join(&[".prime", "agent", "auth.json"]))
        .await
        .unwrap_or_default();
    let env_provider = PRIME_AGENT_PROVIDERS
        .iter()
        .find(|(_, var)| std::env::var(var).is_ok_and(|value| !value.trim().is_empty()))
        .map(|(provider, _)| *provider);
    prime_agent_status(&auth_file, env_provider)
}

async fn check_opencode() -> AuthStatus {
    let bin = opencode_binary();
    let binary_ok = file_exists(&bin).await;
    if !binary_ok {
        return AuthStatus {
            backend: BackendId::Opencode,
            ok: false,
            hint: "OpenCode binary not found at ~/.opencode/bin/opencode.".into(),
            login_command: "curl -fsSL https://opencode.ai/install | bash".into(),
        };
    }
    AuthStatus {
        backend: BackendId::Opencode,
        ok: true,
        hint: "OpenCode binary present. Free models work out of the box; run `opencode auth login` only if you want to add paid providers.".into(),
        login_command: format!("{} auth login", bin.display()),
    }
}

pub async fn check_auth(backend: BackendId) -> AuthStatus {
    match backend {
        BackendId::Codex => check_codex().await,
        BackendId::Antigravity => check_antigravity().await,
        BackendId::Claude => check_claude().await,
        BackendId::Opencode => check_opencode().await,
        BackendId::PrimeAgent => check_prime_agent().await,
        other => AuthStatus {
            backend: other,
            ok: false,
            hint: format!("No auth checker registered for backend {other}."),
            login_command: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_requires_a_successful_cli_probe() {
        let logged_in = claude_status_from_exit(0);
        assert!(logged_in.ok);
        assert_eq!(logged_in.backend, BackendId::Claude);

        let logged_out = claude_status_from_exit(1);
        assert!(!logged_out.ok);
        assert_eq!(logged_out.login_command, "claude auth login");
        assert!(logged_out.hint.contains("not logged in"));
    }

    #[test]
    fn prime_agent_reads_the_auth_file_first_then_the_environment() {
        // Shape captured from ~/.prime/agent/auth.json (prime-agent 0.7.1).
        let file = r#"{"anthropic":{"type":"oauth","access":"x"},"prime-inference":{"type":"api_key","key":"y"}}"#;
        let from_file = prime_agent_status(file, None);
        assert!(from_file.ok);
        assert_eq!(from_file.backend, BackendId::PrimeAgent);
        assert!(
            from_file.hint.contains("anthropic, prime-inference"),
            "{}",
            from_file.hint
        );

        // No auth file, but a provider env var is exported: prime-agent would still run.
        let from_env = prime_agent_status("", Some("anthropic"));
        assert!(from_env.ok);
        assert!(from_env.hint.contains("environment"), "{}", from_env.hint);

        // Neither source, and an unparseable file, are both "not authenticated" — never a panic.
        for empty in ["", "{}", "not json at all"] {
            let none = prime_agent_status(empty, None);
            assert!(!none.ok, "{empty:?} must not count as a credential");
            assert_eq!(none.login_command, "prime-agent");
            assert!(none.hint.contains("/login"));
        }
    }
}

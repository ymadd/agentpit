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

async fn check_gemini() -> AuthStatus {
    let creds = home_join(&[".gemini", "oauth_creds.json"]);
    let ok = file_exists(&creds).await;
    AuthStatus {
        backend: BackendId::Gemini,
        ok,
        hint: if ok {
            "Gemini OAuth credentials are present.".into()
        } else {
            "Gemini CLI has no OAuth credentials. Launch it once to log in.".into()
        },
        login_command: "gemini".into(),
    }
}

async fn check_claude() -> AuthStatus {
    let cfg = home_join(&[".claude.json"]);
    let ok = file_exists(&cfg).await;
    AuthStatus {
        backend: BackendId::Claude,
        ok,
        hint: if ok {
            "Claude Code config is present.".into()
        } else {
            "Claude Code is not configured. Run it once interactively to sign in.".into()
        },
        login_command: "claude".into(),
    }
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
        BackendId::Gemini => check_gemini().await,
        BackendId::Claude => check_claude().await,
        BackendId::Opencode => check_opencode().await,
        other => AuthStatus {
            backend: other,
            ok: false,
            hint: format!("No auth checker registered for backend {other}."),
            login_command: String::new(),
        },
    }
}

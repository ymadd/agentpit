use std::io::{self, Write};

use console::style;

use super::state::SessionState;
use crate::dispatch::resolve_transport;

/// Print the one-time startup banner.
pub fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");
    eprintln!(
        "\n {} {}\n{}",
        style(" agentpit ").on_cyan().black().bold(),
        style(format!("v{version}")).dim(),
        style("type a task, /help for commands, Ctrl-D to exit").dim()
    );
}

/// Print the per-turn status line showing active backend, transport, and cwd.
/// Written to stderr so it does not contaminate piped stdout from dispatch.
pub fn print_status_line(state: &SessionState) {
    let backend = state.active_backend.unwrap_or(state.config.default.backend);

    let transport = resolve_transport(backend, &state.regs)
        .map(|t| t.as_str())
        .unwrap_or("none");

    let cwd_display = abbreviate_cwd(&state.cwd);

    eprintln!(
        "{}",
        style(format!("[{backend} | {transport} | {cwd_display}]")).dim()
    );
}

/// Abbreviate a path: substitute `~` for home, show last 2 components.
fn abbreviate_cwd(path: &std::path::Path) -> String {
    let home = dirs::home_dir();

    let display = if let Some(ref h) = home {
        if let Ok(rel) = path.strip_prefix(h) {
            let rel_str = rel.display().to_string();
            if rel_str.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rel_str}")
            }
        } else {
            path.display().to_string()
        }
    } else {
        path.display().to_string()
    };

    // Keep only last 2 path components for brevity.
    let parts: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return display;
    }
    // Preserve leading ~ or / prefix.
    let prefix = if display.starts_with("~/") {
        "~/"
    } else if display.starts_with('/') {
        "/"
    } else {
        ""
    };
    format!("{}…/{}", prefix, parts[parts.len() - 1])
}

/// Flush stderr (best-effort; terminal writes rarely fail).
pub fn flush_stderr() {
    let _ = io::stderr().flush();
}

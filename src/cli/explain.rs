use anyhow::Result;

use crate::config::RouteKey;
use crate::types::BackendId;

pub async fn run(
    target: String,
    deep: bool,
    backend: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let mut lines = vec![
        "You are working in a codebase rooted at the current working directory.".to_string(),
        String::new(),
        format!("Explain: {target}"),
        String::new(),
        "Workflow — MUST follow. Do not produce an explanation without reading.".to_string(),
        "1. Locate and read <target> in full (a file, symbol, module, or concept).".to_string(),
        "2. Read related files (callers, callees, neighbouring modules) needed to explain it accurately. Speculation without reading the surrounding code is not acceptable.".to_string(),
        "3. Cite file:line when it helps the reader navigate.".to_string(),
        String::new(),
    ];
    lines.push(if deep {
        "Provide a deep walk-through: design rationale, control flow, edge cases, and how it interacts with the surrounding system.".to_string()
    } else {
        "Keep the explanation tight (under 200 words). Lead with purpose, then mechanism.".to_string()
    });
    super::rescue::run_with_route(lines.join("\n"), backend, cwd, true, RouteKey::Explain).await
}

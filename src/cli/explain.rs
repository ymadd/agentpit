use anyhow::Result;

use crate::config::RouteKey;
use crate::types::BackendId;

pub async fn run(
    target: String,
    deep: bool,
    backend: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let lines = vec![
        format!("Explain: {target}"),
        if deep {
            "Provide a deep walk-through: design rationale, control flow, edge cases, and how it interacts with the surrounding system.".into()
        } else {
            "Keep the explanation tight (under 200 words). Lead with purpose, then mechanism.".into()
        },
    ];
    super::rescue::run_with_route(lines.join("\n"), backend, cwd, true, RouteKey::Explain).await
}

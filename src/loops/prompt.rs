//! Rendering the text a step sees (design §4.3 rules 9–10), and reading an agent's verdict.
//!
//! Templates know exactly seven placeholders — `goal`, `inputs.<name>`, `iteration`,
//! `feedback`, `instructions`, `nodes.<id>.output`, `nodes.<id>.outcome` — and nothing else:
//! no expressions, conditions or escapes. Validation already refused anything else, so an
//! unknown token here is left in place verbatim rather than guessed at.

use std::path::Path;

use agentpit_events::loops::*;

use super::paths::check_log_rel;

/// How much of an upstream node's output a template may pull in.
pub const MAX_NODE_OUTPUT_BYTES: usize = 32 * 1024;
/// Board excerpt of an answer.
pub const EXCERPT_BYTES: usize = 280;

/// Replace every `{{token}}` with `lookup(token)` (trimmed). Unterminated `{{` and tokens
/// `lookup` does not know are kept as written.
pub fn render_template(template: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = after[..end].trim();
        match lookup(token) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Whether a template references `token` (e.g. `"feedback"`).
pub fn mentions(template: &str, token: &str) -> bool {
    placeholders(template).is_ok_and(|t| t.iter().any(|t| t == token))
}

/// `{{feedback}}`: what earlier iterations left, one bullet per item. Empty when none.
pub fn feedback_block(items: &[FeedbackItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("Feedback from earlier iterations (fix these):\n");
    for f in items {
        let step = f
            .step_id
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- [{} {}{step}] {}\n",
            f.source, f.node, f.summary
        ));
        if let Some(detail) = f.detail.as_deref().filter(|d| !d.trim().is_empty()) {
            for line in detail.trim_end().lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// `{{instructions}}`: what people told this step while the loop ran. Empty when none.
pub fn instructions_block(texts: &[&str]) -> String {
    if texts.is_empty() {
        return String::new();
    }
    let mut out = String::from("Instructions from the user (these take priority):\n");
    for t in texts {
        let mut lines = t.trim().lines();
        if let Some(first) = lines.next() {
            out.push_str("- ");
            out.push_str(first);
            out.push('\n');
        }
        for line in lines {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// What a verdict agent is told about its last line.
pub const VERDICT_INSTRUCTION: &str = "When you are done, end your answer with one final line that is exactly \
     `VERDICT: PASS` or `VERDICT: FAIL`. For FAIL, list the concrete problems right above that \
     line: they are handed to the next iteration.";

/// Read `VERDICT: PASS|FAIL` from the last non-empty line (markdown emphasis and code
/// ticks around it are ignored). `None` when the answer has no verdict line.
pub fn parse_verdict(answer: &str) -> Option<Verdict> {
    let mut lines: Vec<&str> = answer.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let last = lines.pop()?;
    let bare: String = last
        .trim()
        .trim_matches(|c: char| c == '*' || c == '`' || c == '_' || c == '#' || c == '>')
        .trim()
        .to_ascii_uppercase();
    let verdict = bare.strip_prefix("VERDICT")?.trim_start();
    let verdict = verdict
        .strip_prefix(':')
        .unwrap_or(verdict)
        .trim()
        .trim_matches(|c: char| c == '*' || c == '`' || c == '.');
    let pass = match verdict {
        "PASS" => true,
        "FAIL" => false,
        _ => return None,
    };
    let findings = lines.join("\n");
    let findings = findings.trim();
    Some(Verdict {
        pass,
        findings: (!pass && !findings.is_empty()).then(|| tail_text(findings, MAX_DETAIL_BYTES)),
    })
}

/// The first non-empty lines of `text`, up to [`EXCERPT_BYTES`].
pub fn excerpt(text: &str) -> Option<String> {
    let joined: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(4)
        .collect();
    let joined = joined.join(" ");
    (!joined.is_empty()).then(|| clamp_text(&joined, EXCERPT_BYTES))
}

/// The last `max` bytes of `text`, cut on a char boundary.
pub fn tail_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// The last `max` bytes of a file (lossy UTF-8), or `None` when it cannot be read.
pub fn file_tail(path: &Path, max: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max as u64);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // A cut through a multi-byte char shows up as a leading replacement char; drop it.
    Some(text.trim_start_matches('\u{FFFD}').to_string())
}

/// Resolve one placeholder for a step of `node` at iteration path `iter`.
pub fn resolve_token(
    state: &LoopState,
    loop_dir: &Path,
    node: &str,
    iter: &[u32],
    instructions: &[&str],
    token: &str,
) -> Option<String> {
    let created = state.created.as_deref()?;
    let parts: Vec<&str> = token.split('.').collect();
    match parts.as_slice() {
        ["goal"] => Some(created.inputs.get("goal").cloned().unwrap_or_default()),
        ["inputs", name] => Some(created.inputs.get(*name).cloned().unwrap_or_default()),
        ["iteration"] => Some(iter.last().map(u32::to_string).unwrap_or_default()),
        ["feedback"] => Some(feedback_block(current_feedback(state, node, iter))),
        ["instructions"] => Some(instructions_block(instructions)),
        ["nodes", target, "outcome"] => Some(
            node_instance(state, node, target, iter)
                .and_then(|i| i.outcome)
                .map(|o| o.to_string())
                .unwrap_or_default(),
        ),
        ["nodes", target, "output"] => Some(node_output(state, loop_dir, node, target, iter)),
        _ => None,
    }
}

fn repeat_chain(state: &LoopState, node: &str) -> Vec<String> {
    state
        .blueprint
        .as_ref()
        .map(|bp| bp.index().repeat_chain(node))
        .unwrap_or_default()
}

/// The feedback handed to the iteration of `node`'s innermost repeat that `iter` names.
pub fn current_feedback<'a>(state: &'a LoopState, node: &str, iter: &[u32]) -> &'a [FeedbackItem] {
    let chain = repeat_chain(state, node);
    let (Some(repeat), Some((_, outer))) = (chain.last(), iter.split_last()) else {
        return &[];
    };
    state
        .repeats
        .get(&instance_key(repeat, outer))
        .filter(|r| r.current == iter[iter.len() - 1])
        .map_or(&[], |r| r.feedback.as_slice())
}

/// The instance of `target` a template of `node` at `iter` means: the one in the same or
/// an enclosing scope when `target` lives there, else the most recently started one.
fn node_instance<'a>(
    state: &'a LoopState,
    node: &str,
    target: &str,
    iter: &[u32],
) -> Option<&'a InstanceRun> {
    let target_chain = repeat_chain(state, target);
    let node_chain = repeat_chain(state, node);
    if node_chain.starts_with(&target_chain)
        && target_chain.len() <= iter.len()
        && let Some(i) = state.instance(target, &iter[..target_chain.len()])
    {
        return Some(i);
    }
    state
        .instances
        .values()
        .filter(|i| i.node == target)
        .max_by_key(|i| {
            i.latest_step
                .as_deref()
                .and_then(|s| state.step(s))
                .map_or(0, |s| s.started_seq)
        })
}

fn node_output(
    state: &LoopState,
    loop_dir: &Path,
    node: &str,
    target: &str,
    iter: &[u32],
) -> String {
    let Some(step) = node_instance(state, node, target, iter)
        .and_then(|i| i.latest_step.as_deref())
        .and_then(|s| state.step(s))
    else {
        return String::new();
    };
    let text = match step.kind {
        NodeKind::Agent => step
            .output
            .as_ref()
            .filter(|b| is_safe_rel_path(&b.path))
            .and_then(|b| std::fs::read_to_string(loop_dir.join(&b.path)).ok())
            .unwrap_or_default(),
        NodeKind::Check => file_tail(
            &loop_dir.join(check_log_rel(&step.step_id)),
            MAX_NODE_OUTPUT_BYTES,
        )
        .unwrap_or_default(),
        NodeKind::Gate => state
            .gates
            .iter()
            .rev()
            .find(|g| g.step_id.as_deref() == Some(step.step_id.as_str()))
            .and_then(|g| g.resolution.as_ref())
            .map(|r| match &r.comment {
                Some(c) => format!("{}: {c}", r.option),
                None => r.option.clone(),
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    if text.len() > MAX_NODE_OUTPUT_BYTES {
        let mut cut = clamp_text(&text, MAX_NODE_OUTPUT_BYTES);
        cut.push_str("\n[… truncated]");
        return cut;
    }
    text
}

/// The full text an agent step is sent: the rendered task, then any feedback and
/// instructions the template did not place itself, then the verdict contract.
pub fn agent_prompt(
    state: &LoopState,
    loop_dir: &Path,
    node: &str,
    spec: &AgentSpec,
    iter: &[u32],
    instructions: &[&str],
) -> String {
    let mut text = render_template(&spec.task, |t| {
        resolve_token(state, loop_dir, node, iter, instructions, t)
    });
    let feedback = current_feedback(state, node, iter);
    if !feedback.is_empty() && !mentions(&spec.task, "feedback") {
        text.push_str("\n\n");
        text.push_str(&feedback_block(feedback));
    }
    if !instructions.is_empty() && !mentions(&spec.task, "instructions") {
        text.push_str("\n\n");
        text.push_str(&instructions_block(instructions));
    }
    if spec.verdict {
        text.push_str("\n\n");
        text.push_str(VERDICT_INSTRUCTION);
    }
    text
}

/// A gate's rendered prompt (`{{instructions}}` is not available to gates).
pub fn gate_prompt(
    state: &LoopState,
    loop_dir: &Path,
    node: &str,
    spec: &GateSpec,
    iter: &[u32],
) -> String {
    render_template(&spec.prompt, |t| {
        resolve_token(state, loop_dir, node, iter, &[], t)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_substitute_known_tokens_and_keep_the_rest() {
        let out = render_template("a {{ x }} b {{y}} c {{z", |t| {
            (t == "x").then(|| "X".to_string())
        });
        assert_eq!(out, "a X b {{y}} c {{z");
        assert_eq!(render_template("no braces", |_| None), "no braces");
        // A value containing braces is not re-expanded.
        assert_eq!(
            render_template("{{x}}{{x}}", |_| Some("{{x}}".into())),
            "{{x}}{{x}}"
        );
    }

    #[test]
    fn verdicts_are_read_from_the_last_line_only() {
        let v = parse_verdict("Looks good.\n\nVERDICT: PASS\n\n").unwrap();
        assert!(v.pass);
        assert_eq!(v.findings, None);
        let v =
            parse_verdict("1. parser panics on nesting\n2. no test\n**VERDICT: FAIL**").unwrap();
        assert!(!v.pass);
        assert_eq!(
            v.findings.as_deref(),
            Some("1. parser panics on nesting\n2. no test")
        );
        assert_eq!(
            parse_verdict("`verdict: fail`").map(|v| v.pass),
            Some(false)
        );
        // Mentioned earlier but not the last line: no verdict.
        assert_eq!(
            parse_verdict("VERDICT: PASS\nbut wait, one more thing"),
            None
        );
        assert_eq!(parse_verdict("VERDICT: MAYBE"), None);
        assert_eq!(parse_verdict(""), None);
    }

    #[test]
    fn blocks_are_empty_without_items() {
        assert_eq!(feedback_block(&[]), "");
        assert_eq!(instructions_block(&[]), "");
        let fb = feedback_block(&[FeedbackItem {
            source: FeedbackSource::Check,
            node: "test".into(),
            step_id: Some("test.i1.a1".into()),
            summary: "cargo test -q exited 101".into(),
            detail: Some("thread 'nested' panicked\nat src/parser.rs:131".into()),
        }]);
        assert!(
            fb.contains("[check test test.i1.a1] cargo test -q exited 101"),
            "{fb}"
        );
        assert!(fb.contains("    at src/parser.rs:131"), "{fb}");
        let ins = instructions_block(&["keep the API\nstable", "no new deps"]);
        assert_eq!(
            ins,
            "Instructions from the user (these take priority):\n- keep the API\n  stable\n- no new deps\n"
        );
    }

    #[test]
    fn tails_and_excerpts_respect_char_boundaries() {
        assert_eq!(tail_text("héllo", 4), "llo");
        assert_eq!(tail_text("abc", 10), "abc");
        assert_eq!(
            excerpt("\n\n  first  \nsecond\n").as_deref(),
            Some("first second")
        );
        assert_eq!(excerpt("   \n"), None);
    }
}

//! Feature extraction for task diagnosis.
//!
//! [`extract`] is a pure function: it reads a task string and returns a fresh
//! [`TaskFeatures`] describing surface signals (rough token count, code-fence presence,
//! imperative verbs, and matched category keywords). It mutates nothing and performs no
//! I/O or LLM calls — the heuristic layer ([`super::heuristic`]) consumes this output.

use serde::Serialize;

use crate::profile::TaskCategory;

/// Surface features of a task, extracted without any model call.
#[derive(Debug, Clone, Serialize)]
pub struct TaskFeatures {
    /// Rough token estimate (`len / 4`, ceil), mirroring `router::estimate_tokens`.
    pub token_estimate: u64,
    /// Whether the task contains a Markdown code fence (```` ``` ````).
    pub has_code_block: bool,
    /// Imperative/command verbs found as whole words, in first-seen order (deduped).
    pub verbs: Vec<String>,
    /// Category keywords found as substrings, paired with the category they signal.
    pub matched_keywords: Vec<(TaskCategory, &'static str)>,
}

/// Category keyword table. Matched as case-insensitive substrings of the whole task.
/// Order is stable so `matched_keywords` is deterministic for a given input.
const KEYWORDS: &[(TaskCategory, &str)] = &[
    // Refactor
    (TaskCategory::Refactor, "refactor"),
    (TaskCategory::Refactor, "rename"),
    (TaskCategory::Refactor, "deduplicate"),
    (TaskCategory::Refactor, "extract method"),
    (TaskCategory::Refactor, "simplify"),
    (TaskCategory::Refactor, "clean up"),
    (TaskCategory::Refactor, "restructure"),
    (TaskCategory::Refactor, "flatten"),
    // SecurityReview
    (TaskCategory::SecurityReview, "security"),
    (TaskCategory::SecurityReview, "vulnerab"),
    (TaskCategory::SecurityReview, "exploit"),
    (TaskCategory::SecurityReview, "injection"),
    (TaskCategory::SecurityReview, "owasp"),
    (TaskCategory::SecurityReview, "cwe"),
    (TaskCategory::SecurityReview, "audit"),
    // AdversarialReview
    (TaskCategory::AdversarialReview, "adversarial"),
    (TaskCategory::AdversarialReview, "red team"),
    (TaskCategory::AdversarialReview, "poke holes"),
    (TaskCategory::AdversarialReview, "devil's advocate"),
    (TaskCategory::AdversarialReview, "challenge assumption"),
    // Review
    (TaskCategory::Review, "code review"),
    (TaskCategory::Review, "review"),
    (TaskCategory::Review, "feedback"),
    (TaskCategory::Review, "critique"),
    // Debug
    (TaskCategory::Debug, "debug"),
    (TaskCategory::Debug, "stack trace"),
    (TaskCategory::Debug, "stacktrace"),
    (TaskCategory::Debug, "panic"),
    (TaskCategory::Debug, "crash"),
    (TaskCategory::Debug, "failing test"),
    (TaskCategory::Debug, "off-by-one"),
    // Coding
    (TaskCategory::Coding, "implement"),
    (TaskCategory::Coding, "function"),
    (TaskCategory::Coding, "feature"),
    (TaskCategory::Coding, "endpoint"),
    // Explain
    (TaskCategory::Explain, "explain"),
    (TaskCategory::Explain, "what does"),
    (TaskCategory::Explain, "how does"),
    (TaskCategory::Explain, "walk me through"),
    (TaskCategory::Explain, "understand"),
    // Docs
    (TaskCategory::Docs, "docstring"),
    (TaskCategory::Docs, "documentation"),
    (TaskCategory::Docs, "document"),
    (TaskCategory::Docs, "readme"),
    // Planning
    (TaskCategory::Planning, "roadmap"),
    (TaskCategory::Planning, "architecture"),
    (TaskCategory::Planning, "break down"),
    (TaskCategory::Planning, "decompose"),
    (TaskCategory::Planning, "milestone"),
    (TaskCategory::Planning, "plan"),
];

/// Imperative verbs and the category each one signals. Matched as whole words.
const VERBS: &[(&str, TaskCategory)] = &[
    ("implement", TaskCategory::Coding),
    ("write", TaskCategory::Coding),
    ("create", TaskCategory::Coding),
    ("build", TaskCategory::Coding),
    ("add", TaskCategory::Coding),
    ("refactor", TaskCategory::Refactor),
    ("rename", TaskCategory::Refactor),
    ("extract", TaskCategory::Refactor),
    ("simplify", TaskCategory::Refactor),
    ("flatten", TaskCategory::Refactor),
    ("restructure", TaskCategory::Refactor),
    ("review", TaskCategory::Review),
    ("audit", TaskCategory::SecurityReview),
    ("debug", TaskCategory::Debug),
    ("fix", TaskCategory::Debug),
    ("explain", TaskCategory::Explain),
    ("describe", TaskCategory::Explain),
    ("document", TaskCategory::Docs),
    ("plan", TaskCategory::Planning),
    ("design", TaskCategory::Planning),
];

/// Look up the category a verb signals, if it is a known command verb.
pub(super) fn verb_category(verb: &str) -> Option<TaskCategory> {
    VERBS.iter().find(|(v, _)| *v == verb).map(|(_, cat)| *cat)
}

/// Extract surface features from a task string. Pure: no mutation of inputs, no I/O.
pub fn extract(task: &str) -> TaskFeatures {
    let lower = task.to_ascii_lowercase();

    let token_estimate = task.len().div_ceil(4) as u64;
    let has_code_block = task.contains("```");

    let matched_keywords = KEYWORDS
        .iter()
        .filter(|(_, kw)| lower.contains(kw))
        .map(|(cat, kw)| (*cat, *kw))
        .collect();

    let verbs = extract_verbs(&lower);

    TaskFeatures {
        token_estimate,
        has_code_block,
        verbs,
        matched_keywords,
    }
}

/// Collect known command verbs appearing as whole words, first-seen order, deduped.
fn extract_verbs(lower: &str) -> Vec<String> {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty() && verb_category(word).is_some())
        .fold(Vec::new(), |mut acc, word| {
            if !acc.iter().any(|w: &String| w == word) {
                acc.push(word.to_string());
            }
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_matches_len_div_four_ceil() {
        assert_eq!(extract("").token_estimate, 0);
        assert_eq!(extract("abc").token_estimate, 1);
        assert_eq!(extract("abcd").token_estimate, 1);
        assert_eq!(extract("abcde").token_estimate, 2);
    }

    #[test]
    fn detects_code_fence() {
        assert!(extract("here:\n```rust\nfn main(){}\n```").has_code_block);
        assert!(!extract("no fence here").has_code_block);
    }

    #[test]
    fn extracts_verbs_as_whole_words_deduped() {
        let f = extract("refactor and then refactor again, please fix it");
        assert_eq!(f.verbs, vec!["refactor".to_string(), "fix".to_string()]);
    }

    #[test]
    fn does_not_match_verb_inside_a_larger_word() {
        // "addendum" contains "add" but is not the verb "add".
        let f = extract("read the addendum");
        assert!(f.verbs.is_empty());
    }

    #[test]
    fn matches_category_keywords() {
        let f = extract("please refactor this module");
        assert!(
            f.matched_keywords
                .iter()
                .any(|(c, _)| *c == TaskCategory::Refactor)
        );
    }
}

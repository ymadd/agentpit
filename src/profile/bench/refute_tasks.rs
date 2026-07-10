//! Stuck candidates for the ④ refute-quality gate (design §5.1, §4.6 crux #1).
//!
//! Each probe wraps an **existing** coding/debug gold task's id, prompt, and [`HiddenTests`]
//! fixture (so the inner grader is the same one `agentpit profile run` already exercises) with a
//! hand-authored `stuck` candidate: a real, locatable bug a competent critique can find and a
//! competent defense can fix — not an unrecoverable mess and not a trivial typo. [`refute_run`]
//! grades the `stuck` text directly (the "before" half) and, after a live critique→defense pass,
//! the defense's revised candidate against the same [`HiddenTests`] (the "after" half); the gate
//! is green only when "after" beats "before" by a margin across the set.
//!
//! Deliberately **not** part of [`super::suite::all_tasks`] — design §5.1 frames this as a
//! standalone go/no-go gate, not a profile capability column, and §4.6 crux #2 keeps the MVP set
//! small (three probes) rather than growing it like the seven complete-gold categories.

use crate::profile::category::TaskCategory;

use super::suite::{FixtureLang, GoldTask, Grading, HiddenTests};

fn refute_task(
    id: &str,
    category: TaskCategory,
    prompt: &str,
    stuck: &str,
    inner: Grading,
) -> GoldTask {
    GoldTask::new(
        id,
        category,
        prompt,
        Grading::Refute {
            stuck: stuck.to_string(),
            inner: Box::new(inner),
        },
    )
}

/// The MVP refute-quality probe set (design §4.6 crux #2: three is enough to start).
pub fn refute_probe_tasks() -> Vec<GoldTask> {
    vec![
        refute_binary_search_bounds(),
        refute_mutable_default_arg(),
        refute_parse_duration(),
    ]
}

/// Same task as `debug/binary_search_bounds`: a classic `lo < hi` (not `lo <= hi`) boundary bug.
/// It finds every target except the one at the final index — a single-line, easy-to-name defect a
/// critique should catch and a defense should fix by widening the loop condition.
fn refute_binary_search_bounds() -> GoldTask {
    refute_task(
        "refute/binary_search_bounds",
        TaskCategory::Debug,
        "The function `bsearch(xs, target)` returns wrong indices at the array boundaries. \
         Fix it so it returns the index of `target` or -1 if absent. Return only the corrected \
         Python module exposing `bsearch` in a final ```python fenced block.",
        "```python\n\
         def bsearch(xs, target):\n\
         \x20   lo, hi = 0, len(xs) - 1\n\
         \x20   while lo < hi:\n\
         \x20       mid = (lo + hi) // 2\n\
         \x20       if xs[mid] == target:\n\
         \x20           return mid\n\
         \x20       elif xs[mid] < target:\n\
         \x20           lo = mid + 1\n\
         \x20       else:\n\
         \x20           hi = mid - 1\n\
         \x20   return -1\n\
         ```\n",
        Grading::HiddenTests(HiddenTests {
            lang: FixtureLang::Python,
            source: "from solution import bsearch\n\n\
                     def test_found():\n\
                     \x20   assert bsearch([1,3,5,7,9], 7) == 3\n\
                     \x20   assert bsearch([1,3,5,7,9], 1) == 0\n\
                     \x20   assert bsearch([1,3,5,7,9], 9) == 4\n\n\
                     def test_absent():\n\
                     \x20   assert bsearch([1,3,5], 4) == -1\n\
                     \x20   assert bsearch([], 1) == -1\n"
                .to_string(),
        }),
    )
}

/// Same task as `debug/mutable_default_arg`: the canonical Python footgun. Every call after the
/// first leaks into the shared default list, so this scores a hard 0 — but the fix (`acc=None`
/// then `if acc is None: acc = []`) is one of the best-known idioms in the language, making it a
/// useful 0→1 probe for whether refute can recover from a total miss, not just a partial one.
fn refute_mutable_default_arg() -> GoldTask {
    refute_task(
        "refute/mutable_default_arg",
        TaskCategory::Debug,
        "`append_item(x, acc=[])` leaks state between calls because of a mutable default \
         argument. Fix it so each call starts fresh. Return only the corrected Python module \
         exposing `append_item` in a final ```python fenced block.",
        "```python\n\
         def append_item(x, acc=[]):\n\
         \x20   acc.append(x)\n\
         \x20   return acc\n\
         ```\n",
        Grading::HiddenTests(HiddenTests {
            lang: FixtureLang::Python,
            source: "from solution import append_item\n\n\
                     def test_no_state_leak():\n\
                     \x20   assert append_item(1) == [1]\n\
                     \x20   assert append_item(2) == [2]\n\
                     \x20   assert append_item(3) == [3]\n"
                .to_string(),
        }),
    )
}

/// Same task as `coding/parse_duration`: the regex matches only the *first* unit, so any
/// multi-unit string (the headline `"1h30m"` example) silently drops everything after it. A
/// single-unit string still works in isolation, but the sandbox grades `test_basic` as one
/// pytest item covering all three assertions, so the first failure (the multi-unit case) zeroes
/// the whole item — empirically a hard 0, like the other two probes, not partial credit.
fn refute_parse_duration() -> GoldTask {
    refute_task(
        "refute/parse_duration",
        TaskCategory::Coding,
        "Implement `parse_duration(s: str) -> int` that converts a duration string like \
         \"1h30m\", \"45s\", or \"2h\" into total seconds. An empty string is 0. Return only \
         a Python module exposing `parse_duration` in a final ```python fenced block.",
        "```python\n\
         import re\n\n\
         def parse_duration(s: str) -> int:\n\
         \x20   if s == \"\":\n\
         \x20       return 0\n\
         \x20   match = re.match(r\"(\\d+)([hms])\", s)\n\
         \x20   if not match:\n\
         \x20       return 0\n\
         \x20   value, unit = match.groups()\n\
         \x20   value = int(value)\n\
         \x20   if unit == \"h\":\n\
         \x20       return value * 3600\n\
         \x20   elif unit == \"m\":\n\
         \x20       return value * 60\n\
         \x20   else:\n\
         \x20       return value\n\
         ```\n",
        Grading::HiddenTests(HiddenTests {
            lang: FixtureLang::Python,
            source: "from solution import parse_duration\n\n\
                     def test_basic():\n\
                     \x20   assert parse_duration(\"1h30m\") == 5400\n\
                     \x20   assert parse_duration(\"45s\") == 45\n\
                     \x20   assert parse_duration(\"2h\") == 7200\n\n\
                     def test_empty():\n\
                     \x20   assert parse_duration(\"\") == 0\n"
                .to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::judge::{GradeOutcome, grade_refute_inner};
    use std::collections::BTreeSet;

    #[test]
    fn probe_ids_are_unique_and_namespaced() {
        let tasks = refute_probe_tasks();
        let unique: BTreeSet<_> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(unique.len(), tasks.len());
        for t in &tasks {
            assert!(t.id.starts_with("refute/"), "{} not namespaced", t.id);
        }
    }

    #[test]
    fn every_probe_carries_a_refute_grading_with_a_non_empty_stuck_candidate() {
        for t in refute_probe_tasks() {
            match &t.grading {
                Grading::Refute { stuck, .. } => assert!(!stuck.trim().is_empty(), "{}", t.id),
                other => panic!("{} graded as {other:?}, not Refute", t.id),
            }
        }
    }

    /// The empirical check the design's crux demands before trusting any live result: each
    /// `stuck` candidate must score *meaningfully below 1.0* against its own inner grader when
    /// graded as-is (the "before" half, offline, no network) — otherwise the probe cannot show a
    /// delta no matter how well refute performs. Requires a local `sandbox-exec`/jail; skips
    /// (rather than fails) when it is unavailable, matching every other sandbox-backed gold test.
    #[test]
    fn each_stuck_candidate_scores_meaningfully_below_one_offline() {
        for t in refute_probe_tasks() {
            let Grading::Refute { stuck, .. } = &t.grading else {
                panic!("{} is not a Refute task", t.id);
            };
            match grade_refute_inner(&t, stuck) {
                Some(GradeOutcome::Scored(score)) => {
                    assert!(
                        score < 0.9,
                        "{} stuck candidate scored too high: {score}",
                        t.id
                    );
                }
                Some(GradeOutcome::Skipped) => {
                    eprintln!(
                        "agentpit: sandbox unavailable — skipping stuck-score check for {}",
                        t.id
                    );
                }
                None => panic!("{} did not return a Refute grading", t.id),
            }
        }
    }
}

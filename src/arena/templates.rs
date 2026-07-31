//! Arena task templates — one probe per capability the matrix tracks.
//!
//! Writing a fresh arena task every time does not happen in practice, and an ad-hoc task lands
//! wherever the keyword heuristic decides it lands. That is the wrong failure mode for the arena:
//! its whole output is a human verdict, the scarcest signal agentpit can collect, and it should
//! not be spent on a cell nobody chose.
//!
//! So the set below is indexed by [`TaskCategory`] — **exactly one template per category, all ten
//! covered** — and every rendered task opens with a
//! [`CATEGORY:`](crate::diagnose::CATEGORY_MARKER) declaration. That marker is read by
//! [`diagnose`](crate::diagnose::diagnose) at [`DECLARED_CONFIDENCE`](crate::diagnose::DECLARED_CONFIDENCE),
//! above every routing gate, so the round's votes fold into the cell the template names rather
//! than one guessed from its prose. Running the ten templates fills the matrix's category axis by
//! construction; which backends you enter fills the other.
//!
//! **Every template produces a diff.** Half of these categories — review, explain, planning —
//! naturally end in prose, and an arena submission with an empty patch is not judgeable (it would
//! hand its opponent a free win). Each such template therefore names an output FILE, so the
//! deliverable arrives as a diff like any other and the blind comparison stays uniform.
//!
//! **Targets are pinned, not discovered.** A template that told each contender to "find something
//! worth refactoring" would have them refactor different files, and comparing those answers no
//! question. Where a subject is needed the caller supplies it, so every contender works the same
//! problem.

use crate::profile::TaskCategory;

/// One reusable arena probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaTemplate {
    /// Stable id for `--template`, namespaced by category.
    pub id: &'static str,
    /// The matrix cell this probe's votes land in.
    pub category: TaskCategory,
    /// What the probe is actually testing — the reason to spend a round on it.
    pub probes: &'static str,
    /// What `--target` means here, or `None` when the template needs no subject. The wording is
    /// shown verbatim when a target is missing, so the error tells the user what to pass.
    pub target: Option<&'static str>,
    /// Body with `{target}` substituted. The `CATEGORY:` line is prepended by [`render`].
    body: &'static str,
}

impl ArenaTemplate {
    /// The full task text: the category declaration, then the body with `{target}` filled in.
    ///
    /// Errors when the template needs a subject and none was given — silently running a
    /// refactor probe with a literal `{target}` in it would burn N agentic runs on nonsense.
    pub fn render(&self, target: Option<&str>) -> Result<String, String> {
        let body = match (self.target, target) {
            (Some(what), None) => {
                return Err(format!("template '{}' needs --target: {what}", self.id));
            }
            (Some(_), Some(t)) => self.body.replace("{target}", t),
            (None, _) => self.body.to_string(),
        };
        // `body` is a plain constant, not a format string, so every placeholder has to be
        // substituted here. `{DELIVER}` shipped as literal text on the first cut precisely
        // because a `const` looks like it interpolates and does not.
        let body = body.replace("{DELIVER}", DELIVER);
        Ok(format!(
            "{} {}\n{body}",
            crate::diagnose::CATEGORY_MARKER,
            self.category.as_str()
        ))
    }
}

/// A shared closing instruction, substituted into `{DELIVER}` by [`ArenaTemplate::render`].
/// Contenders are otherwise free to interpret "done" differently — one writes the file, another
/// explains what it would have written — and that difference is not a capability difference
/// worth voting on.
const DELIVER: &str = "Write the deliverable to the file named above, creating it if needed. Do \
                       not modify anything else. Keep it self-contained: it will be read on its \
                       own, without your reply.";

pub const ALL: &[ArenaTemplate] = &[
    ArenaTemplate {
        id: "coding/new-unit",
        category: TaskCategory::Coding,
        probes: "writing new code to a fixed spec, in the project's own language and conventions",
        target: Some("the path of the file to create, e.g. src/duration.rs"),
        body: "Create {target} implementing a function `parse_duration` that turns a duration \
               string into a whole number of seconds. It accepts a sequence of `<number><unit>` \
               segments — `h`, `m`, `s` — in any order, e.g. `1h30m`, `90s`, `2h`, `45m15s`. \
               Reject empty input, unknown units, a missing number, and a segment repeated twice. \
               Use this project's existing language, error-handling style, and test conventions, \
               and add tests for both the accepted and the rejected cases.",
    },
    ArenaTemplate {
        id: "refactor/untangle",
        category: TaskCategory::Refactor,
        probes: "improving structure with behaviour held fixed — the discipline half of refactoring",
        target: Some("the file to refactor, e.g. src/router.rs"),
        body: "Refactor {target} to reduce duplication and nesting. Behaviour must not change: \
               the public API, its observable outputs, and every existing test stay exactly as \
               they are. Do not add features, do not rename public items, and do not touch any \
               other file. If the existing tests do not already pin the behaviour you are moving, \
               add the missing test FIRST, in the same file's usual test location.",
    },
    ArenaTemplate {
        id: "review/find-defects",
        category: TaskCategory::Review,
        probes: "finding real defects, and — just as much — not inventing ones that are not there",
        target: Some("the file to review, e.g. src/dispatch.rs"),
        body: "Review {target} for defects and write your findings to REVIEW.md. For each finding \
               give the line, what breaks, and the concrete input or state that triggers it. \
               Order by severity. Do NOT modify the reviewed file — the report is the whole \
               deliverable. If you believe the file is sound, say so and list what you checked; \
               a short honest report beats a padded one.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "adversarialreview/trust-nothing",
        category: TaskCategory::AdversarialReview,
        probes: "reading against the code's own claims, where comments and names are evidence of \
                 intent rather than of behaviour",
        target: Some("the file to audit, e.g. src/profile/bench/score.rs"),
        body: "Audit {target} on the assumption that its comments, names, and docstrings may be \
               wrong. They describe what someone intended, not what the code does. Write \
               FINDINGS.md listing, for each place they disagree, what the code actually does and \
               what the surrounding text claims. Then add a second section listing what you \
               checked and found to be CORRECT — a claim you verified is a result, and an audit \
               that reports only hits cannot be told apart from one that guessed.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "securityreview/owasp-pass",
        category: TaskCategory::SecurityReview,
        probes: "identifying vulnerabilities by class rather than by vibe, and resisting \
                 plausible-looking non-issues",
        target: Some("the file or directory to audit, e.g. src/mcp/"),
        body: "Perform a security review of {target} and write SECURITY-REVIEW.md. For each \
               issue give the location, the CWE id, the attack that exploits it, and the smallest \
               fix. Cover at least: injection, path traversal, unsafe deserialization, secret \
               handling, TOCTOU and other race conditions, and missing authorization. Do not \
               modify the audited code. Report only what you can trace to a concrete attack — an \
               issue you cannot exploit belongs in a separate 'not exploitable here' \
               section.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "debug/root-cause",
        category: TaskCategory::Debug,
        probes: "reaching the actual cause instead of the first change that makes the symptom stop",
        target: Some(
            "the failing test, command, or observed symptom, e.g. \
                      'cargo test router::tests::picks_cheapest fails'",
        ),
        body: "Reproduce and fix this failure: {target}\n\nWork in this order and show it: \
               reproduce it first, identify the root cause, then make the smallest change that \
               fixes that cause. Do not weaken, skip, or special-case any assertion to make the \
               test pass. Add a regression test that fails before your fix and passes after. \
               Write a short DEBUG-NOTES.md giving the reproduction, the root cause, and why your \
               change is the smallest one that addresses it.",
    },
    ArenaTemplate {
        id: "explain/how-it-works",
        category: TaskCategory::Explain,
        probes: "explaining WHY the code is shaped this way, not restating what it says",
        target: Some("the file or module to explain, e.g. src/profile/mod.rs"),
        body: "Explain {target} in EXPLANATION.md for an engineer who is competent but new to \
               this codebase. Cover: what it is responsible for, the invariants it maintains, how \
               data flows through it, and — most important — the decisions that are not obvious \
               from reading it, with the reason each one is the way it is. Do not paraphrase the \
               code line by line; if a sentence would be redundant next to the code it describes, \
               cut it.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "docs/user-facing",
        category: TaskCategory::Docs,
        probes: "writing for someone who wants to USE the thing, from the outside in",
        target: Some("the feature, command, or module to document, e.g. 'agentpit arena'"),
        body: "Write user-facing documentation for {target} into DOCS.md. Lead with what it is \
               for and when to reach for it, then the shortest example that actually works, then \
               the options, then the failure modes and what to do about each. Read the real code \
               before writing — every flag, default, and output shape you state must match it. \
               Write for a user, not a maintainer: internal structure belongs in it only where it \
               changes what the user should do.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "planning/implementation-plan",
        category: TaskCategory::Planning,
        probes: "sequencing real work under real constraints, including naming what could go wrong",
        target: Some("the goal to plan, e.g. 'add a --dry-run flag to every write path'"),
        body: "Produce an implementation plan for this goal, in PLAN.md: {target}\n\nRead enough \
               of the codebase to make it specific — name the actual files and functions each \
               step touches. Give the steps in an order where each one leaves the tree working, \
               say what you verify after each, and call out the risks and the decisions someone \
               would have to make along the way. Write no implementation code; the plan is the \
               deliverable. A plan that could have been written without reading this repository \
               is a failed plan.\n\n{DELIVER}",
    },
    ArenaTemplate {
        id: "longcontext/subtree-inventory",
        category: TaskCategory::LongContext,
        probes: "holding a whole subtree at once — findings that require having read all of it, \
                 not any one file",
        target: Some("the directory to inventory, e.g. src/"),
        body: "Read every source file under {target} and write INVENTORY.md. It must contain: \
               (1) each public entry point and the one line that says what it is for; (2) which \
               of them nothing else in the subtree calls; (3) where the real module boundaries \
               are, as opposed to where the directory structure suggests they are; (4) any two \
               places that implement the same idea differently. Points 2 through 4 cannot be \
               answered from a single file — they are the point of the exercise. State how many \
               files you read.\n\n{DELIVER}",
    },
];

/// Look one up by id.
pub fn find(id: &str) -> Option<&'static ArenaTemplate> {
    ALL.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::{DiagnoseMethod, diagnose};

    #[test]
    fn every_matrix_category_has_exactly_one_template() {
        for category in TaskCategory::ALL {
            let found: Vec<_> = ALL.iter().filter(|t| t.category == *category).collect();
            assert_eq!(
                found.len(),
                1,
                "{category} has {} template(s); the set is meant to cover the matrix exactly once",
                found.len()
            );
        }
        assert_eq!(ALL.len(), TaskCategory::ALL.len());
    }

    #[test]
    fn ids_are_unique_and_namespaced_by_their_category() {
        let mut seen = std::collections::BTreeSet::new();
        for t in ALL {
            assert!(seen.insert(t.id), "duplicate template id {}", t.id);
            let (prefix, _) = t.id.split_once('/').expect("id is <category>/<name>");
            assert_eq!(
                prefix,
                t.category.as_str(),
                "{} is namespaced under the wrong category",
                t.id
            );
        }
    }

    /// The load-bearing property: a rendered template lands in the cell it names, decided by the
    /// declaration rather than re-guessed from its prose. Without this the arena's votes would
    /// scatter across the matrix — several of these bodies are written in language the keyword
    /// heuristic would read as another category entirely.
    #[test]
    fn a_rendered_template_diagnoses_back_to_its_own_category() {
        for t in ALL {
            let task = t.render(Some("src/example.rs")).unwrap();
            let d = diagnose(&task);
            assert_eq!(
                d.primary, t.category,
                "{} rendered into a task that diagnoses as {}",
                t.id, d.primary
            );
            assert_eq!(d.method, DiagnoseMethod::Declared, "{}", t.id);
            assert!(d.confidence >= crate::diagnose::LLM_ASSIST_CONFIDENCE_THRESHOLD);
        }
    }

    #[test]
    fn the_target_is_substituted_and_never_left_as_a_placeholder() {
        for t in ALL {
            let rendered = t.render(Some("src/thing.rs")).unwrap();
            assert!(
                !rendered.contains("{target}"),
                "{} left an unsubstituted placeholder",
                t.id
            );
            if t.target.is_some() {
                assert!(
                    rendered.contains("src/thing.rs"),
                    "{} ignored its target",
                    t.id
                );
            }
        }
    }

    #[test]
    fn a_template_that_needs_a_subject_refuses_to_run_without_one() {
        for t in ALL.iter().filter(|t| t.target.is_some()) {
            let err = t.render(None).unwrap_err();
            assert!(err.contains(t.id), "{err}");
            // The error has to say what to pass, not just that something is missing.
            assert!(err.contains(t.target.unwrap()), "{err}");
        }
    }

    /// A prose category with no named output file would produce an empty patch, which the arena
    /// treats as a non-submission — the round would silently have nothing to compare.
    #[test]
    fn every_prose_category_names_an_output_file() {
        let prose = [
            TaskCategory::Review,
            TaskCategory::AdversarialReview,
            TaskCategory::SecurityReview,
            TaskCategory::Explain,
            TaskCategory::Docs,
            TaskCategory::Planning,
            TaskCategory::LongContext,
        ];
        for t in ALL.iter().filter(|t| prose.contains(&t.category)) {
            let rendered = t.render(Some("x")).unwrap();
            assert!(
                rendered.contains(".md"),
                "{} must name a deliverable file, else it produces no diff to judge",
                t.id
            );
        }
    }

    #[test]
    fn find_resolves_ids_and_rejects_unknown_ones() {
        assert_eq!(
            find("review/find-defects").unwrap().category,
            TaskCategory::Review
        );
        assert!(find("review/nope").is_none());
    }
}

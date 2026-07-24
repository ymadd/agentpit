//! Static gold-task definitions for the bench suite (design §2.2).
//!
//! The data half of the suite: one builder per probe across the seven complete-gold categories,
//! aggregated by [`all_tasks`]. The types these construct — [`GoldTask`], [`Grading`] and its
//! grading-metadata payloads — live in [`suite`](super::suite); this module only fills them with
//! literals. Pure and immutable: every builder returns a fresh value.

use crate::profile::category::TaskCategory;

use super::suite::{
    AdversarialItem, AdversarialKind, Defect, FixtureLang, GoldTask, Grading, HiddenTests, Needle,
    RefactorGrading, SecurityDefect,
};

/// A hidden-test fixture in one line.
fn hidden(lang: FixtureLang, source: &str) -> Grading {
    Grading::HiddenTests(HiddenTests {
        lang,
        source: source.to_string(),
    })
}

/// Every gold task across the seven complete-gold categories, in category order. Pure: builds a
/// fresh `Vec` from the per-category builders and returns it.
pub fn all_tasks() -> Vec<GoldTask> {
    [
        coding_tasks(),
        debug_tasks(),
        refactor_tasks(),
        review_tasks(),
        security_review_tasks(),
        adversarial_tasks(),
        long_context_tasks(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Coding probes (design §2.2: parse_duration / Top-K frequency / RLE).
fn coding_tasks() -> Vec<GoldTask> {
    vec![
        coding_parse_duration(),
        coding_top_k_frequent(),
        coding_run_length_encode(),
    ]
}

/// Coding probe: `parse_duration` string→seconds.
fn coding_parse_duration() -> GoldTask {
    GoldTask::new(
        "coding/parse_duration",
        TaskCategory::Coding,
        "Implement `parse_duration(s: str) -> int` that converts a duration string like \
         \"1h30m\", \"45s\", or \"2h\" into total seconds. An empty string is 0. Return only \
         a Python module exposing `parse_duration` in a final ```python fenced block.",
        hidden(
            FixtureLang::Python,
            "from solution import parse_duration\n\n\
             def test_basic():\n\
             \x20   assert parse_duration(\"1h30m\") == 5400\n\
             \x20   assert parse_duration(\"45s\") == 45\n\
             \x20   assert parse_duration(\"2h\") == 7200\n\n\
             def test_empty():\n\
             \x20   assert parse_duration(\"\") == 0\n",
        ),
    )
}

/// Coding probe: `top_k_frequent` words by frequency.
fn coding_top_k_frequent() -> GoldTask {
    GoldTask::new(
        "coding/top_k_frequent",
        TaskCategory::Coding,
        "Implement `top_k_frequent(words: list[str], k: int) -> list[str]` returning the k \
         most frequent words, most-frequent first, ties broken lexicographically. Return only \
         a Python module exposing `top_k_frequent` in a final ```python fenced block.",
        hidden(
            FixtureLang::Python,
            "from solution import top_k_frequent\n\n\
             def test_top_k():\n\
             \x20   assert top_k_frequent([\"a\",\"b\",\"a\",\"c\",\"b\",\"a\"], 2) == [\"a\",\"b\"]\n\
             \x20   assert top_k_frequent([\"x\"], 1) == [\"x\"]\n\
             \x20   assert top_k_frequent([\"z\",\"y\",\"z\",\"y\"], 2) == [\"y\",\"z\"]\n",
        ),
    )
}

/// Coding probe: run-length `rle_encode`/`rle_decode` round-trip.
fn coding_run_length_encode() -> GoldTask {
    GoldTask::new(
        "coding/run_length_encode",
        TaskCategory::Coding,
        "Implement `rle_encode(&str) -> String` and `rle_decode(&str) -> String` such that \
         encode turns \"aaabbc\" into \"a3b2c1\" and decode is its inverse. Return only a Rust \
         module exposing both in a final ```rust fenced block.",
        hidden(
            FixtureLang::Rust,
            "use solution::{rle_encode, rle_decode};\n\n\
             #[test]\n\
             fn roundtrip() {\n\
             \x20   assert_eq!(rle_encode(\"aaabbc\"), \"a3b2c1\");\n\
             \x20   assert_eq!(rle_decode(\"a3b2c1\"), \"aaabbc\");\n\
             \x20   assert_eq!(rle_encode(\"\"), \"\");\n\
             }\n",
        ),
    )
}

/// Debug probes (design §2.2: binary-search boundary / mutable default arg / inclusive off-by-one).
fn debug_tasks() -> Vec<GoldTask> {
    vec![
        debug_binary_search_bounds(),
        debug_mutable_default_arg(),
        debug_inclusive_off_by_one(),
    ]
}

/// Debug probe: binary-search boundary indices.
fn debug_binary_search_bounds() -> GoldTask {
    GoldTask::new(
        "debug/binary_search_bounds",
        TaskCategory::Debug,
        "The function `bsearch(xs, target)` returns wrong indices at the array boundaries. \
         Fix it so it returns the index of `target` or -1 if absent. Return only the corrected \
         Python module exposing `bsearch` in a final ```python fenced block.",
        hidden(
            FixtureLang::Python,
            "from solution import bsearch\n\n\
             def test_found():\n\
             \x20   assert bsearch([1,3,5,7,9], 7) == 3\n\
             \x20   assert bsearch([1,3,5,7,9], 1) == 0\n\
             \x20   assert bsearch([1,3,5,7,9], 9) == 4\n\n\
             def test_absent():\n\
             \x20   assert bsearch([1,3,5], 4) == -1\n\
             \x20   assert bsearch([], 1) == -1\n",
        ),
    )
}

/// Debug probe: mutable-default-argument state leak.
fn debug_mutable_default_arg() -> GoldTask {
    GoldTask::new(
        "debug/mutable_default_arg",
        TaskCategory::Debug,
        "`append_item(x, acc=[])` leaks state between calls because of a mutable default \
         argument. Fix it so each call starts fresh. Return only the corrected Python module \
         exposing `append_item` in a final ```python fenced block.",
        hidden(
            FixtureLang::Python,
            "from solution import append_item\n\n\
             def test_no_state_leak():\n\
             \x20   assert append_item(1) == [1]\n\
             \x20   assert append_item(2) == [2]\n\
             \x20   assert append_item(3) == [3]\n",
        ),
    )
}

/// Debug probe: inclusive off-by-one (and overflow) in `sum_range`.
fn debug_inclusive_off_by_one() -> GoldTask {
    GoldTask::new(
        "debug/inclusive_off_by_one",
        TaskCategory::Debug,
        "`sum_range(lo, hi)` should sum the inclusive range lo..=hi without overflowing on \
         large bounds, but it is off by one (and overflows). Fix it. Return only the corrected \
         Rust module exposing `sum_range(u64, u64) -> u64` in a final ```rust fenced block.",
        hidden(
            FixtureLang::Rust,
            "use solution::sum_range;\n\n\
             #[test]\n\
             fn inclusive_bounds() {\n\
             \x20   assert_eq!(sum_range(1, 5), 15);\n\
             \x20   assert_eq!(sum_range(0, 0), 0);\n\
             \x20   assert_eq!(sum_range(10, 10), 10);\n\
             }\n",
        ),
    )
}

/// Refactor probes (design §2.2: de-duplicate / flatten nesting / O(N²)→O(N)). Each carries the
/// behaviour-equivalence hard gate plus complexity/LOC baselines.
fn refactor_tasks() -> Vec<GoldTask> {
    vec![
        refactor_dedupe_discount(),
        refactor_flatten_classify(),
        refactor_dedupe_linear_scan(),
    ]
}

/// Refactor probe: de-duplicate the discount arithmetic in `total_price`.
fn refactor_dedupe_discount() -> GoldTask {
    GoldTask::new(
        "refactor/dedupe_discount",
        TaskCategory::Refactor,
        "Refactor `total_price(amount, tier)` to remove the duplicated discount arithmetic \
         across tiers without changing behaviour. Return only the refactored Python module in \
         a final ```python fenced block.",
        Grading::Refactor(RefactorGrading {
            behavior_test: HiddenTests {
                lang: FixtureLang::Python,
                source: "from solution import total_price\n\n\
                         def test_behavior_equivalence():\n\
                         \x20   assert total_price(100, \"gold\") == 80\n\
                         \x20   assert total_price(100, \"silver\") == 90\n\
                         \x20   assert total_price(100, \"none\") == 100\n"
                    .to_string(),
            },
            complexity_baseline: Some(4),
            loc_baseline: Some(12),
        }),
    )
}

/// Refactor probe: flatten nested conditionals in `classify`.
fn refactor_flatten_classify() -> GoldTask {
    GoldTask::new(
        "refactor/flatten_classify",
        TaskCategory::Refactor,
        "Flatten the deeply nested conditionals in `classify(n)` using early returns or a \
         table, preserving behaviour. Return only the refactored Python module in a final \
         ```python fenced block.",
        Grading::Refactor(RefactorGrading {
            behavior_test: HiddenTests {
                lang: FixtureLang::Python,
                source: "from solution import classify\n\n\
                         def test_behavior_equivalence():\n\
                         \x20   assert classify(5) == \"low\"\n\
                         \x20   assert classify(50) == \"mid\"\n\
                         \x20   assert classify(500) == \"high\"\n"
                    .to_string(),
            },
            complexity_baseline: Some(5),
            loc_baseline: None,
        }),
    )
}

/// Refactor probe: O(N²)→O(N) rewrite of `has_duplicate`.
fn refactor_dedupe_linear_scan() -> GoldTask {
    GoldTask::new(
        "refactor/dedupe_linear_scan",
        TaskCategory::Refactor,
        "Rewrite `has_duplicate(xs)` from its O(N²) nested scan to an O(N) single pass with a \
         set, preserving behaviour. Return only the refactored Rust module exposing \
         `has_duplicate(&[i64]) -> bool` in a final ```rust fenced block.",
        Grading::Refactor(RefactorGrading {
            behavior_test: HiddenTests {
                lang: FixtureLang::Rust,
                source: "use solution::has_duplicate;\n\n\
                         #[test]\n\
                         fn behavior_equivalence() {\n\
                         \x20   assert!(has_duplicate(&[1, 2, 3, 2]));\n\
                         \x20   assert!(!has_duplicate(&[1, 2, 3, 4]));\n\
                         \x20   assert!(!has_duplicate(&[]));\n\
                         }\n"
                .to_string(),
            },
            complexity_baseline: Some(3),
            loc_baseline: Some(8),
        }),
    )
}

/// Plain-review probes (design §2.2: known API-handler bug / spec violation / noise resistance).
/// The noise-resistance task has an empty defect list — the only correct answer is `[]`.
fn review_tasks() -> Vec<GoldTask> {
    vec![
        GoldTask::new(
            "review/api_handler_bug",
            TaskCategory::Review,
            "Review this request handler and report defects as a final JSON array of \
             {line, kind} objects. If there are none, return []:\n```python\n\
             1  def get_user(req, db):\n\
             2      uid = req.args.get(\"id\")\n\
             3      row = db.query(uid)\n\
             4      return {\"name\": row.name}\n```",
            Grading::Review {
                defects: vec![Defect {
                    line: 4,
                    kind: "missing-null-check".to_string(),
                    cwe: Some("CWE-476".to_string()),
                }],
            },
        ),
        GoldTask::new(
            "review/spec_violation",
            TaskCategory::Review,
            "The spec says `withdraw(balance, amount)` must reject overdrafts. Review the code and \
             report defects as a final JSON array of {line, kind} objects. If there are none, \
             return []:\n```python\n\
             1  def withdraw(balance, amount):\n\
             2      balance -= amount\n\
             3      return balance\n```",
            Grading::Review {
                defects: vec![Defect {
                    line: 2,
                    kind: "missing-overdraft-guard".to_string(),
                    cwe: None,
                }],
            },
        ),
        GoldTask::new(
            "review/clean_no_defects",
            TaskCategory::Review,
            "Review this function for defects and report them as a final JSON array of \
             {line, kind} objects. If there are none, return []:\n```python\n\
             1  def add(a, b):\n\
             2      return a + b\n```",
            Grading::Review { defects: vec![] },
        ),
    ]
}

/// Security-review probes (design §2.2: injection set with CWEs / auth-secret set / FP resistance).
/// Every `SecurityDefect` carries a required CWE-id; the FP-resistance task has an empty list.
fn security_review_tasks() -> Vec<GoldTask> {
    vec![
        securityreview_injection_set(),
        securityreview_auth_secret_set(),
        securityreview_false_positive_resistance(),
    ]
}

/// Security-review probe: three injection defects (SQLi / command / path traversal).
fn securityreview_injection_set() -> GoldTask {
    GoldTask::new(
        "securityreview/injection_set",
        TaskCategory::SecurityReview,
        "Audit this code and report security defects as a final JSON array of \
         {line, kind, cwe} objects. If there are none, return []:\n```python\n\
         1  def search(db, q):\n\
         2      return db.execute(\"SELECT * FROM t WHERE n='\" + q + \"'\")\n\
         3  def run(cmd):\n\
         4      os.system(\"ping \" + cmd)\n\
         5  def load(path):\n\
         6      return open(\"/data/\" + path).read()\n```",
        Grading::SecurityReview {
            defects: vec![
                SecurityDefect {
                    line: 2,
                    kind: "sql-injection".to_string(),
                    cwe: "CWE-89".to_string(),
                    severity: Some("high".to_string()),
                },
                SecurityDefect {
                    line: 4,
                    kind: "command-injection".to_string(),
                    cwe: "CWE-78".to_string(),
                    severity: Some("high".to_string()),
                },
                SecurityDefect {
                    line: 6,
                    kind: "path-traversal".to_string(),
                    cwe: "CWE-22".to_string(),
                    severity: None,
                },
            ],
        },
    )
}

/// Security-review probe: hardcoded secret plus missing authorization.
fn securityreview_auth_secret_set() -> GoldTask {
    GoldTask::new(
        "securityreview/auth_secret_set",
        TaskCategory::SecurityReview,
        "Audit this code and report security defects as a final JSON array of \
         {line, kind, cwe} objects. If there are none, return []:\n```python\n\
         1  API_KEY = \"sk-live-9f3a2b\"\n\
         2  def admin(req):\n\
         3      if req.user:\n\
         4          return delete_all()\n```",
        Grading::SecurityReview {
            defects: vec![
                SecurityDefect {
                    line: 1,
                    kind: "hardcoded-secret".to_string(),
                    cwe: "CWE-798".to_string(),
                    severity: Some("high".to_string()),
                },
                SecurityDefect {
                    line: 3,
                    kind: "missing-authorization".to_string(),
                    cwe: "CWE-862".to_string(),
                    severity: Some("high".to_string()),
                },
            ],
        },
    )
}

/// Security-review probe: parameterised query — the only correct answer is no defects.
fn securityreview_false_positive_resistance() -> GoldTask {
    GoldTask::new(
        "securityreview/false_positive_resistance",
        TaskCategory::SecurityReview,
        "Audit this code and report security defects as a final JSON array of \
         {line, kind, cwe} objects. If there are none, return []:\n```python\n\
         1  def search(db, q):\n\
         2      return db.execute(\"SELECT * FROM t WHERE n=?\", (q,))\n```",
        Grading::SecurityReview { defects: vec![] },
    )
}

/// Adversarial-review probes (design §2.2: lying-comment resistance / subtle defect + decoy /
/// spec violation under passing tests). Decoys carry [`AdversarialKind::Decoy`].
fn adversarial_tasks() -> Vec<GoldTask> {
    vec![adversarial_lying_comment(), adversarial_subtle_plus_decoy()]
}

/// Adversarial probe: real defect under a "validated and safe" lying comment (the comment is a decoy).
fn adversarial_lying_comment() -> GoldTask {
    GoldTask::new(
        "adversarialreview/lying_comment",
        TaskCategory::AdversarialReview,
        "Review this code. A comment claims correctness — do not trust comments. Report real \
         defects as a final JSON array of {line, kind} objects. If there are none, return \
         []:\n```python\n\
         1  # validated and safe\n\
         2  def transfer(a, b, amt):\n\
         3      a.bal -= amt\n\
         4      b.bal += amt\n```",
        Grading::Adversarial {
            items: vec![
                AdversarialItem {
                    line: 3,
                    kind: "missing-amount-validation".to_string(),
                    cwe: Some("CWE-20".to_string()),
                    item: AdversarialKind::Real,
                },
                AdversarialItem {
                    line: 1,
                    kind: "comment-claims-safe".to_string(),
                    cwe: None,
                    item: AdversarialKind::Decoy,
                },
            ],
        },
    )
}

/// Adversarial probe: a subtle division-by-zero next to a correct-but-suspicious clamp (the decoy).
fn adversarial_subtle_plus_decoy() -> GoldTask {
    GoldTask::new(
        "adversarialreview/subtle_plus_decoy",
        TaskCategory::AdversarialReview,
        "Review this code and report real defects as a final JSON array of {line, kind} \
         objects. Some lines look suspicious but are correct. If there are none, return \
         []:\n```python\n\
         1  def avg(xs):\n\
         2      return sum(xs) / len(xs)\n\
         3  def clamp(x):\n\
         4      return max(0, min(100, x))\n```",
        Grading::Adversarial {
            items: vec![
                AdversarialItem {
                    line: 2,
                    kind: "division-by-zero".to_string(),
                    cwe: Some("CWE-369".to_string()),
                    item: AdversarialKind::Real,
                },
                AdversarialItem {
                    line: 4,
                    kind: "suspicious-clamp".to_string(),
                    cwe: None,
                    item: AdversarialKind::Decoy,
                },
            ],
        },
    )
}

/// Long-context probes (design §2.2: needle extraction / set-then-win / lost-in-the-middle).
/// All graded by exact string match — no LLM judge. The filler is real: each probe carries
/// hundreds of generated distractor lines, so a backend that cannot actually hold the
/// document cannot answer by pattern-matching a five-line prompt.
fn long_context_tasks() -> Vec<GoldTask> {
    vec![
        GoldTask::new(
            "longcontext/needle_extraction",
            TaskCategory::LongContext,
            &format!(
                "Among the configuration lines below, find the deploy region. Answer with \
                 only the value.\n{}\ndeploy.region = ap-northeast-1\n{}",
                filler_config_lines(0, 220),
                filler_config_lines(220, 220),
            ),
            Grading::LongContext {
                needles: vec![Needle {
                    needle: "deploy.region".to_string(),
                    expected: "ap-northeast-1".to_string(),
                }],
            },
        ),
        GoldTask::new(
            "longcontext/set_then_win",
            TaskCategory::LongContext,
            &format!(
                "The document below sets `request.timeout` several times; the last assignment \
                 wins. What is the effective timeout? Answer with only the number.\n\
                 {}\nrequest.timeout = 10\n{}\nrequest.timeout = 30\n{}\nrequest.timeout = 45\n{}",
                filler_config_lines(500, 150),
                filler_config_lines(650, 150),
                filler_config_lines(800, 150),
                filler_config_lines(950, 60),
            ),
            Grading::LongContext {
                needles: vec![Needle {
                    needle: "effective timeout".to_string(),
                    expected: "45".to_string(),
                }],
            },
        ),
        GoldTask::new(
            "longcontext/lost_in_the_middle",
            TaskCategory::LongContext,
            &format!(
                "A single fact is buried in the middle of the log below. What is the launch \
                 code? Answer with only the code.\n{}\nThe launch code is THX-1138.\n{}",
                filler_log_lines(0, 300),
                filler_log_lines(300, 300),
            ),
            Grading::LongContext {
                needles: vec![Needle {
                    needle: "launch code".to_string(),
                    expected: "THX-1138".to_string(),
                }],
            },
        ),
    ]
}

/// Deterministic config-style distractor lines (`svc_NNNN.<key> = <value>`). Values are derived
/// from the line index so the text never repeats a needle value and stays stable across builds
/// (grading is exact-match; the filler must not collide with `ap-northeast-1`, `45`, or the
/// launch code as a *value of the probed key*).
fn filler_config_lines(start: usize, count: usize) -> String {
    let keys = [
        "pool_size",
        "retry_limit",
        "cache_ttl",
        "batch_window",
        "max_inflight",
    ];
    (start..start + count)
        .map(|i| {
            let key = keys[i % keys.len()];
            format!("svc_{i:04}.{key} = {}", 1000 + i * 7 % 8999)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Deterministic log-style distractor lines for the lost-in-the-middle probe.
fn filler_log_lines(start: usize, count: usize) -> String {
    let events = [
        "healthcheck ok",
        "cache warmed",
        "queue drained",
        "lease renewed",
        "gc pass done",
    ];
    (start..start + count)
        .map(|i| {
            format!(
                "2026-07-0{} 12:{:02}:{:02} worker-{} {}",
                i % 9 + 1,
                i / 60 % 60,
                i % 60,
                i % 16,
                events[i % events.len()],
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding (2026-07 eval): long-context probes were five-line prompts with
    /// "...many lines..." placeholders, so they measured nothing about context capacity.
    #[test]
    fn long_context_prompts_carry_hundreds_of_real_lines() {
        for task in long_context_tasks() {
            let lines = task.prompt.lines().count();
            assert!(
                lines > 400,
                "{} has only {lines} lines — not a long-context probe",
                task.id
            );
            assert!(
                !task.prompt.contains("..."),
                "{} still contains a placeholder ellipsis",
                task.id
            );
        }
    }

    /// Review finding (2026-07 eval): several review/security/adversarial prompts leaked
    /// the expected answer (a parenthetical naming the defect, inline comments annotating
    /// the buggy line, or "already parameterised" giving away the clean verdict).
    #[test]
    fn grading_prompts_do_not_leak_their_expected_defects() {
        for task in all_tasks() {
            let leaks: &[&str] = &[
                "dereferences",
                "may be None",
                "already parameterised",
                "no check that",
                "ZeroDivisionError",
                "idiomatic, correct",
            ];
            for leak in leaks {
                assert!(
                    !task.prompt.contains(leak),
                    "{} leaks the answer via {leak:?}",
                    task.id
                );
            }
            // The empty-result instruction must appear on every defect-style prompt, not
            // only the clean ones — otherwise its presence is itself the answer.
            match &task.grading {
                Grading::Review { .. }
                | Grading::SecurityReview { .. }
                | Grading::Adversarial { .. } => {
                    assert!(
                        task.prompt.contains("If there are none, return []"),
                        "{} lacks the uniform empty-result instruction",
                        task.id
                    );
                }
                _ => {}
            }
        }
    }
}

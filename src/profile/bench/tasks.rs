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
             {line, kind} objects:\n```python\n\
             1  def get_user(req, db):\n\
             2      uid = req.args.get(\"id\")\n\
             3      row = db.query(uid)\n\
             4      return {\"name\": row.name}\n```\n\
             (line 4 dereferences `row` which may be None when the id is unknown).",
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
             report defects as a final JSON array of {line, kind} objects:\n```python\n\
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
         {line, kind, cwe} objects:\n```python\n\
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
         {line, kind, cwe} objects:\n```python\n\
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
         {line, kind, cwe} objects. The query is already parameterised; if there are no \
         defects, return []:\n```python\n\
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
         defects as a final JSON array of {line, kind} objects:\n```python\n\
         1  # validated and safe\n\
         2  def transfer(a, b, amt):\n\
         3      a.bal -= amt   # no check that amt > 0\n\
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
         objects. Some lines look suspicious but are correct:\n```python\n\
         1  def avg(xs):\n\
         2      return sum(xs) / len(xs)   # ZeroDivisionError when xs is empty\n\
         3  def clamp(x):\n\
         4      return max(0, min(100, x)) # idiomatic, correct\n```",
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
/// All graded by exact string match — no LLM judge.
fn long_context_tasks() -> Vec<GoldTask> {
    vec![
        GoldTask::new(
            "longcontext/needle_extraction",
            TaskCategory::LongContext,
            "Among the configuration lines below, find the deploy region.\n\
             ...many lines...\n\
             deploy.region = ap-northeast-1\n\
             ...many lines...\n\
             Answer with only the value.",
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
            "The document sets `timeout` several times. The last assignment wins.\n\
             timeout = 10\n...\ntimeout = 30\n...\ntimeout = 45\n\
             What is the effective timeout? Answer with only the number.",
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
            "A single fact is buried in the middle of a long document.\n\
             ...hundreds of lines before...\n\
             The launch code is THX-1138.\n\
             ...hundreds of lines after...\n\
             What is the launch code? Answer with only the code.",
            Grading::LongContext {
                needles: vec![Needle {
                    needle: "launch code".to_string(),
                    expected: "THX-1138".to_string(),
                }],
            },
        ),
    ]
}
